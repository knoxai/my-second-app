use std::borrow::Cow;
use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use api::rest::OrderByInterface;
use futures::future::BoxFuture;
use futures::FutureExt;
use segment::common::reciprocal_rank_fusion::rrf_scoring;
use segment::types::{
    Filter, HasIdCondition, PointIdType, ScoredPoint, WithPayload, WithPayloadInterface, WithVector,
};
use tokio::runtime::Handle;

use super::LocalShard;
use crate::collection_manager::segments_searcher::SegmentsSearcher;
use crate::operations::types::{
    CollectionError, CollectionResult, CoreSearchRequest, CoreSearchRequestBatch,
    ScrollRequestInternal,
};
use crate::operations::universal_query::planned_query::{
    MergePlan, PlannedQuery, PrefetchSource, ResultsMerge,
};
use crate::operations::universal_query::shard_query::{Fusion, ScoringQuery, ShardQueryResponse};

struct PrefetchHolder {
    core_results: Vec<Vec<ScoredPoint>>,
    scrolls: Vec<Vec<ScoredPoint>>,
}

impl PrefetchHolder {
    fn new(core_results: Vec<Vec<ScoredPoint>>, scrolls: Vec<Vec<ScoredPoint>>) -> Self {
        Self {
            core_results,
            scrolls,
        }
    }

    /// Returns an iterator by Cow over the sources in no specific order.
    fn iter_sources(
        &self,
        core_indices: Vec<usize>,
        scroll_indices: Vec<usize>,
        merged_list: Vec<Vec<ScoredPoint>>,
    ) -> PrefetchIterator {
        PrefetchIterator {
            prefetch_holder: self,
            core_indices,
            scroll_indices,
            merged_list,
        }
    }
}

struct PrefetchIterator<'a> {
    prefetch_holder: &'a PrefetchHolder,
    core_indices: Vec<usize>,
    scroll_indices: Vec<usize>,
    merged_list: Vec<Vec<ScoredPoint>>,
}

impl<'a> Iterator for PrefetchIterator<'a> {
    type Item = Cow<'a, Vec<ScoredPoint>>;

    fn next(&mut self) -> Option<Self::Item> {
        if let Some(idx) = self.core_indices.pop() {
            return self
                .prefetch_holder
                .core_results
                .get(idx)
                .map(Cow::Borrowed);
        }
        if let Some(idx) = self.scroll_indices.pop() {
            return self.prefetch_holder.scrolls.get(idx).map(Cow::Borrowed);
        }
        self.merged_list.pop().map(Cow::Owned)
    }
}

impl LocalShard {
    #[allow(unreachable_code, clippy::diverging_sub_expression, unused_variables)]
    pub async fn do_planned_query(
        &self,
        request: PlannedQuery,
        search_runtime_handle: &Handle,
        timeout: Option<Duration>,
    ) -> CollectionResult<ShardQueryResponse> {
        let core_results = self
            .do_search(request.searches, search_runtime_handle, timeout)
            .await?;

        let scrolls = self
            .query_scroll_batch(request.scrolls, search_runtime_handle)
            .await?;

        let prefetch_holder = PrefetchHolder::new(core_results, scrolls);

        let mut scored_points = self
            .recurse_prefetch(
                request.merge_plan,
                &prefetch_holder,
                search_runtime_handle,
                timeout,
                0, // initial depth
            )
            .await?;

        // fetch payload and/or vector for scored points if necessary
        if request.with_payload.is_required() || request.with_vector.is_enabled() {
            // ids to retrieve (deduplication happens in the searcher)
            let point_ids = scored_points
                .iter()
                .flatten()
                .map(|scored_point| scored_point.id)
                .collect::<Vec<PointIdType>>();

            // it might make sense to change this approach to fetch payload and vector at the collection level
            // after the shard results merge, but it requires careful benchmarking
            let mut records = SegmentsSearcher::retrieve(
                self.segments(),
                &point_ids,
                &WithPayload::from(&request.with_payload),
                &request.with_vector,
            )?;

            // update scored points in place
            for (scored_point, record) in scored_points.iter_mut().flatten().zip(records.iter_mut())
            {
                scored_point.payload = record.payload.take();
                scored_point.vector = record.vector.take();
            }
        }

        Ok(scored_points)
    }

    fn recurse_prefetch<'shard, 'query>(
        &'shard self,
        merge_plan: MergePlan,
        prefetch_holder: &'query PrefetchHolder,
        search_runtime_handle: &'shard Handle,
        timeout: Option<Duration>,
        depth: usize,
    ) -> BoxFuture<'query, CollectionResult<Vec<Vec<ScoredPoint>>>>
    where
        'shard: 'query,
    {
        async move {
            let max_len = merge_plan.sources.len();
            let mut search_indices = Vec::with_capacity(max_len);
            let mut scroll_indices = Vec::with_capacity(max_len);
            let mut merged_list = Vec::with_capacity(max_len);

            for source in merge_plan.sources.into_iter() {
                match source {
                    PrefetchSource::SearchesIdx(idx) => search_indices.push(idx),
                    PrefetchSource::ScrollsIdx(idx) => scroll_indices.push(idx),
                    PrefetchSource::Prefetch(prefetch) => {
                        let merged = self
                            .recurse_prefetch(
                                prefetch,
                                prefetch_holder,
                                search_runtime_handle,
                                timeout,
                                depth + 1,
                            )
                            .await?;
                        merged_list.extend(merged);
                    }
                }
            }

            let sources = prefetch_holder.iter_sources(search_indices, scroll_indices, merged_list);

            let root_query_needs_intermediate_results = || {
                merge_plan
                    .merge
                    .as_ref()
                    .map(|plan| plan.rescore.needs_intermediate_results())
                    .unwrap_or(false)
            };

            if depth == 0 && root_query_needs_intermediate_results() {
                // in case of top level RRF, we need to propagate intermediate results
                Ok(sources.map(Cow::into_owned).collect())
            } else {
                let merged = self
                    .merge_prefetches(sources, merge_plan.merge, search_runtime_handle, timeout)
                    .await?;
                Ok(vec![merged])
            }
        }
        .boxed()
    }

    /// Rescore list of scored points
    #[allow(clippy::too_many_arguments)]
    async fn rescore<'a>(
        &self,
        sources: impl Iterator<Item = Cow<'a, Vec<ScoredPoint>>>,
        merge: ResultsMerge,
        search_runtime_handle: &Handle,
        timeout: Option<Duration>,
    ) -> CollectionResult<Vec<ScoredPoint>> {
        let ResultsMerge {
            rescore,
            filter,
            score_threshold,
            limit,
        } = merge;

        match rescore {
            ScoringQuery::Fusion(Fusion::Rrf) => {
                let sources: Vec<_> = sources.map(Cow::into_owned).collect();

                // TODO(universal-query): Remove this ugly part when we propagate merged filters to leaf queries
                let valid_ids = if let Some(filter) = filter {
                    let filter =
                        filter_with_sources_ids(sources.iter().map(Cow::Borrowed), Some(filter));
                    Some(self.read_filtered(Some(&filter))?)
                } else {
                    None
                };

                let mut top_rrf = rrf_scoring(sources);

                top_rrf = top_rrf
                    .into_iter()
                    .filter(|point| {
                        // TODO(universal-query): Remove this ugly part when we propagate merged filters to leaf queries
                        valid_ids
                            .as_ref()
                            .map(|valid_ids| valid_ids.contains(&point.id))
                            .unwrap_or(true)
                    })
                    .take_while(|point| {
                        // TODO(universal-query): Refactor this ugly part when we propagate merged filters to leaf queries
                        score_threshold
                            .map(|threshold| point.score >= threshold)
                            .unwrap_or(true)
                    })
                    .take(limit)
                    .collect();

                Ok(top_rrf)
            }
            ScoringQuery::OrderBy(order_by) => {
                // create single scroll request for rescoring query
                let filter = filter_with_sources_ids(sources, filter);

                let scroll_request = ScrollRequestInternal {
                    offset: None,
                    limit: Some(limit),
                    filter: Some(filter),
                    with_payload: Some(WithPayloadInterface::Bool(false)),
                    with_vector: WithVector::Bool(false),
                    order_by: Some(OrderByInterface::Struct(order_by)),
                };

                self.query_scroll_batch(Arc::new(vec![scroll_request]), search_runtime_handle)
                    .await?
                    .pop()
                    .ok_or_else(|| {
                        CollectionError::service_error(
                            "Rescoring with order-by query didn't return expected batch of results",
                        )
                    })
            }
            ScoringQuery::Vector(query_enum) => {
                // create single search request for rescoring query
                let filter = filter_with_sources_ids(sources, filter);

                let search_request = CoreSearchRequest {
                    query: query_enum,
                    filter: Some(filter),
                    params: None,
                    limit,
                    offset: 0,
                    with_payload: None, // the payload is fetched separately
                    with_vector: None,  // the vector is fetched separately
                    score_threshold,
                };

                let rescoring_core_search_request = CoreSearchRequestBatch {
                    searches: vec![search_request],
                };

                self.do_search(
                    Arc::new(rescoring_core_search_request),
                    search_runtime_handle,
                    timeout,
                )
                .await?
                // One search request is sent. We expect only one result
                .pop()
                .ok_or_else(|| {
                    CollectionError::service_error(
                        "Rescoring with vector(s) query didn't return expected batch of results",
                    )
                })
            }
        }
    }

    /// Merge multiple prefetches into a single result up to the limit.
    /// Rescores if required.
    async fn merge_prefetches<'a>(
        &self,
        mut sources: impl Iterator<Item = Cow<'a, Vec<ScoredPoint>>>,
        merge: Option<ResultsMerge>,
        search_runtime_handle: &Handle,
        timeout: Option<Duration>,
    ) -> CollectionResult<Vec<ScoredPoint>> {
        if let Some(results_merge) = merge {
            self.rescore(sources, results_merge, search_runtime_handle, timeout)
                .await
        } else {
            // The whole query request has no prefetches, and everything comes directly from a single source
            let top = sources
                .next()
                .ok_or_else(|| {
                    CollectionError::service_error("No sources to merge in the query request")
                })?
                .into_owned();

            debug_assert!(sources.next().is_none());

            Ok(top)
        }
    }
}

/// Extracts point ids from sources, creates a filter and merges it with the provided filter.
fn filter_with_sources_ids<'a>(
    sources: impl Iterator<Item = Cow<'a, Vec<ScoredPoint>>>,
    filter: Option<Filter>,
) -> Filter {
    let mut point_ids = HashSet::new();

    for source in sources {
        for point in source.iter() {
            point_ids.insert(point.id);
        }
    }

    // create filter for target point ids
    let ids_filter = Filter::new_must(segment::types::Condition::HasId(HasIdCondition::from(
        point_ids,
    )));

    filter.unwrap_or_default().merge_owned(ids_filter)
}
