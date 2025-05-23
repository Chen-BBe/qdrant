use std::sync::atomic::{AtomicBool, Ordering};

use bitvec::prelude::BitSlice;
use common::counter::hardware_counter::HardwareCounterCell;
use common::ext::BitSliceExt as _;
use common::fixed_length_priority_queue::FixedLengthPriorityQueue;
use common::types::{PointOffsetType, ScoreType, ScoredPointOffset};

use super::query::{
    ContextQuery, DiscoveryQuery, RecoBestScoreQuery, RecoQuery, RecoSumScoresQuery, TransformInto,
};
use super::query_scorer::custom_query_scorer::CustomQueryScorer;
use crate::common::operation_error::{CancellableResult, OperationError, OperationResult};
use crate::data_types::vectors::{DenseVector, QueryVector, VectorElementType, VectorInternal};
use crate::spaces::metric::Metric;
use crate::spaces::simple::{CosineMetric, DotProductMetric, EuclidMetric, ManhattanMetric};
use crate::types::Distance;
use crate::vector_storage::dense::memmap_dense_vector_storage::MemmapDenseVectorStorage;
use crate::vector_storage::dense::mmap_dense_vectors::MmapDenseVectors;
use crate::vector_storage::query_scorer::QueryScorer;
use crate::vector_storage::query_scorer::metric_query_scorer::MetricQueryScorer;
use crate::vector_storage::{RawScorer, VectorStorage as _};

pub fn new<'a>(
    query: QueryVector,
    storage: &'a MemmapDenseVectorStorage<VectorElementType>,
    point_deleted: &'a BitSlice,
    hardware_counter: HardwareCounterCell,
) -> OperationResult<Box<dyn RawScorer + 'a>> {
    AsyncRawScorerBuilder::new(query, storage, point_deleted, hardware_counter).build()
}

pub struct AsyncRawScorerImpl<'a, TQueryScorer: QueryScorer<[VectorElementType]>> {
    points_count: PointOffsetType,
    query_scorer: TQueryScorer,
    storage: &'a MmapDenseVectors<VectorElementType>,
    point_deleted: &'a BitSlice,
    vec_deleted: &'a BitSlice,
}

impl<'a, TQueryScorer> AsyncRawScorerImpl<'a, TQueryScorer>
where
    TQueryScorer: QueryScorer<[VectorElementType]>,
{
    fn new(
        points_count: PointOffsetType,
        query_scorer: TQueryScorer,
        storage: &'a MmapDenseVectors<VectorElementType>,
        point_deleted: &'a BitSlice,
        vec_deleted: &'a BitSlice,
    ) -> Self {
        Self {
            points_count,
            query_scorer,
            storage,
            point_deleted,
            vec_deleted,
        }
    }
}

impl<TQueryScorer> RawScorer for AsyncRawScorerImpl<'_, TQueryScorer>
where
    TQueryScorer: QueryScorer<[VectorElementType]>,
{
    fn score_points(&self, points: &[PointOffsetType], scores: &mut [ScoredPointOffset]) -> usize {
        let points_stream = points
            .iter()
            .copied()
            .filter(|point_id| self.check_vector(*point_id));

        let mut processed = 0;
        self.storage
            .read_vectors_async(points_stream, |idx, point_id, other_vector| {
                scores[idx] = ScoredPointOffset {
                    idx: point_id,
                    score: self.query_scorer.score(other_vector),
                };
                processed += 1;
            })
            .unwrap();

        // ToDo: io_uring is experimental, it can fail if it is not supported.
        // Instead of silently falling back to the sync implementation, we prefer to panic
        // and notify the user that they better use the default IO implementation.

        processed
    }

    fn score_points_unfiltered(
        &self,
        points: &mut dyn Iterator<Item = PointOffsetType>,
    ) -> Vec<ScoredPointOffset> {
        let mut scores = vec![];

        self.storage
            .read_vectors_async(points, |_idx, point_id, other_vector| {
                scores.push(ScoredPointOffset {
                    idx: point_id,
                    score: self.query_scorer.score(other_vector),
                });
            })
            .unwrap();

        // ToDo: io_uring is experimental, it can fail if it is not supported.
        // Instead of silently falling back to the sync implementation, we prefer to panic
        // and notify the user that they better use the default IO implementation.

        scores
    }

    fn check_vector(&self, point: PointOffsetType) -> bool {
        point < self.points_count
            // Deleted points propagate to vectors; check vector deletion for possible early return
            // Default to not deleted if our deleted flags failed grow
            && !self.vec_deleted.get_bit(point as usize).unwrap_or(false)
            // Additionally check point deletion for integrity if delete propagation to vector failed
            // Default to deleted if the point mapping was removed from the ID tracker
            && !self.point_deleted.get_bit(point as usize).unwrap_or(true)
    }

    fn score_point(&self, point: PointOffsetType) -> ScoreType {
        self.query_scorer.score_stored(point)
    }

    fn score_internal(&self, point_a: PointOffsetType, point_b: PointOffsetType) -> ScoreType {
        self.query_scorer.score_internal(point_a, point_b)
    }

    fn peek_top_iter(
        &self,
        points: &mut dyn Iterator<Item = PointOffsetType>,
        top: usize,
        is_stopped: &AtomicBool,
    ) -> CancellableResult<Vec<ScoredPointOffset>> {
        if top == 0 {
            return Ok(vec![]);
        }

        let mut pq = FixedLengthPriorityQueue::new(top);
        let points_stream = points
            .take_while(|_| !is_stopped.load(Ordering::Relaxed))
            .filter(|point_id| self.check_vector(*point_id));

        self.storage
            .read_vectors_async(points_stream, |_, point_id, other_vector| {
                let scored_point_offset = ScoredPointOffset {
                    idx: point_id,
                    score: self.query_scorer.score(other_vector),
                };
                pq.push(scored_point_offset);
            })
            .unwrap();

        // ToDo: io_uring is experimental, it can fail if it is not supported.
        // Instead of silently falling back to the sync implementation, we prefer to panic
        // and notify the user that they better use the default IO implementation.

        Ok(pq.into_sorted_vec())
    }

    fn peek_top_all(
        &self,
        top: usize,
        is_stopped: &AtomicBool,
    ) -> CancellableResult<Vec<ScoredPointOffset>> {
        if top == 0 {
            return Ok(vec![]);
        }

        let points_stream = (0..self.points_count)
            .take_while(|_| !is_stopped.load(Ordering::Relaxed))
            .filter(|point_id| self.check_vector(*point_id));

        let mut pq = FixedLengthPriorityQueue::new(top);
        self.storage
            .read_vectors_async(points_stream, |_, point_id, other_vector| {
                let scored_point_offset = ScoredPointOffset {
                    idx: point_id,
                    score: self.query_scorer.score(other_vector),
                };
                pq.push(scored_point_offset);
            })
            .unwrap();

        // ToDo: io_uring is experimental, it can fail if it is not supported.
        // Instead of silently falling back to the sync implementation, we prefer to panic
        // and notify the user that they better use the default IO implementation.

        Ok(pq.into_sorted_vec())
    }
}

struct AsyncRawScorerBuilder<'a> {
    points_count: PointOffsetType,
    query: QueryVector,
    storage: &'a MemmapDenseVectorStorage<VectorElementType>,
    point_deleted: &'a BitSlice,
    vec_deleted: &'a BitSlice,
    distance: Distance,
    hardware_counter: HardwareCounterCell,
}

impl<'a> AsyncRawScorerBuilder<'a> {
    pub fn new(
        query: QueryVector,
        storage: &'a MemmapDenseVectorStorage<VectorElementType>,
        point_deleted: &'a BitSlice,
        hardware_counter: HardwareCounterCell,
    ) -> Self {
        let points_count = storage.total_vector_count() as _;
        let vec_deleted = storage.deleted_vector_bitslice();

        let distance = storage.distance();

        Self {
            points_count,
            query,
            storage,
            point_deleted,
            vec_deleted,
            distance,
            hardware_counter,
        }
    }

    pub fn build(self) -> OperationResult<Box<dyn RawScorer + 'a>> {
        match self.distance {
            Distance::Cosine => self._build_with_metric::<CosineMetric>(),
            Distance::Euclid => self._build_with_metric::<EuclidMetric>(),
            Distance::Dot => self._build_with_metric::<DotProductMetric>(),
            Distance::Manhattan => self._build_with_metric::<ManhattanMetric>(),
        }
    }

    fn _build_with_metric<TMetric: Metric<VectorElementType> + 'a>(
        self,
    ) -> OperationResult<Box<dyn RawScorer + 'a>> {
        let Self {
            points_count,
            query,
            storage,
            point_deleted,
            vec_deleted,
            distance: _,
            hardware_counter,
        } = self;

        match query {
            QueryVector::Nearest(vector) => {
                match vector {
                    VectorInternal::Dense(dense_vector) => {
                        let query_scorer = MetricQueryScorer::<VectorElementType, TMetric, _>::new(
                            dense_vector,
                            storage,
                            hardware_counter,
                        );
                        Ok(Box::new(AsyncRawScorerImpl::new(
                            points_count,
                            query_scorer,
                            storage.get_mmap_vectors(),
                            point_deleted,
                            vec_deleted,
                        )))
                    }
                    VectorInternal::Sparse(_sparse_vector) => Err(OperationError::service_error(
                        "sparse vectors are not supported for async scorer",
                    )), // TODO(sparse) add support?
                    VectorInternal::MultiDense(_multi_dense_vector) => {
                        Err(OperationError::service_error(
                            "multi-dense vectors are not supported for async scorer",
                        ))
                    } // TODO(colbert) add support?
                }
            }
            QueryVector::RecommendBestScore(reco_query) => {
                let reco_query: RecoQuery<DenseVector> = reco_query.transform_into()?;
                let query_scorer = CustomQueryScorer::<VectorElementType, TMetric, _, _, _>::new(
                    RecoBestScoreQuery::from(reco_query),
                    storage,
                    hardware_counter,
                );
                Ok(Box::new(AsyncRawScorerImpl::new(
                    points_count,
                    query_scorer,
                    storage.get_mmap_vectors(),
                    point_deleted,
                    vec_deleted,
                )))
            }
            QueryVector::RecommendSumScores(reco_query) => {
                let reco_query: RecoQuery<DenseVector> = reco_query.transform_into()?;
                let query_scorer = CustomQueryScorer::<VectorElementType, TMetric, _, _, _>::new(
                    RecoSumScoresQuery::from(reco_query),
                    storage,
                    hardware_counter,
                );
                Ok(Box::new(AsyncRawScorerImpl::new(
                    points_count,
                    query_scorer,
                    storage.get_mmap_vectors(),
                    point_deleted,
                    vec_deleted,
                )))
            }
            QueryVector::Discovery(discovery_query) => {
                let discovery_query: DiscoveryQuery<DenseVector> =
                    discovery_query.transform_into()?;
                let query_scorer = CustomQueryScorer::<VectorElementType, TMetric, _, _, _>::new(
                    discovery_query,
                    storage,
                    hardware_counter,
                );
                Ok(Box::new(AsyncRawScorerImpl::new(
                    points_count,
                    query_scorer,
                    storage.get_mmap_vectors(),
                    point_deleted,
                    vec_deleted,
                )))
            }
            QueryVector::Context(context_query) => {
                let context_query: ContextQuery<DenseVector> = context_query.transform_into()?;
                let query_scorer = CustomQueryScorer::<VectorElementType, TMetric, _, _, _>::new(
                    context_query,
                    storage,
                    hardware_counter,
                );
                Ok(Box::new(AsyncRawScorerImpl::new(
                    points_count,
                    query_scorer,
                    storage.get_mmap_vectors(),
                    point_deleted,
                    vec_deleted,
                )))
            }
        }
    }
}
