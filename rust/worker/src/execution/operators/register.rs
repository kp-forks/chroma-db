use async_trait::async_trait;
use chroma_error::{ChromaError, ErrorCodes};
use chroma_log::Log;
use chroma_sysdb::FlushCompactionError;
use chroma_sysdb::SysDb;
use chroma_system::Operator;
use chroma_types::{CollectionUuid, FlushCompactionResponse, SegmentFlushInfo};
use std::sync::Arc;
use thiserror::Error;

/// The register  operator is responsible for flushing compaction data to the sysdb
/// as well as updating the log offset in the log service.
#[derive(Debug)]
pub struct RegisterOperator {}

impl RegisterOperator {
    /// Create a new flush sysdb operator.
    pub fn new() -> Box<Self> {
        Box::new(RegisterOperator {})
    }
}

#[derive(Debug)]
/// The input for the flush sysdb operator.
/// This input is used to flush compaction data to the sysdb as well as update the log offset in the log service.
/// # Parameters
/// * `tenant` - The tenant id.
/// * `collection_id` - The collection id.
/// * `log_position` - The log position. Note that this is the log position for the last record that
///   was flushed to S3.
/// * `collection_version` - The collection version. This is the current collection version before
///   the flush operation. This version will be incremented by 1 after the flush operation. If the
///   collection version in sysdb is not the same as the current collection version, the flush
///   operation will fail.
/// * `segment_flush_info` - The segment flush info.
/// * `total_records_post_compaction` - The total number of records in the collection post compaction.
/// * `sysdb` - The sysdb client.
/// * `log` - The log client.
pub struct RegisterInput {
    tenant: String,
    collection_id: CollectionUuid,
    log_position: i64,
    collection_version: i32,
    segment_flush_info: Arc<[SegmentFlushInfo]>,
    total_records_post_compaction: u64,
    collection_logical_size_bytes: u64,
    sysdb: SysDb,
    log: Log,
}

impl RegisterInput {
    #[allow(clippy::too_many_arguments)]
    /// Create a new flush sysdb input.
    pub fn new(
        tenant: String,
        collection_id: CollectionUuid,
        log_position: i64,
        collection_version: i32,
        segment_flush_info: Arc<[SegmentFlushInfo]>,
        total_records_post_compaction: u64,
        collection_logical_size_bytes: u64,
        sysdb: SysDb,
        log: Log,
    ) -> Self {
        RegisterInput {
            tenant,
            collection_id,
            log_position,
            collection_version,
            segment_flush_info,
            total_records_post_compaction,
            collection_logical_size_bytes,
            sysdb,
            log,
        }
    }
}

/// The output for the flush sysdb operator.
/// # Parameters
/// * `result` - The result of the flush compaction operation.
#[derive(Debug)]
pub struct RegisterOutput {
    _sysdb_registration_result: FlushCompactionResponse,
}

#[derive(Error, Debug)]
pub enum RegisterError {
    #[error("Flush compaction error: {0}")]
    FlushCompactionError(#[from] FlushCompactionError),
    #[error("Update log offset error: {0}")]
    UpdateLogOffsetError(#[from] Box<dyn ChromaError>),
}

impl ChromaError for RegisterError {
    fn code(&self) -> ErrorCodes {
        match self {
            RegisterError::FlushCompactionError(e) => e.code(),
            RegisterError::UpdateLogOffsetError(e) => e.code(),
        }
    }
}

#[async_trait]
impl Operator<RegisterInput, RegisterOutput> for RegisterOperator {
    type Error = RegisterError;

    fn get_name(&self) -> &'static str {
        "RegisterOperator"
    }

    async fn run(&self, input: &RegisterInput) -> Result<RegisterOutput, RegisterError> {
        let mut sysdb = input.sysdb.clone();
        let mut log = input.log.clone();
        let result = sysdb
            .flush_compaction(
                input.tenant.clone(),
                input.collection_id,
                input.log_position,
                input.collection_version,
                input.segment_flush_info.clone(),
                input.total_records_post_compaction,
                input.collection_logical_size_bytes,
            )
            .await;

        // We must make sure that the log postion in sysdb is always greater than or equal to the log position
        // in the log service. If the log position in sysdb is less than the log position in the log service,
        // the we may lose data in compaction.
        let sysdb_registration_result = match result {
            Ok(response) => response,
            Err(error) => return Err(RegisterError::FlushCompactionError(error)),
        };

        let result = log
            .update_collection_log_offset(&input.tenant, input.collection_id, input.log_position)
            .await;

        match result {
            Ok(_) => Ok(RegisterOutput {
                _sysdb_registration_result: sysdb_registration_result,
            }),
            Err(error) => Err(RegisterError::UpdateLogOffsetError(error)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chroma_log::in_memory_log::InMemoryLog;
    use chroma_sysdb::{GetCollectionsOptions, TestSysDb};
    use chroma_types::{Collection, Segment, SegmentScope, SegmentType, SegmentUuid};
    use std::collections::HashMap;
    use std::str::FromStr;

    #[tokio::test]
    async fn test_register_operator() {
        let mut sysdb = SysDb::Test(TestSysDb::new());
        let log = Log::InMemory(InMemoryLog::new());
        let total_records_post_compaction: u64 = 5;
        let size_bytes_post_compaction: u64 = 25000;
        let last_compaction_time_secs: u64 = 1741037006;
        let collection_version = 0;

        let tenant_1 = "tenant_1".to_string();
        let collection_1 = Collection {
            name: "collection_1".to_string(),
            dimension: Some(1),
            tenant: tenant_1.clone(),
            database: "database_1".to_string(),
            total_records_post_compaction,
            size_bytes_post_compaction,
            last_compaction_time_secs,
            ..Default::default()
        };
        let collection_uuid_1 = collection_1.collection_id;

        let tenant_2 = "tenant_2".to_string();
        let collection_2 = Collection {
            name: "collection_2".to_string(),
            dimension: Some(1),
            tenant: tenant_2.clone(),
            database: "database_2".to_string(),
            total_records_post_compaction,
            size_bytes_post_compaction,
            last_compaction_time_secs,
            ..Default::default()
        };
        let collection_uuid_2 = collection_2.collection_id;

        match sysdb {
            SysDb::Test(ref mut sysdb) => {
                sysdb.add_collection(collection_1);
                sysdb.add_collection(collection_2);
            }
            _ => panic!("Invalid sysdb type"),
        }

        let mut file_path_1 = HashMap::new();
        file_path_1.insert("hnsw".to_string(), vec!["path_1".to_string()]);
        let segment_id_1 = SegmentUuid::from_str("00000000-0000-0000-0000-000000000003").unwrap();

        let segment_1 = Segment {
            id: segment_id_1,
            r#type: SegmentType::HnswDistributed,
            scope: SegmentScope::VECTOR,
            collection: collection_uuid_1,
            metadata: None,
            file_path: file_path_1.clone(),
        };

        let mut file_path_2 = HashMap::new();
        file_path_2.insert("hnsw".to_string(), vec!["path_2".to_string()]);
        let segment_id_2 = SegmentUuid::from_str("00000000-0000-0000-0000-000000000004").unwrap();
        let segment_2 = Segment {
            id: segment_id_2,
            r#type: SegmentType::HnswDistributed,
            scope: SegmentScope::VECTOR,
            collection: collection_uuid_2,
            metadata: None,
            file_path: file_path_2.clone(),
        };
        match sysdb {
            SysDb::Test(ref mut sysdb) => {
                sysdb.add_segment(segment_1);
                sysdb.add_segment(segment_2);
            }
            _ => panic!("Invalid sysdb type"),
        }

        let mut file_path_3 = HashMap::new();
        file_path_3.insert("hnsw".to_string(), vec!["path_3".to_string()]);

        let mut file_path_4 = HashMap::new();
        file_path_4.insert("hnsw".to_string(), vec!["path_4".to_string()]);
        let segment_flush_info = vec![
            SegmentFlushInfo {
                segment_id: segment_id_1,
                file_paths: file_path_3.clone(),
            },
            SegmentFlushInfo {
                segment_id: segment_id_2,
                file_paths: file_path_4.clone(),
            },
        ];

        let log_position = 100;
        let operator = RegisterOperator::new();
        let input = RegisterInput::new(
            tenant_1.clone(),
            collection_uuid_1,
            log_position,
            collection_version,
            segment_flush_info.into(),
            total_records_post_compaction,
            size_bytes_post_compaction,
            sysdb.clone(),
            log.clone(),
        );

        let result = operator.run(&input).await;

        assert!(result.is_ok());
        let result = result.unwrap();
        assert_eq!(
            result._sysdb_registration_result.collection_id,
            collection_uuid_1
        );
        assert_eq!(
            result._sysdb_registration_result.collection_version,
            collection_version + 1
        );

        let collections = sysdb
            .get_collections(GetCollectionsOptions {
                collection_id: Some(collection_uuid_1),
                ..Default::default()
            })
            .await;

        assert!(collections.is_ok());
        let collection = collections.unwrap();
        assert_eq!(collection.len(), 1);
        let collection = collection[0].clone();
        assert_eq!(collection.log_position, log_position);
        assert_eq!(
            collection.total_records_post_compaction,
            total_records_post_compaction
        );

        let collection_1_segments = sysdb
            .get_segments(None, None, None, collection_uuid_1)
            .await
            .unwrap();
        let collection_2_segments = sysdb
            .get_segments(None, None, None, collection_uuid_2)
            .await
            .unwrap();

        let segments = collection_1_segments
            .iter()
            .chain(collection_2_segments.iter())
            .collect::<Vec<&Segment>>();

        assert_eq!(segments.len(), 2);
        let segment_1 = segments.iter().find(|s| s.id == segment_id_1).unwrap();
        assert_eq!(segment_1.file_path, file_path_3);
        let segment_2 = segments.iter().find(|s| s.id == segment_id_2).unwrap();
        assert_eq!(segment_2.file_path, file_path_4);
    }
}
