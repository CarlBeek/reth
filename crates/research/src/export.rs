//! Export research SQLite data to Parquet datasets optimized for analytics.

use arrow_array::{
    ArrayRef, BooleanArray, Float64Array, Int64Array, RecordBatch, StringArray, UInt64Array,
};
use arrow_schema::{DataType, Field, Schema};
use parquet::{arrow::ArrowWriter, basic::Compression, file::properties::WriterProperties};
use rusqlite::{params, Connection};
use serde::Serialize;
use std::{
    fs::{self, File},
    io,
    path::{Path, PathBuf},
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};
use thiserror::Error;

/// Errors that can occur while exporting research data to Parquet.
#[derive(Debug, Error)]
pub enum ExportError {
    /// SQLite read/query failure.
    #[error("SQLite error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    /// Filesystem failure while creating output datasets.
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    /// Arrow batch/schema construction failure.
    #[error("Arrow error: {0}")]
    Arrow(#[from] arrow_schema::ArrowError),
    /// Parquet writer failure.
    #[error("Parquet error: {0}")]
    Parquet(#[from] parquet::errors::ParquetError),
    /// Manifest JSON serialization failure.
    #[error("Serialization error: {0}")]
    Serialization(#[from] serde_json::Error),
}

/// Summary of a completed Parquet export.
#[derive(Debug, Clone, Serialize)]
pub struct ExportStats {
    /// Source SQLite database path.
    pub db_path: PathBuf,
    /// Root output directory containing all exported datasets.
    pub output_dir: PathBuf,
    /// Maximum rows written per Arrow/Parquet batch.
    pub row_group_size: usize,
    /// Number of blocks per partition bucket.
    pub block_bucket_size: u64,
    /// Whether the export appended incrementally from checkpoints.
    pub incremental: bool,
    /// Number of rows written to the block coverage dataset.
    pub coverage_rows: usize,
    /// Number of rows written to the hot divergence dataset.
    pub hot_rows: usize,
    /// Number of rows written to the cold artifact dataset.
    pub artifact_rows: usize,
    /// Schedule names exported.
    pub schedules: Vec<String>,
    /// Highest divergence id exported after this run.
    pub last_divergence_id: i64,
    /// Highest block coverage id exported after this run.
    pub last_coverage_id: i64,
}

#[derive(Debug, Clone, Default, Serialize, serde::Deserialize)]
struct ExportCheckpoint {
    last_divergence_id: i64,
    last_coverage_id: i64,
}

#[derive(Debug, Clone)]
struct CoverageRow {
    schedule_name: String,
    schedule_kind: String,
    schedule_config_hash: String,
    block_number: u64,
    block_hash: String,
    parent_hash: String,
    timestamp: u64,
    tx_count: u64,
    divergence_count: u64,
    status_divergence_count: u64,
    gas_divergence_count: u64,
    call_tree_divergence_count: u64,
    event_log_divergence_count: u64,
    output_divergence_count: u64,
    created_address_divergence_count: u64,
    logs_bloom_divergence_count: u64,
    total_baseline_gas_used: u64,
    total_schedule_gas_used: u64,
    total_gas_delta: i64,
}

#[derive(Debug, Clone)]
struct HotRow {
    divergence_id: i64,
    schedule_name: String,
    schedule_kind: String,
    schedule_description: String,
    schedule_config_hash: String,
    block_number: u64,
    tx_index: u64,
    tx_hash: String,
    block_hash: String,
    parent_hash: String,
    timestamp: u64,
    divergence_type: String,
    baseline_success: bool,
    schedule_success: bool,
    baseline_gas_used: u64,
    schedule_gas_used: u64,
    baseline_intrinsic_gas: u64,
    schedule_intrinsic_gas: Option<u64>,
    gas_delta: i64,
    gas_efficiency_ratio: Option<f64>,
    tx_category: Option<String>,
    status_changed: bool,
    gas_changed: bool,
    call_tree_changed: bool,
    event_logs_changed: bool,
    output_changed: bool,
    created_address_changed: bool,
    logs_bloom_changed: bool,
    sender: String,
    recipient: Option<String>,
    value_wei: String,
    input_len: u64,
    input_zero_bytes: u64,
    input_nonzero_bytes: u64,
    tx_gas_limit: u64,
    access_list_accounts: u64,
    access_list_storage_slots: u64,
    authorization_count: u64,
    is_create: bool,
    baseline_output_len: Option<u64>,
    schedule_output_len: Option<u64>,
    baseline_output_hash: Option<String>,
    schedule_output_hash: Option<String>,
    baseline_created_address: Option<String>,
    schedule_created_address: Option<String>,
    baseline_log_count: u64,
    schedule_log_count: u64,
    baseline_logs_bloom: String,
    schedule_logs_bloom: String,
    baseline_call_frames_hash: Option<String>,
    schedule_call_frames_hash: Option<String>,
    baseline_event_logs_hash: Option<String>,
    schedule_event_logs_hash: Option<String>,
}

#[derive(Debug, Clone)]
struct ArtifactRow {
    divergence_id: i64,
    schedule_name: String,
    block_number: u64,
    tx_index: u64,
    tx_hash: String,
    operation_counts: Option<String>,
    oog_info: Option<String>,
    divergence_location: Option<String>,
    baseline_call_frames: Option<String>,
    schedule_call_frames: Option<String>,
    baseline_event_logs: Option<String>,
    schedule_event_logs: Option<String>,
}

fn hex_blob(bytes: Vec<u8>) -> String {
    use std::fmt::Write;
    let mut s = String::with_capacity(2 + bytes.len() * 2);
    s.push_str("0x");
    for b in &bytes {
        let _ = write!(s, "{b:02x}");
    }
    s
}

fn encode_partition_value(value: &str) -> String {
    use std::fmt::Write;

    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' => {
                encoded.push(byte as char);
            }
            _ => {
                let _ = write!(encoded, "~{byte:02X}");
            }
        }
    }
    encoded
}

fn dataset_dir(base: &Path, dataset: &str) -> PathBuf {
    base.join(dataset)
}

fn schedule_dir(base: &Path, dataset: &str, schedule_name: &str) -> PathBuf {
    dataset_dir(base, dataset)
        .join(format!("schedule_name={}", encode_partition_value(schedule_name)))
}

fn block_bucket_dir(base: &Path, dataset: &str, schedule_name: &str, block_bucket: u64) -> PathBuf {
    schedule_dir(base, dataset, schedule_name).join(format!("block_bucket={block_bucket:016}"))
}

fn parquet_path(
    base: &Path,
    dataset: &str,
    schedule_name: &str,
    block_bucket: u64,
    export_id: u128,
) -> PathBuf {
    block_bucket_dir(base, dataset, schedule_name, block_bucket)
        .join(format!("part-{export_id:020}.parquet"))
}

fn writer_props(row_group_size: usize) -> WriterProperties {
    WriterProperties::builder()
        .set_compression(Compression::ZSTD(Default::default()))
        .set_max_row_group_size(row_group_size)
        .build()
}

fn make_batch(schema: Arc<Schema>, columns: Vec<ArrayRef>) -> Result<RecordBatch, ExportError> {
    Ok(RecordBatch::try_new(schema, columns)?)
}

fn string_array(values: Vec<String>) -> ArrayRef {
    Arc::new(StringArray::from(values))
}

fn opt_string_array(values: Vec<Option<String>>) -> ArrayRef {
    Arc::new(StringArray::from(values))
}

fn u64_array(values: Vec<u64>) -> ArrayRef {
    Arc::new(UInt64Array::from(values))
}

fn opt_u64_array(values: Vec<Option<u64>>) -> ArrayRef {
    Arc::new(UInt64Array::from(values))
}

fn i64_array(values: Vec<i64>) -> ArrayRef {
    Arc::new(Int64Array::from(values))
}

fn opt_f64_array(values: Vec<Option<f64>>) -> ArrayRef {
    Arc::new(Float64Array::from(values))
}

fn bool_array(values: Vec<bool>) -> ArrayRef {
    Arc::new(BooleanArray::from(values))
}

fn coverage_schema() -> Arc<Schema> {
    Arc::new(Schema::new(vec![
        Field::new("schedule_name", DataType::Utf8, false),
        Field::new("schedule_kind", DataType::Utf8, false),
        Field::new("schedule_config_hash", DataType::Utf8, false),
        Field::new("block_number", DataType::UInt64, false),
        Field::new("block_hash", DataType::Utf8, false),
        Field::new("parent_hash", DataType::Utf8, false),
        Field::new("timestamp", DataType::UInt64, false),
        Field::new("tx_count", DataType::UInt64, false),
        Field::new("divergence_count", DataType::UInt64, false),
        Field::new("status_divergence_count", DataType::UInt64, false),
        Field::new("gas_divergence_count", DataType::UInt64, false),
        Field::new("call_tree_divergence_count", DataType::UInt64, false),
        Field::new("event_log_divergence_count", DataType::UInt64, false),
        Field::new("output_divergence_count", DataType::UInt64, false),
        Field::new("created_address_divergence_count", DataType::UInt64, false),
        Field::new("logs_bloom_divergence_count", DataType::UInt64, false),
        Field::new("total_baseline_gas_used", DataType::UInt64, false),
        Field::new("total_schedule_gas_used", DataType::UInt64, false),
        Field::new("total_gas_delta", DataType::Int64, false),
    ]))
}

fn hot_schema() -> Arc<Schema> {
    Arc::new(Schema::new(vec![
        Field::new("divergence_id", DataType::Int64, false),
        Field::new("schedule_name", DataType::Utf8, false),
        Field::new("schedule_kind", DataType::Utf8, false),
        Field::new("schedule_description", DataType::Utf8, false),
        Field::new("schedule_config_hash", DataType::Utf8, false),
        Field::new("block_number", DataType::UInt64, false),
        Field::new("tx_index", DataType::UInt64, false),
        Field::new("tx_hash", DataType::Utf8, false),
        Field::new("block_hash", DataType::Utf8, false),
        Field::new("parent_hash", DataType::Utf8, false),
        Field::new("timestamp", DataType::UInt64, false),
        Field::new("divergence_type", DataType::Utf8, false),
        Field::new("baseline_success", DataType::Boolean, false),
        Field::new("schedule_success", DataType::Boolean, false),
        Field::new("baseline_gas_used", DataType::UInt64, false),
        Field::new("schedule_gas_used", DataType::UInt64, false),
        Field::new("baseline_intrinsic_gas", DataType::UInt64, false),
        Field::new("schedule_intrinsic_gas", DataType::UInt64, true),
        Field::new("gas_delta", DataType::Int64, false),
        Field::new("gas_efficiency_ratio", DataType::Float64, true),
        Field::new("tx_category", DataType::Utf8, true),
        Field::new("status_changed", DataType::Boolean, false),
        Field::new("gas_changed", DataType::Boolean, false),
        Field::new("call_tree_changed", DataType::Boolean, false),
        Field::new("event_logs_changed", DataType::Boolean, false),
        Field::new("output_changed", DataType::Boolean, false),
        Field::new("created_address_changed", DataType::Boolean, false),
        Field::new("logs_bloom_changed", DataType::Boolean, false),
        Field::new("sender", DataType::Utf8, false),
        Field::new("recipient", DataType::Utf8, true),
        Field::new("value_wei", DataType::Utf8, false),
        Field::new("input_len", DataType::UInt64, false),
        Field::new("input_zero_bytes", DataType::UInt64, false),
        Field::new("input_nonzero_bytes", DataType::UInt64, false),
        Field::new("tx_gas_limit", DataType::UInt64, false),
        Field::new("access_list_accounts", DataType::UInt64, false),
        Field::new("access_list_storage_slots", DataType::UInt64, false),
        Field::new("authorization_count", DataType::UInt64, false),
        Field::new("is_create", DataType::Boolean, false),
        Field::new("baseline_output_len", DataType::UInt64, true),
        Field::new("schedule_output_len", DataType::UInt64, true),
        Field::new("baseline_output_hash", DataType::Utf8, true),
        Field::new("schedule_output_hash", DataType::Utf8, true),
        Field::new("baseline_created_address", DataType::Utf8, true),
        Field::new("schedule_created_address", DataType::Utf8, true),
        Field::new("baseline_log_count", DataType::UInt64, false),
        Field::new("schedule_log_count", DataType::UInt64, false),
        Field::new("baseline_logs_bloom", DataType::Utf8, false),
        Field::new("schedule_logs_bloom", DataType::Utf8, false),
        Field::new("baseline_call_frames_hash", DataType::Utf8, true),
        Field::new("schedule_call_frames_hash", DataType::Utf8, true),
        Field::new("baseline_event_logs_hash", DataType::Utf8, true),
        Field::new("schedule_event_logs_hash", DataType::Utf8, true),
    ]))
}

fn artifacts_schema() -> Arc<Schema> {
    Arc::new(Schema::new(vec![
        Field::new("divergence_id", DataType::Int64, false),
        Field::new("schedule_name", DataType::Utf8, false),
        Field::new("block_number", DataType::UInt64, false),
        Field::new("tx_index", DataType::UInt64, false),
        Field::new("tx_hash", DataType::Utf8, false),
        Field::new("operation_counts", DataType::Utf8, true),
        Field::new("oog_info", DataType::Utf8, true),
        Field::new("divergence_location", DataType::Utf8, true),
        Field::new("baseline_call_frames", DataType::Utf8, true),
        Field::new("schedule_call_frames", DataType::Utf8, true),
        Field::new("baseline_event_logs", DataType::Utf8, true),
        Field::new("schedule_event_logs", DataType::Utf8, true),
    ]))
}

fn coverage_batch(rows: &[CoverageRow]) -> Result<RecordBatch, ExportError> {
    make_batch(
        coverage_schema(),
        vec![
            string_array(rows.iter().map(|r| r.schedule_name.clone()).collect()),
            string_array(rows.iter().map(|r| r.schedule_kind.clone()).collect()),
            string_array(rows.iter().map(|r| r.schedule_config_hash.clone()).collect()),
            u64_array(rows.iter().map(|r| r.block_number).collect()),
            string_array(rows.iter().map(|r| r.block_hash.clone()).collect()),
            string_array(rows.iter().map(|r| r.parent_hash.clone()).collect()),
            u64_array(rows.iter().map(|r| r.timestamp).collect()),
            u64_array(rows.iter().map(|r| r.tx_count).collect()),
            u64_array(rows.iter().map(|r| r.divergence_count).collect()),
            u64_array(rows.iter().map(|r| r.status_divergence_count).collect()),
            u64_array(rows.iter().map(|r| r.gas_divergence_count).collect()),
            u64_array(rows.iter().map(|r| r.call_tree_divergence_count).collect()),
            u64_array(rows.iter().map(|r| r.event_log_divergence_count).collect()),
            u64_array(rows.iter().map(|r| r.output_divergence_count).collect()),
            u64_array(rows.iter().map(|r| r.created_address_divergence_count).collect()),
            u64_array(rows.iter().map(|r| r.logs_bloom_divergence_count).collect()),
            u64_array(rows.iter().map(|r| r.total_baseline_gas_used).collect()),
            u64_array(rows.iter().map(|r| r.total_schedule_gas_used).collect()),
            i64_array(rows.iter().map(|r| r.total_gas_delta).collect()),
        ],
    )
}

fn hot_batch(rows: &[HotRow]) -> Result<RecordBatch, ExportError> {
    make_batch(
        hot_schema(),
        vec![
            i64_array(rows.iter().map(|r| r.divergence_id).collect()),
            string_array(rows.iter().map(|r| r.schedule_name.clone()).collect()),
            string_array(rows.iter().map(|r| r.schedule_kind.clone()).collect()),
            string_array(rows.iter().map(|r| r.schedule_description.clone()).collect()),
            string_array(rows.iter().map(|r| r.schedule_config_hash.clone()).collect()),
            u64_array(rows.iter().map(|r| r.block_number).collect()),
            u64_array(rows.iter().map(|r| r.tx_index).collect()),
            string_array(rows.iter().map(|r| r.tx_hash.clone()).collect()),
            string_array(rows.iter().map(|r| r.block_hash.clone()).collect()),
            string_array(rows.iter().map(|r| r.parent_hash.clone()).collect()),
            u64_array(rows.iter().map(|r| r.timestamp).collect()),
            string_array(rows.iter().map(|r| r.divergence_type.clone()).collect()),
            bool_array(rows.iter().map(|r| r.baseline_success).collect()),
            bool_array(rows.iter().map(|r| r.schedule_success).collect()),
            u64_array(rows.iter().map(|r| r.baseline_gas_used).collect()),
            u64_array(rows.iter().map(|r| r.schedule_gas_used).collect()),
            u64_array(rows.iter().map(|r| r.baseline_intrinsic_gas).collect()),
            opt_u64_array(rows.iter().map(|r| r.schedule_intrinsic_gas).collect()),
            i64_array(rows.iter().map(|r| r.gas_delta).collect()),
            opt_f64_array(rows.iter().map(|r| r.gas_efficiency_ratio).collect()),
            opt_string_array(rows.iter().map(|r| r.tx_category.clone()).collect()),
            bool_array(rows.iter().map(|r| r.status_changed).collect()),
            bool_array(rows.iter().map(|r| r.gas_changed).collect()),
            bool_array(rows.iter().map(|r| r.call_tree_changed).collect()),
            bool_array(rows.iter().map(|r| r.event_logs_changed).collect()),
            bool_array(rows.iter().map(|r| r.output_changed).collect()),
            bool_array(rows.iter().map(|r| r.created_address_changed).collect()),
            bool_array(rows.iter().map(|r| r.logs_bloom_changed).collect()),
            string_array(rows.iter().map(|r| r.sender.clone()).collect()),
            opt_string_array(rows.iter().map(|r| r.recipient.clone()).collect()),
            string_array(rows.iter().map(|r| r.value_wei.clone()).collect()),
            u64_array(rows.iter().map(|r| r.input_len).collect()),
            u64_array(rows.iter().map(|r| r.input_zero_bytes).collect()),
            u64_array(rows.iter().map(|r| r.input_nonzero_bytes).collect()),
            u64_array(rows.iter().map(|r| r.tx_gas_limit).collect()),
            u64_array(rows.iter().map(|r| r.access_list_accounts).collect()),
            u64_array(rows.iter().map(|r| r.access_list_storage_slots).collect()),
            u64_array(rows.iter().map(|r| r.authorization_count).collect()),
            bool_array(rows.iter().map(|r| r.is_create).collect()),
            opt_u64_array(rows.iter().map(|r| r.baseline_output_len).collect()),
            opt_u64_array(rows.iter().map(|r| r.schedule_output_len).collect()),
            opt_string_array(rows.iter().map(|r| r.baseline_output_hash.clone()).collect()),
            opt_string_array(rows.iter().map(|r| r.schedule_output_hash.clone()).collect()),
            opt_string_array(rows.iter().map(|r| r.baseline_created_address.clone()).collect()),
            opt_string_array(rows.iter().map(|r| r.schedule_created_address.clone()).collect()),
            u64_array(rows.iter().map(|r| r.baseline_log_count).collect()),
            u64_array(rows.iter().map(|r| r.schedule_log_count).collect()),
            string_array(rows.iter().map(|r| r.baseline_logs_bloom.clone()).collect()),
            string_array(rows.iter().map(|r| r.schedule_logs_bloom.clone()).collect()),
            opt_string_array(rows.iter().map(|r| r.baseline_call_frames_hash.clone()).collect()),
            opt_string_array(rows.iter().map(|r| r.schedule_call_frames_hash.clone()).collect()),
            opt_string_array(rows.iter().map(|r| r.baseline_event_logs_hash.clone()).collect()),
            opt_string_array(rows.iter().map(|r| r.schedule_event_logs_hash.clone()).collect()),
        ],
    )
}

fn artifacts_batch(rows: &[ArtifactRow]) -> Result<RecordBatch, ExportError> {
    make_batch(
        artifacts_schema(),
        vec![
            i64_array(rows.iter().map(|r| r.divergence_id).collect()),
            string_array(rows.iter().map(|r| r.schedule_name.clone()).collect()),
            u64_array(rows.iter().map(|r| r.block_number).collect()),
            u64_array(rows.iter().map(|r| r.tx_index).collect()),
            string_array(rows.iter().map(|r| r.tx_hash.clone()).collect()),
            opt_string_array(rows.iter().map(|r| r.operation_counts.clone()).collect()),
            opt_string_array(rows.iter().map(|r| r.oog_info.clone()).collect()),
            opt_string_array(rows.iter().map(|r| r.divergence_location.clone()).collect()),
            opt_string_array(rows.iter().map(|r| r.baseline_call_frames.clone()).collect()),
            opt_string_array(rows.iter().map(|r| r.schedule_call_frames.clone()).collect()),
            opt_string_array(rows.iter().map(|r| r.baseline_event_logs.clone()).collect()),
            opt_string_array(rows.iter().map(|r| r.schedule_event_logs.clone()).collect()),
        ],
    )
}

fn open_writer(
    path: &Path,
    schema: Arc<Schema>,
    row_group_size: usize,
) -> Result<ArrowWriter<File>, ExportError> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let file = File::create(path)?;
    Ok(ArrowWriter::try_new(file, schema, Some(writer_props(row_group_size)))?)
}

fn collect_schedules(conn: &Connection) -> Result<Vec<String>, ExportError> {
    let mut stmt = conn.prepare(
        "SELECT DISTINCT schedule_name FROM (
            SELECT schedule_name FROM schedule_divergences
            UNION
            SELECT schedule_name FROM schedule_block_coverage
        ) ORDER BY schedule_name",
    )?;
    let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
    let mut schedules = Vec::new();
    for row in rows {
        schedules.push(row?);
    }
    Ok(schedules)
}

fn block_bucket(block_number: u64, block_bucket_size: u64) -> u64 {
    (block_number / block_bucket_size) * block_bucket_size
}

fn checkpoint_path(out_dir: &Path) -> PathBuf {
    out_dir.join("_checkpoint.json")
}

fn save_checkpoint(out_dir: &Path, checkpoint: &ExportCheckpoint) -> Result<(), ExportError> {
    fs::write(checkpoint_path(out_dir), serde_json::to_vec_pretty(checkpoint)?)?;
    Ok(())
}

fn export_run_id() -> u128 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_millis()
}

/// Streams rows into partitioned Parquet files, flushing every `row_group_size` rows.
///
/// Rows must arrive ordered by block_number so that each bucket's rows are consecutive.
/// Memory usage is bounded to `row_group_size` rows at a time.
struct PartitionedSink<T, F> {
    base_dir: PathBuf,
    dataset: String,
    schema: Arc<Schema>,
    schedule_name: String,
    export_id: u128,
    row_group_size: usize,
    block_bucket_size: u64,
    to_batch: F,
    current_bucket: Option<u64>,
    buffer: Vec<T>,
    writer: Option<ArrowWriter<File>>,
    total: usize,
}

impl<T, F> PartitionedSink<T, F>
where
    F: Fn(&[T]) -> Result<RecordBatch, ExportError>,
{
    fn new(
        base_dir: &Path,
        dataset: &str,
        schema: Arc<Schema>,
        schedule_name: &str,
        export_id: u128,
        row_group_size: usize,
        block_bucket_size: u64,
        to_batch: F,
    ) -> Self {
        Self {
            base_dir: base_dir.to_path_buf(),
            dataset: dataset.to_string(),
            schema,
            schedule_name: schedule_name.to_string(),
            export_id,
            row_group_size,
            block_bucket_size,
            to_batch,
            current_bucket: None,
            buffer: Vec::with_capacity(row_group_size),
            writer: None,
            total: 0,
        }
    }

    fn push(&mut self, block_number: u64, row: T) -> Result<(), ExportError> {
        let bucket = block_bucket(block_number, self.block_bucket_size);

        if self.current_bucket != Some(bucket) {
            self.flush_and_close()?;
            self.current_bucket = Some(bucket);
        }

        self.buffer.push(row);
        if self.buffer.len() >= self.row_group_size {
            self.flush_buffer()?;
        }
        Ok(())
    }

    fn flush_buffer(&mut self) -> Result<(), ExportError> {
        if self.buffer.is_empty() {
            return Ok(());
        }
        let bucket = self.current_bucket.expect("bucket must be set before flush");
        let writer = match self.writer.as_mut() {
            Some(w) => w,
            None => {
                let path = parquet_path(
                    &self.base_dir,
                    &self.dataset,
                    &self.schedule_name,
                    bucket,
                    self.export_id,
                );
                self.writer = Some(open_writer(&path, self.schema.clone(), self.row_group_size)?);
                self.writer.as_mut().unwrap()
            }
        };
        let batch = (self.to_batch)(&self.buffer)?;
        writer.write(&batch)?;
        self.total += self.buffer.len();
        self.buffer.clear();
        Ok(())
    }

    fn flush_and_close(&mut self) -> Result<(), ExportError> {
        self.flush_buffer()?;
        if let Some(writer) = self.writer.take() {
            writer.close()?;
        }
        Ok(())
    }

    fn finish(mut self) -> Result<usize, ExportError> {
        self.flush_and_close()?;
        Ok(self.total)
    }
}

fn export_coverage_for_schedule(
    conn: &Connection,
    out_dir: &Path,
    schedule_name: &str,
    block_bucket_size: u64,
    row_group_size: usize,
    export_id: u128,
) -> Result<(usize, i64), ExportError> {
    let mut stmt = conn.prepare(
        "SELECT id, schedule_name, schedule_kind, schedule_config_hash, block_number,
                block_hash, parent_hash, timestamp, tx_count, divergence_count,
                status_divergence_count, gas_divergence_count, call_tree_divergence_count,
                event_log_divergence_count, output_divergence_count,
                created_address_divergence_count, logs_bloom_divergence_count,
                total_baseline_gas_used, total_schedule_gas_used, total_gas_delta
         FROM schedule_block_coverage
         WHERE schedule_name = ?1
         ORDER BY block_number",
    )?;
    let mut rows = stmt.query(params![schedule_name])?;
    let mut sink = PartitionedSink::new(
        out_dir,
        "block_coverage",
        coverage_schema(),
        schedule_name,
        export_id,
        row_group_size,
        block_bucket_size,
        coverage_batch,
    );
    let mut max_id = 0;
    while let Some(row) = rows.next()? {
        let source_id: i64 = row.get(0)?;
        let block_number: u64 = row.get(4)?;
        let coverage = CoverageRow {
            schedule_name: row.get(1)?,
            schedule_kind: row.get(2)?,
            schedule_config_hash: row.get(3)?,
            block_number,
            block_hash: hex_blob(row.get(5)?),
            parent_hash: hex_blob(row.get(6)?),
            timestamp: row.get(7)?,
            tx_count: row.get(8)?,
            divergence_count: row.get(9)?,
            status_divergence_count: row.get(10)?,
            gas_divergence_count: row.get(11)?,
            call_tree_divergence_count: row.get(12)?,
            event_log_divergence_count: row.get(13)?,
            output_divergence_count: row.get(14)?,
            created_address_divergence_count: row.get(15)?,
            logs_bloom_divergence_count: row.get(16)?,
            total_baseline_gas_used: row.get(17)?,
            total_schedule_gas_used: row.get(18)?,
            total_gas_delta: row.get(19)?,
        };
        max_id = max_id.max(source_id);
        sink.push(block_number, coverage)?;
    }
    let total = sink.finish()?;
    Ok((total, max_id))
}

fn export_hot_for_schedule(
    conn: &Connection,
    out_dir: &Path,
    schedule_name: &str,
    block_bucket_size: u64,
    row_group_size: usize,
    export_id: u128,
) -> Result<(usize, i64), ExportError> {
    let mut stmt = conn.prepare(
        "SELECT id, schedule_name, schedule_kind, schedule_description, schedule_config_hash,
                block_number, tx_index, tx_hash, block_hash, parent_hash, timestamp,
                divergence_type, baseline_success, schedule_success, baseline_gas_used,
                schedule_gas_used, baseline_intrinsic_gas, schedule_intrinsic_gas,
                gas_delta, gas_efficiency_ratio, tx_category, status_changed, gas_changed,
                call_tree_changed, event_logs_changed, output_changed,
                created_address_changed, logs_bloom_changed, sender, recipient, value_wei,
                input_len, input_zero_bytes, input_nonzero_bytes, tx_gas_limit,
                access_list_accounts, access_list_storage_slots, authorization_count, is_create,
                baseline_output_len, schedule_output_len, baseline_output_hash, schedule_output_hash,
                baseline_created_address, schedule_created_address, baseline_log_count,
                schedule_log_count, baseline_logs_bloom, schedule_logs_bloom,
                baseline_call_frames_hash, schedule_call_frames_hash,
                baseline_event_logs_hash, schedule_event_logs_hash
         FROM schedule_divergences
         WHERE schedule_name = ?1
         ORDER BY block_number, tx_index",
    )?;
    let mut rows = stmt.query(params![schedule_name])?;
    let mut sink = PartitionedSink::new(
        out_dir,
        "divergences_hot",
        hot_schema(),
        schedule_name,
        export_id,
        row_group_size,
        block_bucket_size,
        hot_batch,
    );
    let mut max_id = 0;
    while let Some(row) = rows.next()? {
        let divergence_id: i64 = row.get(0)?;
        let block_number: u64 = row.get(5)?;
        let hot = HotRow {
            divergence_id,
            schedule_name: row.get(1)?,
            schedule_kind: row.get(2)?,
            schedule_description: row.get(3)?,
            schedule_config_hash: row.get(4)?,
            block_number,
            tx_index: row.get(6)?,
            tx_hash: hex_blob(row.get(7)?),
            block_hash: hex_blob(row.get(8)?),
            parent_hash: hex_blob(row.get(9)?),
            timestamp: row.get(10)?,
            divergence_type: row.get(11)?,
            baseline_success: row.get(12)?,
            schedule_success: row.get(13)?,
            baseline_gas_used: row.get(14)?,
            schedule_gas_used: row.get(15)?,
            baseline_intrinsic_gas: row.get(16)?,
            schedule_intrinsic_gas: row.get(17)?,
            gas_delta: row.get(18)?,
            gas_efficiency_ratio: row.get(19)?,
            tx_category: row.get(20)?,
            status_changed: row.get(21)?,
            gas_changed: row.get(22)?,
            call_tree_changed: row.get(23)?,
            event_logs_changed: row.get(24)?,
            output_changed: row.get(25)?,
            created_address_changed: row.get(26)?,
            logs_bloom_changed: row.get(27)?,
            sender: row.get(28)?,
            recipient: row.get(29)?,
            value_wei: row.get(30)?,
            input_len: row.get(31)?,
            input_zero_bytes: row.get(32)?,
            input_nonzero_bytes: row.get(33)?,
            tx_gas_limit: row.get(34)?,
            access_list_accounts: row.get(35)?,
            access_list_storage_slots: row.get(36)?,
            authorization_count: row.get(37)?,
            is_create: row.get(38)?,
            baseline_output_len: row.get(39)?,
            schedule_output_len: row.get(40)?,
            baseline_output_hash: row.get(41)?,
            schedule_output_hash: row.get(42)?,
            baseline_created_address: row.get(43)?,
            schedule_created_address: row.get(44)?,
            baseline_log_count: row.get(45)?,
            schedule_log_count: row.get(46)?,
            baseline_logs_bloom: row.get(47)?,
            schedule_logs_bloom: row.get(48)?,
            baseline_call_frames_hash: row.get(49)?,
            schedule_call_frames_hash: row.get(50)?,
            baseline_event_logs_hash: row.get(51)?,
            schedule_event_logs_hash: row.get(52)?,
        };
        max_id = max_id.max(divergence_id);
        sink.push(block_number, hot)?;
    }
    let total = sink.finish()?;
    Ok((total, max_id))
}

fn export_artifacts_for_schedule(
    conn: &Connection,
    out_dir: &Path,
    schedule_name: &str,
    block_bucket_size: u64,
    row_group_size: usize,
    export_id: u128,
) -> Result<(usize, i64), ExportError> {
    let mut stmt = conn.prepare(
        "SELECT id, schedule_name, block_number, tx_index, tx_hash,
                operation_counts, oog_info, divergence_location,
                baseline_call_frames, schedule_call_frames,
                baseline_event_logs, schedule_event_logs
         FROM schedule_divergences
         WHERE schedule_name = ?1
           AND (
                operation_counts IS NOT NULL OR
                oog_info IS NOT NULL OR
                divergence_location IS NOT NULL OR
                baseline_call_frames IS NOT NULL OR
                schedule_call_frames IS NOT NULL OR
                baseline_event_logs IS NOT NULL OR
                schedule_event_logs IS NOT NULL
           )
         ORDER BY block_number, tx_index",
    )?;
    let mut rows = stmt.query(params![schedule_name])?;
    let mut sink = PartitionedSink::new(
        out_dir,
        "divergence_artifacts",
        artifacts_schema(),
        schedule_name,
        export_id,
        row_group_size,
        block_bucket_size,
        artifacts_batch,
    );
    let mut max_id = 0;
    while let Some(row) = rows.next()? {
        let divergence_id: i64 = row.get(0)?;
        let block_number: u64 = row.get(2)?;
        let artifact = ArtifactRow {
            divergence_id,
            schedule_name: row.get(1)?,
            block_number,
            tx_index: row.get(3)?,
            tx_hash: hex_blob(row.get(4)?),
            operation_counts: row.get(5)?,
            oog_info: row.get(6)?,
            divergence_location: row.get(7)?,
            baseline_call_frames: row.get(8)?,
            schedule_call_frames: row.get(9)?,
            baseline_event_logs: row.get(10)?,
            schedule_event_logs: row.get(11)?,
        };
        max_id = max_id.max(divergence_id);
        sink.push(block_number, artifact)?;
    }
    let total = sink.finish()?;
    Ok((total, max_id))
}

fn remove_dir_if_exists(path: &Path) -> Result<(), ExportError> {
    match fs::remove_dir_all(path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err.into()),
    }
}

fn remove_stale_schedule_dirs(
    out_dir: &Path,
    dataset: &str,
    schedules: &[String],
) -> Result<(), ExportError> {
    let dataset_path = dataset_dir(out_dir, dataset);
    if !dataset_path.exists() {
        return Ok(());
    }

    let expected: std::collections::HashSet<_> = schedules
        .iter()
        .map(|schedule| format!("schedule_name={}", encode_partition_value(schedule)))
        .collect();

    for entry in fs::read_dir(&dataset_path)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let file_name = entry.file_name();
        let file_name = file_name.to_string_lossy();
        if file_name.starts_with("schedule_name=") && !expected.contains(file_name.as_ref()) {
            fs::remove_dir_all(entry.path())?;
        }
    }

    Ok(())
}

/// Export the research SQLite database into Parquet datasets optimized for analytics.
///
/// The export preserves all currently stored rows while splitting them into:
///
/// - `block_coverage/`
/// - `divergences_hot/`
/// - `divergence_artifacts/`
///
/// Each dataset is partitioned by `schedule_name`, and a `_manifest.json` file is written at the
/// root with export counts and parameters.
///
/// Incremental exports are snapshot-correct: each run rewrites the live schedule partitions from
/// SQLite so upserts and block-range deletes are reflected in Parquet without requiring a full
/// dataset refresh.
pub fn export_sqlite_to_parquet(
    db_path: impl AsRef<Path>,
    out_dir: impl AsRef<Path>,
    row_group_size: usize,
    block_bucket_size: u64,
    incremental: bool,
) -> Result<ExportStats, ExportError> {
    let db_path = db_path.as_ref().to_path_buf();
    let out_dir = out_dir.as_ref().to_path_buf();

    if !incremental {
        // Clean dataset directories to avoid stale part files from prior runs.
        for dataset in &["block_coverage", "divergences_hot", "divergence_artifacts"] {
            let dir = out_dir.join(dataset);
            if dir.exists() {
                fs::remove_dir_all(&dir)?;
            }
        }
        // Also remove old checkpoint/manifest so we start clean.
        let _ = fs::remove_file(checkpoint_path(&out_dir));
        let _ = fs::remove_file(out_dir.join("_manifest.json"));
    }

    fs::create_dir_all(&out_dir)?;
    let conn = Connection::open(&db_path)?;
    let schedules = collect_schedules(&conn)?;
    let export_id = export_run_id();

    let mut coverage_rows = 0usize;
    let mut hot_rows = 0usize;
    let mut artifact_rows = 0usize;
    let mut last_divergence_id = 0;
    let mut last_coverage_id = 0;

    for dataset in &["block_coverage", "divergences_hot", "divergence_artifacts"] {
        remove_stale_schedule_dirs(&out_dir, dataset, &schedules)?;
    }

    for schedule in &schedules {
        for dataset in &["block_coverage", "divergences_hot", "divergence_artifacts"] {
            remove_dir_if_exists(&schedule_dir(&out_dir, dataset, schedule))?;
        }

        let (coverage_written, coverage_max_id) = export_coverage_for_schedule(
            &conn,
            &out_dir,
            schedule,
            block_bucket_size,
            row_group_size,
            export_id,
        )?;
        coverage_rows += coverage_written;
        last_coverage_id = last_coverage_id.max(coverage_max_id);

        let (hot_written, divergence_max_id) = export_hot_for_schedule(
            &conn,
            &out_dir,
            schedule,
            block_bucket_size,
            row_group_size,
            export_id,
        )?;
        hot_rows += hot_written;
        last_divergence_id = last_divergence_id.max(divergence_max_id);

        let (artifact_written, artifact_max_id) = export_artifacts_for_schedule(
            &conn,
            &out_dir,
            schedule,
            block_bucket_size,
            row_group_size,
            export_id,
        )?;
        artifact_rows += artifact_written;
        last_divergence_id = last_divergence_id.max(artifact_max_id);
    }

    let stats = ExportStats {
        db_path: db_path.clone(),
        output_dir: out_dir.clone(),
        row_group_size,
        block_bucket_size,
        incremental,
        coverage_rows,
        hot_rows,
        artifact_rows,
        schedules,
        last_divergence_id,
        last_coverage_id,
    };
    save_checkpoint(&out_dir, &ExportCheckpoint { last_divergence_id, last_coverage_id })?;
    let manifest_path = out_dir.join("_manifest.json");
    fs::write(manifest_path, serde_json::to_vec_pretty(&stats)?)?;
    Ok(stats)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        database::{DivergenceDatabase, ScheduleBlockCoverage, ScheduleDivergence},
        divergence::DivergenceType,
    };
    use alloy_primitives::B256;
    use arrow_array::{Int64Array, UInt64Array};
    use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;

    fn sample_divergence(schedule: &str, block: u64, tx_idx: u64) -> ScheduleDivergence {
        ScheduleDivergence {
            schedule_name: schedule.to_string(),
            block_number: block,
            tx_index: tx_idx,
            tx_hash: B256::ZERO,
            timestamp: 1700000000,
            divergence_type: DivergenceType::GasPattern,
            schedule_kind: "test".to_string(),
            schedule_description: "test schedule".to_string(),
            schedule_config_hash: "abc123".to_string(),
            block_hash: B256::ZERO,
            parent_hash: B256::ZERO,
            baseline_success: true,
            baseline_gas_used: 21000,
            baseline_intrinsic_gas: 21000,
            schedule_success: true,
            schedule_gas_used: 20000,
            schedule_intrinsic_gas: Some(20000),
            gas_delta: 1000,
            gas_efficiency_ratio: Some(0.95),
            tx_category: None,
            affected_opcodes: None,
            affected_precompiles: None,
            oog_info: None,
            divergence_location: None,
            operation_counts: Some(r#"{"ADD":5}"#.to_string()),
            baseline_call_frames: None,
            schedule_call_frames: None,
            baseline_event_logs: None,
            schedule_event_logs: None,
            baseline_call_frames_hash: None,
            schedule_call_frames_hash: None,
            baseline_event_logs_hash: None,
            schedule_event_logs_hash: None,
            status_changed: false,
            gas_changed: true,
            call_tree_changed: false,
            event_logs_changed: false,
            output_changed: false,
            created_address_changed: false,
            logs_bloom_changed: false,
            sender: "0x0000000000000000000000000000000000000001".to_string(),
            recipient: Some("0x0000000000000000000000000000000000000002".to_string()),
            value_wei: "0".to_string(),
            input_len: 0,
            input_zero_bytes: 0,
            input_nonzero_bytes: 0,
            tx_gas_limit: 21000,
            access_list_accounts: 0,
            access_list_storage_slots: 0,
            authorization_count: 0,
            is_create: false,
            baseline_output_len: None,
            schedule_output_len: None,
            baseline_output_hash: None,
            schedule_output_hash: None,
            baseline_created_address: None,
            schedule_created_address: None,
            baseline_log_count: 0,
            schedule_log_count: 0,
            baseline_logs_bloom: String::new(),
            schedule_logs_bloom: String::new(),
        }
    }

    fn sample_coverage(schedule: &str, block: u64) -> ScheduleBlockCoverage {
        ScheduleBlockCoverage {
            schedule_name: schedule.to_string(),
            schedule_kind: "test".to_string(),
            schedule_config_hash: "abc123".to_string(),
            block_number: block,
            block_hash: B256::ZERO,
            parent_hash: B256::ZERO,
            timestamp: 1700000000,
            tx_count: 10,
            divergence_count: 2,
            status_divergence_count: 0,
            gas_divergence_count: 2,
            call_tree_divergence_count: 0,
            event_log_divergence_count: 0,
            output_divergence_count: 0,
            created_address_divergence_count: 0,
            logs_bloom_divergence_count: 0,
            total_baseline_gas_used: 210000,
            total_schedule_gas_used: 200000,
            total_gas_delta: 10000,
        }
    }

    fn count_parquet_rows(path: &Path) -> usize {
        let file = File::open(path).unwrap();
        let reader = ParquetRecordBatchReaderBuilder::try_new(file).unwrap().build().unwrap();
        reader.map(|batch| batch.unwrap().num_rows()).sum()
    }

    fn parquet_i64_values_recursive(dir: &Path, column: &str) -> Vec<i64> {
        let mut values = Vec::new();
        for path in walkdir(dir) {
            if path.extension().is_none_or(|e| e != "parquet") {
                continue;
            }
            let file = File::open(path).unwrap();
            let reader = ParquetRecordBatchReaderBuilder::try_new(file).unwrap().build().unwrap();
            for batch in reader {
                let batch = batch.unwrap();
                let column = batch
                    .column_by_name(column)
                    .unwrap()
                    .as_any()
                    .downcast_ref::<Int64Array>()
                    .unwrap();
                values.extend((0..column.len()).map(|idx| column.value(idx)));
            }
        }
        values
    }

    fn parquet_u64_values_recursive(dir: &Path, column: &str) -> Vec<u64> {
        let mut values = Vec::new();
        for path in walkdir(dir) {
            if path.extension().is_none_or(|e| e != "parquet") {
                continue;
            }
            let file = File::open(path).unwrap();
            let reader = ParquetRecordBatchReaderBuilder::try_new(file).unwrap().build().unwrap();
            for batch in reader {
                let batch = batch.unwrap();
                let column = batch
                    .column_by_name(column)
                    .unwrap()
                    .as_any()
                    .downcast_ref::<UInt64Array>()
                    .unwrap();
                values.extend((0..column.len()).map(|idx| column.value(idx)));
            }
        }
        values
    }

    fn count_parquet_rows_recursive(dir: &Path) -> usize {
        let mut total = 0;
        if !dir.exists() {
            return 0;
        }
        for entry in walkdir(dir) {
            if entry.extension().is_some_and(|e| e == "parquet") {
                total += count_parquet_rows(&entry);
            }
        }
        total
    }

    fn walkdir(dir: &Path) -> Vec<PathBuf> {
        let mut result = Vec::new();
        if let Ok(entries) = fs::read_dir(dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    result.extend(walkdir(&path));
                } else {
                    result.push(path);
                }
            }
        }
        result
    }

    #[test]
    fn test_round_trip_export() {
        let tmp = tempfile::tempdir().unwrap();
        let db_path = tmp.path().join("test.db");
        let out_dir = tmp.path().join("parquet_out");

        let db = DivergenceDatabase::open(&db_path).unwrap();
        db.record_schedule_divergence(&sample_divergence("4x", 100, 0)).unwrap();
        db.record_schedule_divergence(&sample_divergence("4x", 100, 1)).unwrap();
        db.record_schedule_divergence(&sample_divergence("4x", 200_001, 0)).unwrap();
        db.record_schedule_block_coverage(&sample_coverage("4x", 100)).unwrap();
        db.record_schedule_block_coverage(&sample_coverage("4x", 200_001)).unwrap();
        drop(db);

        let stats = export_sqlite_to_parquet(&db_path, &out_dir, 1000, 100_000, false).unwrap();

        assert_eq!(stats.hot_rows, 3);
        assert_eq!(stats.coverage_rows, 2);
        assert_eq!(stats.artifact_rows, 3); // all 3 have operation_counts
        assert_eq!(stats.schedules, vec!["4x"]);
        assert!(stats.last_divergence_id > 0);
        assert!(stats.last_coverage_id > 0);

        // Verify Parquet files are readable with correct row counts.
        let hot_count = count_parquet_rows_recursive(&out_dir.join("divergences_hot"));
        assert_eq!(hot_count, 3);

        let cov_count = count_parquet_rows_recursive(&out_dir.join("block_coverage"));
        assert_eq!(cov_count, 2);

        let art_count = count_parquet_rows_recursive(&out_dir.join("divergence_artifacts"));
        assert_eq!(art_count, 3);

        // Verify partitioning: block 100 is in bucket 0, block 200001 is in bucket 200000.
        assert!(out_dir
            .join("divergences_hot/schedule_name=4x/block_bucket=0000000000000000")
            .exists());
        assert!(out_dir
            .join("divergences_hot/schedule_name=4x/block_bucket=0000000000200000")
            .exists());

        // Verify manifest and checkpoint files exist.
        assert!(out_dir.join("_manifest.json").exists());
        assert!(out_dir.join("_checkpoint.json").exists());
    }

    #[test]
    fn test_incremental_export() {
        let tmp = tempfile::tempdir().unwrap();
        let db_path = tmp.path().join("test.db");
        let out_dir = tmp.path().join("parquet_out");

        let db = DivergenceDatabase::open(&db_path).unwrap();
        db.record_schedule_divergence(&sample_divergence("4x", 100, 0)).unwrap();
        db.record_schedule_block_coverage(&sample_coverage("4x", 100)).unwrap();
        drop(db);

        // First export.
        let stats1 = export_sqlite_to_parquet(&db_path, &out_dir, 1000, 100_000, true).unwrap();
        assert_eq!(stats1.hot_rows, 1);
        assert_eq!(stats1.coverage_rows, 1);

        // Insert more data.
        let db = DivergenceDatabase::open(&db_path).unwrap();
        db.record_schedule_divergence(&sample_divergence("4x", 200, 0)).unwrap();
        db.record_schedule_block_coverage(&sample_coverage("4x", 200)).unwrap();
        drop(db);

        // Incremental export rewrites the live snapshot for the schedule.
        let stats2 = export_sqlite_to_parquet(&db_path, &out_dir, 1000, 100_000, true).unwrap();
        assert_eq!(stats2.hot_rows, 2);
        assert_eq!(stats2.coverage_rows, 2);
        assert!(stats2.incremental);

        // Total Parquet rows should match the current SQLite snapshot.
        let hot_count = count_parquet_rows_recursive(&out_dir.join("divergences_hot"));
        assert_eq!(hot_count, 2);
    }

    #[test]
    fn test_incremental_export_rewrites_updated_rows() {
        let tmp = tempfile::tempdir().unwrap();
        let db_path = tmp.path().join("test.db");
        let out_dir = tmp.path().join("parquet_out");

        let db = DivergenceDatabase::open(&db_path).unwrap();
        db.record_schedule_divergence(&sample_divergence("4x", 100, 0)).unwrap();
        drop(db);

        export_sqlite_to_parquet(&db_path, &out_dir, 1000, 100_000, true).unwrap();

        let db = DivergenceDatabase::open(&db_path).unwrap();
        let mut updated = sample_divergence("4x", 100, 0);
        updated.gas_delta = 777;
        updated.schedule_gas_used = 19_223;
        db.record_schedule_divergence(&updated).unwrap();
        drop(db);

        let stats = export_sqlite_to_parquet(&db_path, &out_dir, 1000, 100_000, true).unwrap();
        assert_eq!(stats.hot_rows, 1);

        let hot_dir = out_dir.join("divergences_hot");
        assert_eq!(count_parquet_rows_recursive(&hot_dir), 1);
        assert_eq!(parquet_i64_values_recursive(&hot_dir, "gas_delta"), vec![777]);
    }

    #[test]
    fn test_incremental_export_removes_deleted_rows() {
        let tmp = tempfile::tempdir().unwrap();
        let db_path = tmp.path().join("test.db");
        let out_dir = tmp.path().join("parquet_out");

        let db = DivergenceDatabase::open(&db_path).unwrap();
        db.record_schedule_divergence(&sample_divergence("4x", 100, 0)).unwrap();
        db.record_schedule_divergence(&sample_divergence("4x", 200, 0)).unwrap();
        db.record_schedule_block_coverage(&sample_coverage("4x", 100)).unwrap();
        db.record_schedule_block_coverage(&sample_coverage("4x", 200)).unwrap();
        drop(db);

        export_sqlite_to_parquet(&db_path, &out_dir, 1000, 100_000, true).unwrap();

        let db = DivergenceDatabase::open(&db_path).unwrap();
        db.delete_schedule_divergences_in_block_range(100, 100).unwrap();
        db.delete_schedule_block_coverage_in_block_range(100, 100).unwrap();
        drop(db);

        let stats = export_sqlite_to_parquet(&db_path, &out_dir, 1000, 100_000, true).unwrap();
        assert_eq!(stats.hot_rows, 1);
        assert_eq!(stats.coverage_rows, 1);

        let hot_dir = out_dir.join("divergences_hot");
        let coverage_dir = out_dir.join("block_coverage");
        assert_eq!(count_parquet_rows_recursive(&hot_dir), 1);
        assert_eq!(count_parquet_rows_recursive(&coverage_dir), 1);
        assert_eq!(parquet_u64_values_recursive(&hot_dir, "block_number"), vec![200]);
        assert_eq!(parquet_u64_values_recursive(&coverage_dir, "block_number"), vec![200]);
    }

    #[test]
    fn test_full_refresh_cleans_old_data() {
        let tmp = tempfile::tempdir().unwrap();
        let db_path = tmp.path().join("test.db");
        let out_dir = tmp.path().join("parquet_out");

        let db = DivergenceDatabase::open(&db_path).unwrap();
        db.record_schedule_divergence(&sample_divergence("4x", 100, 0)).unwrap();
        db.record_schedule_block_coverage(&sample_coverage("4x", 100)).unwrap();
        drop(db);

        // Initial export.
        export_sqlite_to_parquet(&db_path, &out_dir, 1000, 100_000, false).unwrap();

        // Full refresh re-exports everything, no duplicates.
        let stats = export_sqlite_to_parquet(&db_path, &out_dir, 1000, 100_000, false).unwrap();
        assert_eq!(stats.hot_rows, 1);
        assert_eq!(stats.coverage_rows, 1);

        let hot_count = count_parquet_rows_recursive(&out_dir.join("divergences_hot"));
        assert_eq!(hot_count, 1);
    }

    #[test]
    fn test_empty_database_exports_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let db_path = tmp.path().join("test.db");
        let out_dir = tmp.path().join("parquet_out");

        let _db = DivergenceDatabase::open(&db_path).unwrap();
        drop(_db);

        let stats = export_sqlite_to_parquet(&db_path, &out_dir, 1000, 100_000, false).unwrap();
        assert_eq!(stats.hot_rows, 0);
        assert_eq!(stats.coverage_rows, 0);
        assert_eq!(stats.artifact_rows, 0);
        assert!(stats.schedules.is_empty());
    }

    #[test]
    fn test_hex_blob_encoding() {
        assert_eq!(hex_blob(vec![0xde, 0xad, 0xbe, 0xef]), "0xdeadbeef");
        assert_eq!(hex_blob(vec![]), "0x");
        assert_eq!(hex_blob(vec![0x00, 0xff]), "0x00ff");
    }

    #[test]
    fn test_block_bucket_calculation() {
        assert_eq!(block_bucket(0, 100_000), 0);
        assert_eq!(block_bucket(99_999, 100_000), 0);
        assert_eq!(block_bucket(100_000, 100_000), 100_000);
        assert_eq!(block_bucket(250_000, 100_000), 200_000);
    }

    #[test]
    fn test_multiple_schedules() {
        let tmp = tempfile::tempdir().unwrap();
        let db_path = tmp.path().join("test.db");
        let out_dir = tmp.path().join("parquet_out");

        let db = DivergenceDatabase::open(&db_path).unwrap();
        db.record_schedule_divergence(&sample_divergence("4x", 100, 0)).unwrap();
        db.record_schedule_divergence(&sample_divergence("8x", 100, 0)).unwrap();
        db.record_schedule_block_coverage(&sample_coverage("4x", 100)).unwrap();
        db.record_schedule_block_coverage(&sample_coverage("8x", 100)).unwrap();
        drop(db);

        let stats = export_sqlite_to_parquet(&db_path, &out_dir, 1000, 100_000, false).unwrap();

        assert_eq!(stats.schedules, vec!["4x", "8x"]);
        assert_eq!(stats.hot_rows, 2);
        assert_eq!(stats.coverage_rows, 2);

        // Each schedule should have its own partition directory.
        assert!(out_dir.join("divergences_hot/schedule_name=4x").exists());
        assert!(out_dir.join("divergences_hot/schedule_name=8x").exists());
    }

    #[test]
    fn test_schedule_partition_encoding_avoids_collisions() {
        assert_eq!(encode_partition_value("foo/bar"), "foo~2Fbar");
        assert_eq!(encode_partition_value("foo=bar"), "foo~3Dbar");
        assert_eq!(encode_partition_value("foo_bar"), "foo_bar");

        let tmp = tempfile::tempdir().unwrap();
        let db_path = tmp.path().join("test.db");
        let out_dir = tmp.path().join("parquet_out");

        let db = DivergenceDatabase::open(&db_path).unwrap();
        db.record_schedule_divergence(&sample_divergence("foo/bar", 100, 0)).unwrap();
        db.record_schedule_divergence(&sample_divergence("foo_bar", 100, 1)).unwrap();
        drop(db);

        export_sqlite_to_parquet(&db_path, &out_dir, 1000, 100_000, false).unwrap();

        assert!(out_dir.join("divergences_hot/schedule_name=foo~2Fbar").exists());
        assert!(out_dir.join("divergences_hot/schedule_name=foo_bar").exists());
        assert_eq!(count_parquet_rows_recursive(&out_dir.join("divergences_hot")), 2);
    }
}
