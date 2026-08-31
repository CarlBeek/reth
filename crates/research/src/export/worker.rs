//! The embedded export worker: drains the `SQLite` outbox and inserts rows into
//! `ClickHouse`, one item at a time.
//!
//! Per item the worker decodes the envelope, verifies its hash and the
//! manifest/config-hash invariants, converts to `ClickHouse` rows, then inserts in
//! a fixed order — run manifest, divergences, summaries, and **coverage last**.
//! Coverage is the remote completion marker, so a consumer that sees a coverage
//! row knows the block's other rows already landed. Only after coverage succeeds
//! is the outbox row marked `exported`.
//!
//! The `SQLite` mutex is never held across an HTTP await: every database access
//! goes through `spawn_blocking` and releases the lock before the worker
//! contacts `ClickHouse`.

use crate::{
    database::{DivergenceDatabase, OutboxItem},
    export::{
        clickhouse::{ClickHouseClient, ClickHouseError, DestinationTable},
        config::ExportConfig,
        model::{block_output_to_rows, AnalysisManifestV1, ExportEnvelopeV1, RunRow},
    },
};
use alloy_primitives::keccak256;
use serde::Serialize;
use std::time::{SystemTime, UNIX_EPOCH};
use thiserror::Error;
use tokio::{
    sync::{mpsc, watch},
    task::JoinSet,
};
use tracing::{debug, error, warn};

/// How often the worker prunes old exported audit rows.
const PRUNE_INTERVAL_SECS: u64 = 3600;

/// Cap on the backoff exponent before shifting, to avoid overflow.
const MAX_BACKOFF_EXPONENT: u32 = 20;

/// Fatal errors that stop the worker (and, via the fatal channel, the process).
/// Transient `ClickHouse` failures never reach here — they stay in the outbox
/// retry loop.
#[derive(Debug, Error)]
pub enum ExportError {
    /// Pending outbox bytes exceeded the configured ceiling. The worker stops
    /// accepting further work rather than growing the outbox without bound.
    #[error("export backlog limit exceeded: {pending_bytes} pending bytes (limit {limit})")]
    BacklogLimitExceeded {
        /// Observed pending bytes.
        pending_bytes: u64,
        /// Configured ceiling.
        limit: u64,
    },
    /// The startup `DESCRIBE TABLE` schema check failed.
    #[error("ClickHouse schema check failed: {0}")]
    SchemaCheck(String),
    /// The HTTP client could not be constructed (e.g. bad CA cert).
    #[error("ClickHouse client init failed: {0}")]
    ClientInit(String),
    /// A blocking database task panicked or was cancelled.
    #[error("export worker runtime error: {0}")]
    Runtime(String),
    /// An underlying database error.
    #[error(transparent)]
    Database(#[from] crate::database::DatabaseError),
}

/// Sink abstraction over the `ClickHouse` client, so the worker's control flow can
/// be tested without a live server.
pub trait ClickHouseSink: Send + Sync {
    /// Insert newline-delimited `JSONEachRow` rows into one table.
    fn insert_rows(
        &self,
        table: DestinationTable,
        body: String,
        dedup_token: String,
    ) -> impl std::future::Future<Output = Result<(), ClickHouseError>> + Send;
}

impl ClickHouseSink for ClickHouseClient {
    async fn insert_rows(
        &self,
        table: DestinationTable,
        body: String,
        dedup_token: String,
    ) -> Result<(), ClickHouseError> {
        self.post_rows(table, body, dedup_token).await
    }
}

/// Build the `ClickHouse` client, run the startup schema check, then drain the
/// outbox until shutdown. A fatal error is reported on `fatal_tx` (so the ExEx
/// can stop the process) and also returned.
pub async fn run_export_worker(
    db: DivergenceDatabase,
    config: ExportConfig,
    shutdown: watch::Receiver<bool>,
    fatal_tx: mpsc::Sender<ExportError>,
) -> Result<(), ExportError> {
    let client = match ClickHouseClient::new(&config) {
        Ok(client) => client,
        Err(e) => {
            let err = ExportError::ClientInit(e.to_string());
            error!(target: "exex::research::export", %err, "export client init failed");
            let _ = fatal_tx.send(ExportError::ClientInit(e.to_string())).await;
            return Err(err);
        }
    };

    if let Err(e) = client.check_schema().await {
        let err = ExportError::SchemaCheck(e.to_string());
        error!(target: "exex::research::export", %err, "ClickHouse schema check failed");
        let _ = fatal_tx.send(ExportError::SchemaCheck(e.to_string())).await;
        return Err(err);
    }

    run_loop(db, config, client, shutdown, fatal_tx).await
}

/// The drain loop, generic over the sink for testability.
async fn run_loop<S>(
    db: DivergenceDatabase,
    config: ExportConfig,
    sink: S,
    mut shutdown: watch::Receiver<bool>,
    fatal_tx: mpsc::Sender<ExportError>,
) -> Result<(), ExportError>
where
    S: ClickHouseSink + Clone + Send + Sync + 'static,
{
    let mut last_prune = now_secs();

    loop {
        if *shutdown.borrow() {
            break;
        }

        let now = now_secs();
        let limit = config.export_batch_items;
        let items = run_db(&db, move |db| db.next_due_exports(now, limit)).await?;

        if items.is_empty() {
            // Nothing due — wait for the poll interval or an early shutdown.
            tokio::select! {
                _ = shutdown.changed() => {},
                _ = tokio::time::sleep(config.poll_interval) => {},
            }
            continue;
        }

        export_batch(&db, &config, &sink, items, now_secs()).await?;

        // Backlog protection: fail loudly rather than grow the outbox forever.
        let stats = run_db(&db, move |db| db.export_backlog_stats(now_secs())).await?;
        debug!(
            target: "exex::research::export",
            pending = stats.pending_count,
            pending_bytes = stats.pending_bytes,
            oldest_age_secs = stats.oldest_pending_age_secs,
            blocked = stats.blocked_count,
            "export backlog"
        );
        if stats.pending_bytes > config.max_pending_bytes {
            let err = ExportError::BacklogLimitExceeded {
                pending_bytes: stats.pending_bytes,
                limit: config.max_pending_bytes,
            };
            error!(target: "exex::research::export", %err, "export backlog limit exceeded; stopping");
            let _ = fatal_tx
                .send(ExportError::BacklogLimitExceeded {
                    pending_bytes: stats.pending_bytes,
                    limit: config.max_pending_bytes,
                })
                .await;
            return Err(err);
        }

        // Periodically prune exported audit rows.
        let now = now_secs();
        if now.saturating_sub(last_prune) >= PRUNE_INTERVAL_SECS {
            let cutoff = now.saturating_sub(config.exported_retention.as_secs());
            if let Ok(pruned) = run_db(&db, move |db| db.prune_exported_before(cutoff)).await &&
                pruned > 0
            {
                debug!(target: "exex::research::export", pruned, "pruned exported outbox rows");
            }
            last_prune = now;
        }
    }

    Ok(())
}

/// Per-item failure: either retry later or permanently block.
#[derive(Debug)]
enum ItemError {
    /// Transient/auth failure or a recoverable DB error — retry with backoff.
    Retry(String),
    /// Permanent failure (corrupt payload, missing manifest, oversized row,
    /// permanent `ClickHouse` error) — block and move on.
    Block(String),
}

/// Pre-serialized rows for one outbox item, ready to group into a batch. Produced
/// by [`decode_item`]; the per-row `max_single_row_bytes` check has already passed.
struct DecodedRows {
    export_id: String,
    config_hash: String,
    run_line: String,
    divergence_lines: Vec<String>,
    tx_gas_result_lines: Vec<String>,
    summary_lines: Vec<String>,
    coverage_line: String,
}

/// Rows from many items grouped by destination table, for batched insertion.
struct GroupedBatch {
    run_lines: Vec<String>,
    divergence_lines: Vec<String>,
    tx_gas_result_lines: Vec<String>,
    summary_lines: Vec<String>,
    coverage_lines: Vec<String>,
    ids: Vec<String>,
}

/// Decode, validate, convert, and serialize one outbox item's rows. Permanent
/// problems (bad payload, missing manifest, oversized row) → `Block`; a transient
/// manifest-lookup failure → `Retry`. Does not contact `ClickHouse`.
async fn decode_item(
    db: &DivergenceDatabase,
    config: &ExportConfig,
    item: &OutboxItem,
    now: u64,
) -> Result<DecodedRows, ItemError> {
    let envelope = ExportEnvelopeV1::decode(&item.payload_zstd, item.payload_hash)
        .map_err(|e| ItemError::Block(format!("payload decode/verify failed: {e}")))?;
    if envelope.analysis_config_hash != item.analysis_config_hash {
        return Err(ItemError::Block(format!(
            "envelope config hash {} disagrees with outbox {}",
            envelope.analysis_config_hash, item.analysis_config_hash
        )));
    }

    let ach = item.analysis_config_hash.clone();
    let record = run_db(db, move |db| db.analysis_manifest(&ach))
        .await
        .map_err(|e| ItemError::Retry(format!("manifest lookup failed: {e}")))?
        .ok_or_else(|| {
            ItemError::Block(format!("no stored manifest for {}", item.analysis_config_hash))
        })?;

    let manifest: AnalysisManifestV1 = serde_json::from_str(&record.manifest_json)
        .map_err(|e| ItemError::Block(format!("stored manifest JSON is invalid: {e}")))?;
    let recomputed = manifest
        .analysis_config_hash()
        .map_err(|e| ItemError::Block(format!("manifest hash failed: {e}")))?;
    if recomputed != item.analysis_config_hash {
        return Err(ItemError::Block(format!(
            "stored manifest hash {recomputed} disagrees with outbox {}",
            item.analysis_config_hash
        )));
    }

    let rows = block_output_to_rows(&envelope.output, &manifest, &item.analysis_config_hash, now)
        .map_err(|e| ItemError::Block(format!("row conversion failed: {e}")))?;
    let run_row = RunRow::new(&manifest, &item.analysis_config_hash, &record.manifest_json, now);

    let run_line = serialize_row(&run_row)?;
    let divergence_lines = serialize_rows(&rows.divergences)?;
    let tx_gas_result_lines = serialize_rows(&rows.tx_gas_results)?;
    let summary_lines = serialize_rows(&rows.summaries)?;
    let coverage_line = serialize_row(&rows.coverage)?;

    for line in std::iter::once(&run_line)
        .chain(&divergence_lines)
        .chain(&tx_gas_result_lines)
        .chain(&summary_lines)
        .chain(std::iter::once(&coverage_line))
    {
        if line.len() > config.max_single_row_bytes {
            return Err(ItemError::Block(format!(
                "row exceeds max_single_row_bytes ({} > {})",
                line.len(),
                config.max_single_row_bytes
            )));
        }
    }

    Ok(DecodedRows {
        export_id: item.export_id.clone(),
        config_hash: item.analysis_config_hash.clone(),
        run_line,
        divergence_lines,
        tx_gas_result_lines,
        summary_lines,
        coverage_line,
    })
}

/// Per-item export path (the fallback used when a batch hits a permanent error):
/// decode one item and insert its tables in order — run → divergences →
/// tx gas results → summaries → coverage (last).
async fn process_item<S: ClickHouseSink>(
    db: &DivergenceDatabase,
    config: &ExportConfig,
    sink: &S,
    item: &OutboxItem,
    now: u64,
) -> Result<(), ItemError> {
    let d = decode_item(db, config, item, now).await?;
    insert_serialized(sink, config, DestinationTable::Run, &[d.run_line], &item.export_id).await?;
    insert_serialized(
        sink,
        config,
        DestinationTable::Divergence,
        &d.divergence_lines,
        &item.export_id,
    )
    .await?;
    insert_serialized(
        sink,
        config,
        DestinationTable::TxGasResult,
        &d.tx_gas_result_lines,
        &item.export_id,
    )
    .await?;
    insert_serialized(sink, config, DestinationTable::Summary, &d.summary_lines, &item.export_id)
        .await?;
    insert_serialized(
        sink,
        config,
        DestinationTable::Coverage,
        &[d.coverage_line],
        &item.export_id,
    )
    .await?;
    Ok(())
}

/// Group many decoded items' rows by destination table. The run row is deduped to
/// one per distinct `analysis_config_hash` (it is identical across a dataset's items).
fn group_decoded(items: Vec<DecodedRows>) -> GroupedBatch {
    let mut g = GroupedBatch {
        run_lines: Vec::new(),
        divergence_lines: Vec::new(),
        tx_gas_result_lines: Vec::new(),
        summary_lines: Vec::new(),
        coverage_lines: Vec::new(),
        ids: Vec::new(),
    };
    let mut seen_run = std::collections::HashSet::new();
    for it in items {
        if seen_run.insert(it.config_hash.clone()) {
            g.run_lines.push(it.run_line);
        }
        g.divergence_lines.extend(it.divergence_lines);
        g.tx_gas_result_lines.extend(it.tx_gas_result_lines);
        g.summary_lines.extend(it.summary_lines);
        g.coverage_lines.push(it.coverage_line);
        g.ids.push(it.export_id);
    }
    g
}

/// Split serialized rows into chunks bounded by `max_batch_rows`/`max_batch_bytes`,
/// returning the newline-delimited `JSONEachRow` chunk bodies. Empty input → empty.
fn chunk_lines(config: &ExportConfig, lines: &[String]) -> Vec<String> {
    let mut chunks = Vec::new();
    let mut chunk = String::new();
    let mut chunk_rows = 0usize;
    for row in lines {
        let would_overflow = chunk_rows >= config.max_batch_rows ||
            (!chunk.is_empty() && chunk.len() + 1 + row.len() > config.max_batch_bytes);
        if chunk_rows > 0 && would_overflow {
            chunks.push(std::mem::take(&mut chunk));
            chunk_rows = 0;
        }
        if !chunk.is_empty() {
            chunk.push('\n');
        }
        chunk.push_str(row);
        chunk_rows += 1;
    }
    if chunk_rows > 0 {
        chunks.push(chunk);
    }
    chunks
}

/// Map a `ClickHouse` failure to a per-item outcome (retry vs permanent block).
fn map_ch_err(e: ClickHouseError) -> ItemError {
    if e.is_retryable() {
        ItemError::Retry(e.to_string())
    } else {
        ItemError::Block(e.to_string())
    }
}

/// Insert one table's rows as chunked `JSONEachRow` batches with up to
/// `export_insert_concurrency` requests in flight. The dedup token is a content
/// hash, so an identical chunk re-sent on retry is deduplicated by `ClickHouse`,
/// and `ReplacingMergeTree` collapses any survivors. Returns on the first error,
/// aborting outstanding requests.
async fn insert_lines_concurrent<S>(
    sink: &S,
    config: &ExportConfig,
    table: DestinationTable,
    lines: &[String],
) -> Result<(), ItemError>
where
    S: ClickHouseSink + Clone + Send + Sync + 'static,
{
    let chunks = chunk_lines(config, lines);
    if chunks.is_empty() {
        return Ok(());
    }
    let mut set: JoinSet<Result<(), ItemError>> = JoinSet::new();
    let mut iter = chunks.into_iter();
    for _ in 0..config.export_insert_concurrency {
        let Some(body) = iter.next() else { break };
        let s = sink.clone();
        set.spawn(async move {
            let token = format!("{}:{:x}", table.name(), keccak256(body.as_bytes()));
            s.insert_rows(table, body, token).await.map_err(map_ch_err)
        });
    }
    while let Some(joined) = set.join_next().await {
        match joined {
            Ok(Ok(())) => {}
            Ok(Err(e)) => return Err(e),
            Err(e) => return Err(ItemError::Retry(format!("insert task join error: {e}"))),
        }
        if let Some(body) = iter.next() {
            let s = sink.clone();
            set.spawn(async move {
                let token = format!("{}:{:x}", table.name(), keccak256(body.as_bytes()));
                s.insert_rows(table, body, token).await.map_err(map_ch_err)
            });
        }
    }
    Ok(())
}

/// Export one drained batch end to end: partition (bad items blocked/retried
/// individually) → group by table → Phase 1 inserts run/divergence/summary
/// concurrently, then a barrier, then Phase 2 inserts coverage → mark the batch
/// `exported`. A transient batch error retries the whole batch; a permanent one
/// falls back to per-item isolation via [`process_item`].
async fn export_batch<S>(
    db: &DivergenceDatabase,
    config: &ExportConfig,
    sink: &S,
    items: Vec<OutboxItem>,
    now: u64,
) -> Result<(), ExportError>
where
    S: ClickHouseSink + Clone + Send + Sync + 'static,
{
    let mut good = Vec::new();
    let mut max_attempts = 0u32;
    for item in &items {
        max_attempts = max_attempts.max(item.attempts);
        match decode_item(db, config, item, now).await {
            Ok(rows) => good.push(rows),
            Err(ItemError::Block(msg)) => {
                let id = item.export_id.clone();
                run_db(db, move |db| db.mark_export_blocked(&id, &msg)).await?;
            }
            Err(ItemError::Retry(msg)) => {
                let attempts = item.attempts.saturating_add(1);
                let next = now.saturating_add(retry_delay_secs(config, attempts));
                let id = item.export_id.clone();
                run_db(db, move |db| db.mark_export_retry(&id, attempts, next, &msg)).await?;
            }
        }
    }
    if good.is_empty() {
        return Ok(());
    }

    let batch = group_decoded(good);
    let ids = batch.ids.clone();

    // Phase 1: everything except coverage (concurrent), then a barrier.
    let phase1 = async {
        insert_lines_concurrent(sink, config, DestinationTable::Run, &batch.run_lines).await?;
        insert_lines_concurrent(
            sink,
            config,
            DestinationTable::Divergence,
            &batch.divergence_lines,
        )
        .await?;
        insert_lines_concurrent(
            sink,
            config,
            DestinationTable::TxGasResult,
            &batch.tx_gas_result_lines,
        )
        .await?;
        insert_lines_concurrent(sink, config, DestinationTable::Summary, &batch.summary_lines).await
    };
    // Phase 2: coverage last — only once every other row for the batch landed.
    let outcome = match phase1.await {
        Ok(()) => {
            insert_lines_concurrent(sink, config, DestinationTable::Coverage, &batch.coverage_lines)
                .await
        }
        Err(e) => Err(e),
    };

    match outcome {
        Ok(()) => {
            run_db(db, move |db| db.mark_exported_batch(&ids, now_secs())).await?;
        }
        Err(ItemError::Retry(msg)) => {
            let next =
                now_secs().saturating_add(retry_delay_secs(config, max_attempts.saturating_add(1)));
            warn!(
                target: "exex::research::export",
                error = %msg, n = ids.len(),
                "export batch failed transiently; scheduling retry"
            );
            run_db(db, move |db| db.mark_export_retry_batch(&ids, next, &msg)).await?;
        }
        Err(ItemError::Block(msg)) => {
            warn!(
                target: "exex::research::export",
                error = %msg, n = ids.len(),
                "permanent batch error; isolating per item"
            );
            for item in items.iter().filter(|i| ids.contains(&i.export_id)) {
                match process_item(db, config, sink, item, now_secs()).await {
                    Ok(()) => {
                        let id = item.export_id.clone();
                        run_db(db, move |db| db.mark_exported(&id, now_secs())).await?;
                    }
                    Err(ItemError::Block(m)) => {
                        let id = item.export_id.clone();
                        run_db(db, move |db| db.mark_export_blocked(&id, &m)).await?;
                    }
                    Err(ItemError::Retry(m)) => {
                        let attempts = item.attempts.saturating_add(1);
                        let next = now_secs().saturating_add(retry_delay_secs(config, attempts));
                        let id = item.export_id.clone();
                        run_db(db, move |db| db.mark_export_retry(&id, attempts, next, &m)).await?;
                    }
                }
            }
        }
    }
    Ok(())
}

/// Split pre-serialized rows into batches bounded by `max_batch_rows` and
/// `max_batch_bytes`, and insert each. Empty input inserts nothing. A single row
/// over `max_single_row_bytes` is a permanent block.
async fn insert_serialized<S: ClickHouseSink>(
    sink: &S,
    config: &ExportConfig,
    table: DestinationTable,
    rows: &[String],
    export_id: &str,
) -> Result<(), ItemError> {
    if rows.is_empty() {
        return Ok(());
    }

    let mut chunk = String::new();
    let mut chunk_rows = 0usize;
    let mut chunk_index = 0usize;

    for row in rows {
        if row.len() > config.max_single_row_bytes {
            return Err(ItemError::Block(format!(
                "{} row exceeds max_single_row_bytes ({} > {})",
                table.name(),
                row.len(),
                config.max_single_row_bytes
            )));
        }
        let would_overflow = chunk_rows >= config.max_batch_rows ||
            (!chunk.is_empty() && chunk.len() + 1 + row.len() > config.max_batch_bytes);
        if chunk_rows > 0 && would_overflow {
            flush(sink, table, std::mem::take(&mut chunk), export_id, chunk_index).await?;
            chunk_index += 1;
            chunk_rows = 0;
        }
        if !chunk.is_empty() {
            chunk.push('\n');
        }
        chunk.push_str(row);
        chunk_rows += 1;
    }
    if chunk_rows > 0 {
        flush(sink, table, chunk, export_id, chunk_index).await?;
    }
    Ok(())
}

async fn flush<S: ClickHouseSink>(
    sink: &S,
    table: DestinationTable,
    body: String,
    export_id: &str,
    chunk_index: usize,
) -> Result<(), ItemError> {
    let token = format!("{export_id}:{}:{chunk_index}", table.name());
    sink.insert_rows(table, body, token).await.map_err(|e| {
        if e.is_retryable() {
            ItemError::Retry(e.to_string())
        } else {
            ItemError::Block(e.to_string())
        }
    })
}

fn serialize_row<T: Serialize>(row: &T) -> Result<String, ItemError> {
    serde_json::to_string(row)
        .map_err(|e| ItemError::Block(format!("row serialization failed: {e}")))
}

fn serialize_rows<T: Serialize>(rows: &[T]) -> Result<Vec<String>, ItemError> {
    rows.iter().map(serialize_row).collect()
}

/// Exponential backoff with bounded jitter: `min(retry_max, retry_initial *
/// 2^attempts) + jitter`, in whole seconds (≥1).
fn retry_delay_secs(config: &ExportConfig, attempts: u32) -> u64 {
    let exponent = attempts.min(MAX_BACKOFF_EXPONENT);
    let base_ms = config.retry_initial.as_millis() as u64;
    let max_ms = config.retry_max.as_millis() as u64;
    let backoff_ms = base_ms.saturating_mul(1u64 << exponent).min(max_ms);
    let jitter_ms = pseudo_jitter(backoff_ms / 4 + 1);
    let total_ms = backoff_ms.saturating_add(jitter_ms);
    (total_ms / 1000).max(1)
}

/// Cheap non-cryptographic jitter derived from the wall clock, avoiding a `rand`
/// dependency. Bounded to `[0, bound)`.
fn pseudo_jitter(bound: u64) -> u64 {
    if bound == 0 {
        return 0;
    }
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.subsec_nanos()).unwrap_or(0);
    u64::from(nanos) % bound
}

fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// Run a blocking database closure off the async runtime, mapping a join failure
/// to [`ExportError::Runtime`] and propagating the inner database error.
async fn run_db<T, F>(db: &DivergenceDatabase, f: F) -> Result<T, ExportError>
where
    F: FnOnce(&DivergenceDatabase) -> Result<T, crate::database::DatabaseError> + Send + 'static,
    T: Send + 'static,
{
    let db = db.clone();
    tokio::task::spawn_blocking(move || f(&db))
        .await
        .map_err(|e| ExportError::Runtime(e.to_string()))?
        .map_err(ExportError::from)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        database::{
            BlockCoverageRow, BlockOutput, BlockSummaryRow, DivergenceRow, DrillInRecord,
            EncodedExportEnvelope,
        },
        divergence::AggregateClass,
        export::{
            config::ExportConfig,
            model::{export_id, normalize_gas_tiers, AnalysisManifestV1},
        },
        schedule::{Eip2780Schedule, ScheduleRegistry},
    };
    use alloy_primitives::{Address, B256};
    use std::sync::{Arc, Mutex};

    /// Callback run *during* the fake sink's insert await (used to prove the
    /// `SQLite` lock is not held while the worker is contacting the sink).
    type InsertHook = Arc<Mutex<Option<Box<dyn FnMut() + Send>>>>;

    /// A scriptable in-memory sink that records insert order and bodies.
    #[derive(Clone)]
    struct FakeSink {
        log: Arc<Mutex<Vec<DestinationTable>>>,
        // Per-table failure to inject, consulted on each insert.
        fail: Arc<Mutex<Option<(DestinationTable, ClickHouseError)>>>,
        on_insert: InsertHook,
    }

    impl FakeSink {
        fn new() -> Self {
            Self {
                log: Arc::new(Mutex::new(Vec::new())),
                fail: Arc::new(Mutex::new(None)),
                on_insert: Arc::new(Mutex::new(None)),
            }
        }
        fn tables(&self) -> Vec<DestinationTable> {
            self.log.lock().unwrap().clone()
        }
        fn fail_on(&self, table: DestinationTable, err: ClickHouseError) {
            *self.fail.lock().unwrap() = Some((table, err));
        }
    }

    impl ClickHouseSink for FakeSink {
        async fn insert_rows(
            &self,
            table: DestinationTable,
            _body: String,
            _dedup_token: String,
        ) -> Result<(), ClickHouseError> {
            if let Some(cb) = self.on_insert.lock().unwrap().as_mut() {
                cb();
            }
            self.log.lock().unwrap().push(table);
            let mut guard = self.fail.lock().unwrap();
            if let Some((failing, err)) = guard.as_ref() &&
                *failing == table
            {
                let cloned = match err {
                    ClickHouseError::Transient(m) => ClickHouseError::Transient(m.clone()),
                    ClickHouseError::Permanent(m) => ClickHouseError::Permanent(m.clone()),
                    ClickHouseError::Auth(m) => ClickHouseError::Auth(m.clone()),
                };
                *guard = None;
                return Err(cloned);
            }
            Ok(())
        }
    }

    fn test_config() -> ExportConfig {
        ExportConfig::insecure_for_test("http://127.0.0.1:1", "default", "x")
    }

    fn coverage(block: u64, drill: u32) -> BlockCoverageRow {
        BlockCoverageRow {
            schedule_name: "eip-2780".to_string(),
            schedule_config_hash: "0xcfg".to_string(),
            block_number: block,
            block_hash: B256::repeat_byte(0xa1),
            parent_hash: B256::repeat_byte(0xa0),
            timestamp: 1_700_000_000,
            tx_count: 1,
            tx_count_unchanged: 0,
            tx_count_gas_only: 0,
            tx_count_stored: drill,
            block_gas_used: 21000,
            block_gas_limit: 30_000_000,
            block_base_fee_per_gas: Some(1_000_000_000),
        }
    }

    fn summary(block: u64) -> BlockSummaryRow {
        BlockSummaryRow {
            schedule_name: "eip-2780".to_string(),
            block_number: block,
            class: AggregateClass::GasOnly,
            tx_count: 1,
            gas_delta_sum: Some(1),
            gas_delta_sum_sq: Some(1),
            gas_delta_min: Some(1),
            gas_delta_max: Some(1),
            gas_delta_log2_hist: None,
            opcode_totals: vec![],
            state_gas_sum: None,
            state_gas_spillover_sum: None,
            multiplier_log2_hist: None,
            tx_count_creation: None,
            tx_count_authorization: None,
            tx_count_runtime_state: None,
            tx_count_no_state: None,
            cold_account_access_count: None,
            storage_drivers: None,
            account_drivers: None,
            tx_count_type_legacy: None,
            tx_count_type_access_list: None,
            tx_count_type_dynamic_fee: None,
            tx_count_type_blob: None,
            tx_count_type_set_code: None,
            tx_count_type_other: None,
            tx_count_simple_transfer: None,
            tx_count_contract_call: None,
            gas_delta_pct_hist: None,
            baseline_gas_used_sum: None,
        }
    }

    fn divergence(block: u64) -> DrillInRecord {
        DrillInRecord {
            divergence: DivergenceRow {
                schedule_name: "eip-2780".to_string(),
                schedule_config_hash: "0xcfg".to_string(),
                block_number: block,
                tx_index: 0,
                tx_hash: B256::repeat_byte(0xd1),
                timestamp: 1_700_000_000,
                sender: Address::repeat_byte(0x01),
                recipient: Some(Address::repeat_byte(0x02)),
                is_create: false,
                tx_gas_limit: 100_000,
                baseline_success: true,
                schedule_success: true,
                event_logs_changed: true,
                baseline_gas_used: 21000,
                schedule_gas_used: 21000,
                ..Default::default()
            },
            call_frames: vec![],
            opcode_counts: vec![],
            baseline_call_frames: vec![],
            baseline_opcode_counts: vec![],
            baseline_event_logs: vec![],
            schedule_event_logs: vec![],
        }
    }

    /// Seed the DB with one pending outbox item and its manifest. Returns the
    /// dataset hash and the populated `OutboxItem`.
    fn seed(db: &DivergenceDatabase, output: BlockOutput) -> (String, OutboxItem) {
        let mut reg = ScheduleRegistry::new();
        reg.register(Eip2780Schedule::new()).unwrap();
        let manifest = AnalysisManifestV1::build(
            &reg,
            normalize_gas_tiers(&[1]),
            Some(50),
            1,
            crate::database::SCHEMA_VERSION,
            "deadbeef",
        );
        let ach = manifest.analysis_config_hash().unwrap();
        let json = manifest.to_json().unwrap();
        db.upsert_analysis_manifest(&crate::database::AnalysisManifestRecord {
            analysis_config_hash: ach.clone(),
            schema_version: crate::database::SCHEMA_VERSION,
            chain_id: 1,
            producer_git_commit: "deadbeef".to_string(),
            replay_semantics: "canonical_pre_tx_state".to_string(),
            manifest_json: json,
        })
        .unwrap();

        let eid = export_id(&ach, &output.coverage.schedule_name, output.coverage.block_hash);
        let envelope = ExportEnvelopeV1::new(ach.clone(), output.clone());
        let encoded = envelope.encode().unwrap();
        let export = EncodedExportEnvelope {
            export_id: eid,
            analysis_config_hash: ach.clone(),
            schedule_config_hash: output.coverage.schedule_config_hash.clone(),
            payload_version: encoded.payload_version,
            payload_zstd: encoded.payload_zstd,
            payload_hash: encoded.payload_hash,
            payload_bytes: encoded.payload_bytes,
        };
        db.record_block_output_with_export(&output, &export).unwrap();
        let item = db.next_due_export(now_secs() + 1).unwrap().unwrap();
        (ach, item)
    }

    #[tokio::test]
    async fn process_item_inserts_in_order_and_skips_empty_batches() {
        let db = DivergenceDatabase::in_memory().unwrap();
        // One summary, zero divergences → divergence batch is skipped.
        let output = BlockOutput {
            coverage: coverage(10, 0),
            summaries: vec![summary(10)],
            drill_ins: vec![],
            recipients: vec![],
            tx_gas_results: vec![],
        };
        let (_ach, item) = seed(&db, output);
        let sink = FakeSink::new();
        let config = test_config();

        process_item(&db, &config, &sink, &item, now_secs()).await.unwrap();
        assert_eq!(
            sink.tables(),
            vec![DestinationTable::Run, DestinationTable::Summary, DestinationTable::Coverage]
        );
    }

    #[tokio::test]
    async fn process_item_inserts_divergences_before_coverage() {
        let db = DivergenceDatabase::in_memory().unwrap();
        let output = BlockOutput {
            coverage: coverage(11, 1),
            summaries: vec![summary(11)],
            drill_ins: vec![divergence(11)],
            recipients: vec![],
            tx_gas_results: vec![],
        };
        let (_ach, item) = seed(&db, output);
        let sink = FakeSink::new();
        let config = test_config();

        process_item(&db, &config, &sink, &item, now_secs()).await.unwrap();
        assert_eq!(
            sink.tables(),
            vec![
                DestinationTable::Run,
                DestinationTable::Divergence,
                DestinationTable::Summary,
                DestinationTable::Coverage,
            ]
        );
    }

    #[tokio::test]
    async fn coverage_transient_failure_is_retryable() {
        let db = DivergenceDatabase::in_memory().unwrap();
        let output = BlockOutput {
            coverage: coverage(12, 0),
            summaries: vec![],
            drill_ins: vec![],
            recipients: vec![],
            tx_gas_results: vec![],
        };
        let (_ach, item) = seed(&db, output);
        let sink = FakeSink::new();
        sink.fail_on(DestinationTable::Coverage, ClickHouseError::Transient("503".into()));
        let config = test_config();

        let err = process_item(&db, &config, &sink, &item, now_secs()).await.unwrap_err();
        assert!(matches!(err, ItemError::Retry(_)));
    }

    #[tokio::test]
    async fn auth_failure_is_retryable_not_blocked() {
        let db = DivergenceDatabase::in_memory().unwrap();
        let output = BlockOutput {
            coverage: coverage(13, 0),
            summaries: vec![],
            drill_ins: vec![],
            recipients: vec![],
            tx_gas_results: vec![],
        };
        let (_ach, item) = seed(&db, output);
        let sink = FakeSink::new();
        sink.fail_on(DestinationTable::Run, ClickHouseError::Auth("401".into()));
        let config = test_config();

        let err = process_item(&db, &config, &sink, &item, now_secs()).await.unwrap_err();
        assert!(matches!(err, ItemError::Retry(_)));
    }

    #[tokio::test]
    async fn oversized_row_is_permanently_blocked() {
        let db = DivergenceDatabase::in_memory().unwrap();
        let output = BlockOutput {
            coverage: coverage(14, 0),
            summaries: vec![],
            drill_ins: vec![],
            recipients: vec![],
            tx_gas_results: vec![],
        };
        let (_ach, item) = seed(&db, output);
        let sink = FakeSink::new();
        let mut config = test_config();
        config.max_single_row_bytes = 1; // every row is "oversized"

        let err = process_item(&db, &config, &sink, &item, now_secs()).await.unwrap_err();
        assert!(matches!(err, ItemError::Block(_)));
    }

    #[tokio::test]
    async fn sqlite_lock_not_held_while_sink_awaits() {
        let db = DivergenceDatabase::in_memory().unwrap();
        let output = BlockOutput {
            coverage: coverage(15, 0),
            summaries: vec![],
            drill_ins: vec![],
            recipients: vec![],
            tx_gas_results: vec![],
        };
        let (_ach, item) = seed(&db, output);
        let sink = FakeSink::new();
        // During each insert, perform a real DB read. If the worker held the
        // SQLite mutex across the await this would deadlock (same-thread re-lock).
        let probe = db.clone();
        let hits = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let hits2 = hits.clone();
        *sink.on_insert.lock().unwrap() = Some(Box::new(move || {
            let _ = probe.export_backlog_stats(0).unwrap();
            hits2.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }));
        let config = test_config();

        process_item(&db, &config, &sink, &item, now_secs()).await.unwrap();
        assert!(hits.load(std::sync::atomic::Ordering::SeqCst) >= 2);
    }

    #[tokio::test]
    async fn run_loop_exports_then_stops_on_shutdown() {
        let db = DivergenceDatabase::in_memory().unwrap();
        let output = BlockOutput {
            coverage: coverage(16, 0),
            summaries: vec![],
            drill_ins: vec![],
            recipients: vec![],
            tx_gas_results: vec![],
        };
        let (_ach, item) = seed(&db, output);
        let export_id = item.export_id.clone();
        let sink = FakeSink::new();
        let config = test_config();
        let (sh_tx, sh_rx) = watch::channel(false);
        let (fatal_tx, _fatal_rx) = mpsc::channel(4);

        let loop_db = db.clone();
        let handle =
            tokio::spawn(async move { run_loop(loop_db, config, sink, sh_rx, fatal_tx).await });

        // Wait until exported, then signal shutdown.
        for _ in 0..200 {
            if outbox_state(&db, &export_id) == Some("exported".to_string()) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        sh_tx.send(true).unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(5), handle)
            .await
            .expect("worker should stop")
            .unwrap()
            .unwrap();

        assert_eq!(outbox_state(&db, &export_id), Some("exported".to_string()));
    }

    #[tokio::test]
    async fn run_loop_leaves_item_pending_on_coverage_failure() {
        let db = DivergenceDatabase::in_memory().unwrap();
        let output = BlockOutput {
            coverage: coverage(17, 0),
            summaries: vec![],
            drill_ins: vec![],
            recipients: vec![],
            tx_gas_results: vec![],
        };
        let (_ach, item) = seed(&db, output);
        let export_id = item.export_id.clone();
        let sink = FakeSink::new();
        sink.fail_on(DestinationTable::Coverage, ClickHouseError::Transient("503".into()));
        let config = test_config();
        let (sh_tx, sh_rx) = watch::channel(false);
        let (fatal_tx, _fatal_rx) = mpsc::channel(4);

        let loop_db = db.clone();
        let handle =
            tokio::spawn(async move { run_loop(loop_db, config, sink, sh_rx, fatal_tx).await });

        for _ in 0..200 {
            if outbox_state(&db, &export_id) == Some("retry".to_string()) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        sh_tx.send(true).unwrap();
        let _ = tokio::time::timeout(std::time::Duration::from_secs(5), handle).await;

        // Not exported; left as retry for a later attempt.
        assert_eq!(outbox_state(&db, &export_id), Some("retry".to_string()));
    }

    #[tokio::test]
    async fn batched_insert_keeps_coverage_after_all_others() {
        let db = DivergenceDatabase::in_memory().unwrap();
        for b in 30..33u64 {
            let mut cov = coverage(b, 1);
            cov.block_hash = B256::repeat_byte(b as u8);
            let output = BlockOutput {
                coverage: cov,
                summaries: vec![summary(b)],
                drill_ins: vec![divergence(b)],
                recipients: vec![],
                tx_gas_results: vec![],
            };
            seed(&db, output);
        }
        let sink = FakeSink::new();
        let config = test_config();
        let items = db.next_due_exports(now_secs() + 1, 100).unwrap();
        assert_eq!(items.len(), 3);
        let ids: Vec<String> = items.iter().map(|i| i.export_id.clone()).collect();
        export_batch(&db, &config, &sink, items, now_secs()).await.unwrap();

        // Every non-coverage insert precedes every coverage insert (the barrier).
        let tables = sink.tables();
        let last_other = tables.iter().rposition(|t| *t != DestinationTable::Coverage);
        let first_cov = tables.iter().position(|t| *t == DestinationTable::Coverage);
        assert!(
            matches!((last_other, first_cov), (Some(l), Some(f)) if l < f),
            "coverage must come after all others: {tables:?}"
        );
        for id in &ids {
            assert_eq!(outbox_state(&db, id).as_deref(), Some("exported"));
        }
    }

    #[tokio::test]
    async fn transient_failure_retries_whole_batch_without_partial_coverage() {
        let db = DivergenceDatabase::in_memory().unwrap();
        for b in 40..42u64 {
            let mut cov = coverage(b, 1);
            cov.block_hash = B256::repeat_byte(b as u8);
            let output = BlockOutput {
                coverage: cov,
                summaries: vec![summary(b)],
                drill_ins: vec![divergence(b)],
                recipients: vec![],
                tx_gas_results: vec![],
            };
            seed(&db, output);
        }
        let sink = FakeSink::new();
        sink.fail_on(DestinationTable::Divergence, ClickHouseError::Transient("nope".into()));
        let config = test_config();
        let items = db.next_due_exports(now_secs() + 1, 100).unwrap();
        assert_eq!(items.len(), 2);
        let ids: Vec<String> = items.iter().map(|i| i.export_id.clone()).collect();
        export_batch(&db, &config, &sink, items, now_secs()).await.unwrap();

        // No coverage sent, and the whole batch is back to retry (no partial export).
        assert!(!sink.tables().contains(&DestinationTable::Coverage));
        for id in &ids {
            assert_eq!(outbox_state(&db, id).as_deref(), Some("retry"));
        }
    }

    fn outbox_state(db: &DivergenceDatabase, export_id: &str) -> Option<String> {
        db.export_state_for_test(export_id)
    }
}
