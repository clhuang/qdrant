use std::sync::{Arc, RwLock, RwLockWriteGuard};
use crate::segment_manager::segment_holder::{SegmentHolder, LockedSegment};
use crate::segment_manager::segment_managers::SegmentUpdater;
use crate::operations::CollectionUpdateOperations;
use crate::collection::{OperationResult, UpdateError};
use segment::types::{SeqNumberType, PointIdType, PayloadKeyType};
use segment::entry::entry_point::{OperationError, SegmentEntry, Result};
use std::collections::{HashSet, HashMap};
use crate::operations::types::VectorType;
use rand::Rng;
use crate::operations::point_ops::PointOps;
use crate::operations::payload_ops::{PayloadOps, PayloadInterface, PayloadVariant};

struct SimpleSegmentUpdater {
    segments: Arc<RwLock<SegmentHolder>>,
}


impl SimpleSegmentUpdater {
    fn check_unprocessed_points(points: &Vec<PointIdType>, processed: &HashSet<PointIdType>) -> OperationResult<usize> {
        let missed_point = points
            .iter()
            .cloned()
            .filter(|p| !processed.contains(p))
            .next();
        match missed_point {
            None => Ok(processed.len()),
            Some(missed_point) => Err(UpdateError::NotFound { missed_point_id: missed_point }),
        }
    }

    /// Tries to delete points from all segments, returns number of actually deleted points
    fn delete_points(&self, op_num: SeqNumberType, ids: &Vec<PointIdType>) -> OperationResult<usize> {
        self.segments.read().unwrap()
            .apply_points(op_num, ids, |id, write_segment|
                write_segment.delete_point(op_num, id),
            )
    }


    /// Checks point id in each segment, update point if found.
    /// All not found points are inserted into random segment.
    /// Returns: number of updated points.
    fn upsert_points(&self, op_num: SeqNumberType, ids: &Vec<PointIdType>, vectors: &Vec<VectorType>) -> OperationResult<usize> {
        let mut updated_points: HashSet<PointIdType> = Default::default();
        let points_map: HashMap<PointIdType, &VectorType> = ids.iter().cloned().zip(vectors).collect();

        let segments = self.segments.read().unwrap();

        let res = segments.apply_points(op_num, ids, |id, write_segment| {
            updated_points.insert(id);
            write_segment.upsert_point(op_num, id, points_map[&id])
        })?;

        let new_point_ids = ids
            .iter()
            .cloned()
            .filter(|x| !updated_points.contains(x));

        let write_segment = segments.random_segment();
        return match write_segment {
            None => Err(UpdateError::ServiceError { error: "No segments exists, expected at least one".to_string() }),
            Some(segment) => {
                let mut write_segment = segment.write().unwrap();
                for point_id in new_point_ids {
                    write_segment.upsert_point(op_num, point_id, points_map[&point_id]);
                }
                Ok(res)
            }
        };
    }

    fn set_payload(
        &self,
        op_num: SeqNumberType,
        payload: &HashMap<PayloadKeyType, PayloadInterface>,
        points: &Vec<PointIdType>,
    ) -> OperationResult<usize> {
        let mut updated_points: HashSet<PointIdType> = Default::default();

        let res = self.segments.read().unwrap().apply_points(op_num, points, |id, write_segment| {
            updated_points.insert(id);
            let mut res = true;
            for (key, payload) in payload {
                res = write_segment.set_payload(op_num, id, key, payload.to_payload())? && res;
            }
            Ok(res)
        })?;

        SimpleSegmentUpdater::check_unprocessed_points(points, &updated_points)?;
        Ok(res)
    }

    fn delete_payload(
        &self,
        op_num: SeqNumberType,
        points: &Vec<PointIdType>,
        keys: &Vec<PayloadKeyType>,
    ) -> OperationResult<usize> {
        let mut updated_points: HashSet<PointIdType> = Default::default();
        let res = self.segments
            .read().unwrap()
            .apply_points(op_num, points, |id, write_segment| {
                updated_points.insert(id);
                let mut res = true;
                for key in keys {
                    res = write_segment.delete_payload(op_num, id, key)? && res;
                }
                Ok(res)
            })?;

        SimpleSegmentUpdater::check_unprocessed_points(points, &updated_points)?;
        Ok(res)
    }

    fn clear_payload(
        &self,
        op_num: SeqNumberType,
        points: &Vec<PointIdType>,
    ) -> OperationResult<usize> {
        let mut updated_points: HashSet<PointIdType> = Default::default();
        let res = self.segments
            .read().unwrap()
            .apply_points(op_num, points, |id, write_segment| {
                updated_points.insert(id);
                write_segment.clear_payload(op_num, id)
            })?;

        SimpleSegmentUpdater::check_unprocessed_points(points, &updated_points)?;
        Ok(res)
    }

    fn wipe_payload(
        &self,
        op_num: SeqNumberType,
    ) -> OperationResult<usize> {
        self.segments.read().unwrap().apply_segments(op_num, |segment| segment.wipe_payload(op_num))
    }

    pub fn process_point_operation(&self, op_num: SeqNumberType, point_operation: &PointOps) -> OperationResult<usize> {
        match point_operation {
            PointOps::UpsertPoints {
                ids,
                vectors,
                ..
            } => self.upsert_points(op_num, ids, vectors),
            PointOps::DeletePoints { ids, .. } => self.delete_points(op_num, ids),
        }
    }


    pub fn process_payload_operation(&self, op_num: SeqNumberType, payload_operation: &PayloadOps) -> OperationResult<usize> {
        match payload_operation {
            PayloadOps::SetPayload {
                payload,
                points,
                ..
            } => self.set_payload(op_num, payload, points),
            PayloadOps::DeletePayload {
                keys,
                points,
                ..
            } => self.delete_payload(op_num, points, keys),
            PayloadOps::ClearPayload {
                points, ..
            } => self.clear_payload(op_num, points),
            PayloadOps::WipePayload { .. } => self.wipe_payload(op_num),
        }
    }
}


impl SegmentUpdater for SimpleSegmentUpdater {
    fn update(&self, op_num: SeqNumberType, operation: &CollectionUpdateOperations) -> OperationResult<usize> {
        match operation {
            CollectionUpdateOperations::PointOperation(point_operation) => self.process_point_operation(op_num, point_operation),
            CollectionUpdateOperations::PayloadOperation(payload_operation) => self.process_payload_operation(op_num, payload_operation),
        }
    }
}


#[cfg(test)]
mod tests {
    use super::*;
    use crate::segment_manager::fixtures::{build_test_holder, build_searcher};
    use crate::segment_manager::segment_managers::SegmentSearcher;

    #[test]
    fn test_point_ops() {
        let searcher = build_searcher();

        let updater = SimpleSegmentUpdater {
            segments: searcher.segments.clone()
        };
        let points = vec![1, 500];

        let vectors = vec![
            vec![2., 2., 2., 2.],
            vec![2., 0., 2., 0.],
        ];

        let res = updater.upsert_points(
            100,
            &points,
            &vectors,
        );

        match res {
            Ok(updated) => assert_eq!(updated, 1),
            Err(_) => assert!(false),
        };

        let records = searcher.retrieve(&vec![1, 2, 500], true, true);

        assert_eq!(records.len(), 3);

        for record in records {
            let v = record.vector.unwrap();

            if record.id == 1 {
                assert_eq!(&v, &vec![2., 2., 2., 2.])
            }
            if record.id == 500 {
                assert_eq!(&v, &vec![2., 0., 2., 0.])
            }
        }

        updater.delete_points(101, &vec![500]);

        let records = searcher.retrieve(&vec![1, 2, 500], true, true);

        for record in records {
            let v = record.vector.unwrap();

            if record.id == 500 {
                assert!(false)
            }
        }
    }

    #[test]
    fn test_payload_ops() {
        let searcher = build_searcher();

        let updater = SimpleSegmentUpdater {
            segments: searcher.segments.clone()
        };

        let mut payload: HashMap<PayloadKeyType, PayloadInterface> = Default::default();

        payload.insert(
            "color".to_string(),
            PayloadInterface::Keyword(PayloadVariant::Value("red".to_string())),
        );

        let points = vec![1, 2, 3];

        updater.process_payload_operation(100, &PayloadOps::SetPayload {
            collection: "".to_string(),
            payload,
            points: points.clone(),
        });

        let res = searcher.retrieve(&points, true, false);

        assert_eq!(res.len(), 3);

        match res.get(0) {
            None => assert!(false),
            Some(r) => match &r.payload {
                None => assert!(false, "No payload assigned"),
                Some(payload) => {
                    assert!(payload.contains_key("color"))
                }
            },
        };

        /// Test payload delete

        updater.delete_payload(101, &vec![3], &vec!["color".to_string(), "empty".to_string()]);
        let res = searcher.retrieve(&vec![3], true, false);
        assert_eq!(res.len(), 1);
        assert!(!res[0].payload.as_ref().unwrap().contains_key("color"));

        /// Test clear payload

        let res = searcher.retrieve(&vec![2], true, false);
        assert_eq!(res.len(), 1);
        assert!(res[0].payload.as_ref().unwrap().contains_key("color"));

        updater.clear_payload(102, &vec![2]);
        let res = searcher.retrieve(&vec![2], true, false);
        assert_eq!(res.len(), 1);
        assert!(!res[0].payload.as_ref().unwrap().contains_key("color"))
    }
}