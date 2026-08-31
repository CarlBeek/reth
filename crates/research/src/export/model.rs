//! Export data model: deterministic dataset identity plus the `ClickHouse` row
//! contract.
//!
//! This module is the single source of truth for three things the rest of the
//! export pipeline depends on:
//!
//! 1. The [`AnalysisManifestV1`] — a deterministic description of the analysis configuration whose
//!    Keccak hash ([`AnalysisManifestV1::analysis_config_hash`]) is the dataset identity. The hash
//!    is stable across process restarts and registry ordering so retries and backfills collapse to
//!    the same logical dataset.
//! 2. The [`ExportEnvelopeV1`] — the versioned, ZSTD-compressed snapshot of a single block's
//!    [`BlockOutput`] that is written to the `SQLite` outbox and later replayed by the export
//!    worker.
//! 3. The `ClickHouse` row model ([`RunRow`], [`CoverageRow`], [`SummaryRow`],
//!    [`DivergenceExportRow`]) and the conversion from a `BlockOutput`. Column names and types here
//!    are the producer contract; the canonical DDL in `bin/reth-research/clickhouse/migrations`
//!    must match.
//!
//! Delivery to `ClickHouse` is at least once. Correctness comes from the
//! deterministic identities in this module combined with `ReplacingMergeTree`,
//! not from assuming any HTTP request runs exactly once.

use crate::{
    database::{
        BlockCoverageRow, BlockOutput, BlockSummaryRow, CallFrameRow, DivergenceRow,
        OpcodeCountRow, TxGasResultRow,
    },
    divergence::{AggregateClass, EventLog},
    schedule::{GasSchedule, ScheduleRegistry},
};
use alloy_primitives::{keccak256, Address, B256};
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Version of the [`AnalysisManifestV1`] envelope. Bump when the manifest
/// struct changes shape in a way that affects the serialized JSON / hash.
pub const MANIFEST_FORMAT_VERSION: u16 = 1;

/// Version of the [`ExportEnvelopeV1`] payload format stored in the outbox.
/// v2: the `BlockOutput` payload was reshaped for the F-series schema (3-fact
/// coverage, `class`-keyed summaries, full-parity divergence columns, and the
/// baseline/schedule trace-kind split).
/// v3: additive v11 `BlockSummaryRow` fields — the six EIP-2718
/// `tx_count_type_*` counts, `tx_count_simple_transfer` /
/// `tx_count_contract_call`, `gas_delta_pct_hist`, `baseline_gas_used_sum`
/// (all `serde(default)`, so v2 payloads still decode).
/// v4: additive v12 fields — `BlockOutput::tx_gas_results` (the unconditional
/// per-tx gas spine) and `BlockCoverageRow::block_base_fee_per_gas` (both
/// `serde(default)`, so v3 payloads still decode as empty / `None`).
pub const ENVELOPE_FORMAT_VERSION: u16 = 4;

/// Replay semantics tag baked into the manifest and every remote row. The
/// research pipeline replays each tx against canonical pre-tx state.
pub const REPLAY_SEMANTICS: &str = "canonical_pre_tx_state";

/// Domain tag mixed into [`export_id`] so the identity is scoped to this
/// payload kind and can't collide with a future payload type.
pub const PAYLOAD_KIND: &str = "block_output_v1";

/// Format tag for the divergence `trace_payload` blob.
pub const TRACE_FORMAT: &str = "research_drill_in_components_v1";

/// Version of the `trace_payload` format.
/// v2: added the baseline call-frame / opcode-count vectors (trace-kind split)
/// plus the F3 `to_address`/`code_address` and F9 failing-frame fields carried
/// by the now-`Serialize` [`CallFrameRow`].
pub const TRACE_FORMAT_VERSION: u16 = 2;

/// ZSTD compression level for the outbox payload. Level 3 is the standard
/// speed/ratio trade-off; the payload is JSON so it compresses well.
const ZSTD_LEVEL: i32 = 3;

/// Errors raised while encoding/decoding export payloads.
#[derive(Debug, Error)]
pub enum ExportModelError {
    /// JSON (de)serialization failure.
    #[error("export serialization error: {0}")]
    Json(#[from] serde_json::Error),
    /// ZSTD compression/decompression I/O failure.
    #[error("export compression error: {0}")]
    Compression(#[from] std::io::Error),
    /// The decoded payload's Keccak hash did not match the stored hash.
    #[error("export payload hash mismatch (corrupt or truncated outbox row)")]
    PayloadHashMismatch,
}

// ─────────────────────────────────────────────────────────────────────────────
// Dataset identity
// ─────────────────────────────────────────────────────────────────────────────

/// Deterministic description of an analysis configuration.
///
/// The Keccak hash of this struct's JSON serialization is the
/// `analysis_config_hash` — the stable dataset identity. Field order is fixed
/// and [`schedules`](Self::schedules) is sorted by name so the JSON, and hence
/// the hash, is deterministic regardless of registry iteration order.
///
/// Operational controls (polling intervals, backfill concurrency) are
/// deliberately excluded: they don't change a block's analytical result, so
/// they must not change the identity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AnalysisManifestV1 {
    /// Manifest envelope version ([`MANIFEST_FORMAT_VERSION`]).
    pub format_version: u16,
    /// Producer `SQLite` schema version the data was written under.
    pub producer_schema_version: u32,
    /// Full git SHA of the producer binary.
    pub producer_git_commit: String,
    /// Chain ID the analysis ran against.
    pub chain_id: u64,
    /// Replay semantics tag ([`REPLAY_SEMANTICS`]).
    pub replay_semantics: String,
    /// Normalized gas-limit-multiplier sweep tiers (clamped ≥1, sorted, deduped).
    pub gas_limit_multipliers: Vec<u64>,
    /// Drill-in retention cap, which changes retained output.
    pub max_divergences_per_block: Option<usize>,
    /// Per-schedule manifests, sorted by `name`.
    pub schedules: Vec<ScheduleManifestV1>,
}

/// Per-schedule entry within an [`AnalysisManifestV1`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScheduleManifestV1 {
    /// Schedule identifier (e.g. `eip-2780`).
    pub name: String,
    /// Schedule kind (`IntrinsicOnly` / `ExecutionOnly` / `Both` / `None`).
    pub kind: String,
    /// Human-readable description.
    pub description: String,
    /// Stable configuration fingerprint material.
    pub config_fingerprint: String,
    /// Keccak hash of the fingerprint, matching the value stored in
    /// `block_coverage.schedule_config_hash`.
    pub config_hash: String,
}

impl ScheduleManifestV1 {
    fn from_schedule(schedule: &dyn GasSchedule) -> Self {
        let config_fingerprint = schedule.config_fingerprint();
        let config_hash = format!("{:#x}", keccak256(config_fingerprint.as_bytes()));
        Self {
            name: schedule.name().to_string(),
            kind: format!("{:?}", schedule.kind()),
            description: schedule.description().to_string(),
            config_fingerprint,
            config_hash,
        }
    }
}

impl AnalysisManifestV1 {
    /// Build the manifest from the registry and the configuration inputs.
    ///
    /// `normalized_gas_tiers` must already be normalized (see
    /// [`normalize_gas_tiers`]); the caller normalizes exactly once and reuses
    /// the same vector for both this manifest and the analyzer so the identity
    /// matches the data actually produced.
    pub fn build(
        registry: &ScheduleRegistry,
        normalized_gas_tiers: Vec<u64>,
        max_divergences_per_block: Option<usize>,
        chain_id: u64,
        producer_schema_version: u32,
        producer_git_commit: impl Into<String>,
    ) -> Self {
        let mut schedules: Vec<ScheduleManifestV1> =
            registry.all().iter().map(|s| ScheduleManifestV1::from_schedule(s.as_ref())).collect();
        schedules.sort_by(|a, b| a.name.cmp(&b.name));

        Self {
            format_version: MANIFEST_FORMAT_VERSION,
            producer_schema_version,
            producer_git_commit: producer_git_commit.into(),
            chain_id,
            replay_semantics: REPLAY_SEMANTICS.to_string(),
            gas_limit_multipliers: normalized_gas_tiers,
            max_divergences_per_block,
            schedules,
        }
    }

    /// Canonical JSON serialization used both for the persisted
    /// `analysis_manifests.manifest_json` column and as the hash preimage.
    pub fn to_json(&self) -> Result<String, ExportModelError> {
        Ok(serde_json::to_string(self)?)
    }

    /// The dataset identity: `keccak256(manifest_json)` as a `0x`-prefixed hex
    /// string (66 chars, matching `ClickHouse` `FixedString(66)`).
    pub fn analysis_config_hash(&self) -> Result<String, ExportModelError> {
        Ok(format!("{:#x}", keccak256(self.to_json()?.as_bytes())))
    }
}

/// Normalize the gas-limit-multiplier tiers: clamp each to at least 1, sort
/// ascending, and deduplicate. An empty input normalizes to `[1]` (the
/// analyzer's single-tier default) so the manifest always reflects a concrete
/// tier set.
pub fn normalize_gas_tiers(tiers: &[u64]) -> Vec<u64> {
    let mut out: Vec<u64> = tiers.iter().map(|&t| t.max(1)).collect();
    out.sort_unstable();
    out.dedup();
    if out.is_empty() {
        out.push(1);
    }
    out
}

// ─────────────────────────────────────────────────────────────────────────────
// Deterministic row identities
// ─────────────────────────────────────────────────────────────────────────────

/// Length-prefixed framing so distinct field tuples can never produce the same
/// preimage by concatenation (e.g. `"a" + "bc"` vs `"ab" + "c"`). Each part is
/// emitted as its `u64` little-endian length followed by its bytes.
fn framed_keccak(parts: &[&[u8]]) -> String {
    let mut buf = Vec::new();
    for part in parts {
        buf.extend_from_slice(&(part.len() as u64).to_le_bytes());
        buf.extend_from_slice(part);
    }
    format!("{:#x}", keccak256(&buf))
}

/// Deterministic per-`(analysis config, schedule, block)` export identity.
pub fn export_id(analysis_config_hash: &str, schedule_name: &str, block_hash: B256) -> String {
    framed_keccak(&[
        analysis_config_hash.as_bytes(),
        schedule_name.as_bytes(),
        block_hash.as_slice(),
        PAYLOAD_KIND.as_bytes(),
    ])
}

/// Deterministic `gas_analysis_block_coverage.row_id`.
pub fn coverage_row_id(export_id: &str) -> String {
    framed_keccak(&[export_id.as_bytes(), b"coverage"])
}

/// Deterministic `gas_analysis_block_summary.row_id`.
pub fn summary_row_id(export_id: &str, class: AggregateClass) -> String {
    framed_keccak(&[export_id.as_bytes(), class.as_str().as_bytes(), b"summary"])
}

/// Deterministic `gas_analysis_divergence.row_id`.
pub fn divergence_row_id(export_id: &str, tx_index: u32, tx_hash: B256) -> String {
    framed_keccak(&[
        export_id.as_bytes(),
        &tx_index.to_le_bytes(),
        tx_hash.as_slice(),
        b"divergence",
    ])
}

/// Deterministic `gas_analysis_tx_gas_result.row_id`.
pub fn tx_gas_result_row_id(export_id: &str, tx_index: u32, tx_hash: B256) -> String {
    framed_keccak(&[
        export_id.as_bytes(),
        &tx_index.to_le_bytes(),
        tx_hash.as_slice(),
        b"tx_gas_result",
    ])
}

// ─────────────────────────────────────────────────────────────────────────────
// Export envelope
// ─────────────────────────────────────────────────────────────────────────────

/// Versioned snapshot of one block's output, plus capture metadata, written to
/// the `SQLite` outbox and replayed by the export worker.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExportEnvelopeV1 {
    /// Payload format version ([`ENVELOPE_FORMAT_VERSION`]).
    pub format_version: u16,
    /// Dataset identity this block belongs to.
    pub analysis_config_hash: String,
    /// The full per-block output.
    pub output: BlockOutput,
    /// Drill-in/opcode capture completeness metadata.
    pub capture: CaptureMetadataV1,
}

/// Metadata describing how complete the captured drill-in / opcode data is for
/// a block, so consumers can distinguish "no drill-ins" from "drill-ins
/// dropped by the retention cap".
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CaptureMetadataV1 {
    /// Drill-in count implied by the coverage bucket counts.
    pub expected_drill_in_count: u32,
    /// Drill-in records actually retained in the output.
    pub retained_drill_in_count: u32,
    /// Whether retention dropped some drill-ins (`retained < expected`).
    pub drill_ins_truncated: bool,
    /// Whether per-opcode capture was complete. `None` in v1 because the
    /// inspector's truncation flag is discarded before the drill-in is built;
    /// do not claim completeness until that flag is propagated.
    pub opcode_capture_complete: Option<bool>,
}

impl CaptureMetadataV1 {
    /// Derive capture metadata from a block's output. The expected drill-in
    /// count is the coverage `tx_count_stored` fact — every tx that produced a
    /// per-tx `divergences` (drill-in) row.
    pub const fn from_output(output: &BlockOutput) -> Self {
        let expected_drill_in_count = output.coverage.tx_count_stored;
        let retained_drill_in_count = output.drill_ins.len() as u32;
        Self {
            expected_drill_in_count,
            retained_drill_in_count,
            drill_ins_truncated: retained_drill_in_count < expected_drill_in_count,
            opcode_capture_complete: None,
        }
    }
}

/// The compressed, hashed result of encoding an [`ExportEnvelopeV1`]. Carries
/// everything the outbox row needs to store and later verify the payload.
#[derive(Debug, Clone)]
pub struct EncodedEnvelope {
    /// Payload format version.
    pub payload_version: u16,
    /// ZSTD-compressed JSON payload.
    pub payload_zstd: Vec<u8>,
    /// Keccak hash of the *uncompressed* JSON, for integrity verification.
    pub payload_hash: B256,
    /// Length of the uncompressed JSON in bytes.
    pub payload_bytes: usize,
}

impl ExportEnvelopeV1 {
    /// Build an envelope for a block, deriving capture metadata from it.
    pub fn new(analysis_config_hash: impl Into<String>, output: BlockOutput) -> Self {
        let capture = CaptureMetadataV1::from_output(&output);
        Self {
            format_version: ENVELOPE_FORMAT_VERSION,
            analysis_config_hash: analysis_config_hash.into(),
            output,
            capture,
        }
    }

    /// Serialize to JSON, hash the uncompressed bytes, then ZSTD-compress.
    ///
    /// This is intended to run on the database writer thread *before* it
    /// acquires the `SQLite` connection lock, keeping the lock hold time short.
    pub fn encode(&self) -> Result<EncodedEnvelope, ExportModelError> {
        let json = serde_json::to_vec(self)?;
        let payload_hash = keccak256(&json);
        let payload_bytes = json.len();
        let payload_zstd = zstd::stream::encode_all(json.as_slice(), ZSTD_LEVEL)?;
        Ok(EncodedEnvelope {
            payload_version: ENVELOPE_FORMAT_VERSION,
            payload_zstd,
            payload_hash,
            payload_bytes,
        })
    }

    /// Decompress, verify the payload hash, then deserialize. Returns
    /// [`ExportModelError::PayloadHashMismatch`] if the bytes were corrupted.
    pub fn decode(payload_zstd: &[u8], expected_hash: B256) -> Result<Self, ExportModelError> {
        let json = zstd::stream::decode_all(payload_zstd)?;
        if keccak256(&json) != expected_hash {
            return Err(ExportModelError::PayloadHashMismatch);
        }
        Ok(serde_json::from_slice(&json)?)
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// ClickHouse row model
//
// Column names and types here are the producer contract. The canonical DDL in
// `bin/reth-research/clickhouse/migrations` must match. Hashes are emitted as
// `0x`-prefixed lowercase hex (FixedString(66)); addresses as FixedString(42).
// `DateTime` columns are emitted as Unix-second integers, which ClickHouse
// accepts in JSONEachRow.
// ─────────────────────────────────────────────────────────────────────────────

fn hash66(value: B256) -> String {
    format!("{value:#x}")
}

fn addr42(value: Address) -> String {
    format!("{value:#x}")
}

fn opt_addr42(value: Option<Address>) -> Option<String> {
    value.map(addr42)
}

/// 4-byte function selector as `0x`-prefixed hex (`ClickHouse` `FixedString(10)`).
fn opt_selector(value: Option<[u8; 4]>) -> Option<String> {
    value.map(|s| format!("0x{}", alloy_primitives::hex::encode(s)))
}

/// Bounded byte blob (revert data / tx output) as `0x`-prefixed hex.
fn opt_bytes_hex(value: &Option<Vec<u8>>) -> Option<String> {
    value.as_ref().map(|b| format!("0x{}", alloy_primitives::hex::encode(b)))
}

/// One row for `gas_analysis_run` — one per deterministic analysis config.
#[allow(missing_docs)]
#[derive(Debug, Clone, Serialize)]
pub struct RunRow {
    pub updated_at: u64,
    pub analysis_config_hash: String,
    pub chain_id: u64,
    pub producer_schema_version: u32,
    pub producer_git_commit: String,
    pub replay_semantics: String,
    pub manifest_json: String,
}

impl RunRow {
    /// Build the run row from the manifest. `manifest_json` should be the same
    /// canonical JSON whose hash is `analysis_config_hash`.
    pub fn new(
        manifest: &AnalysisManifestV1,
        analysis_config_hash: impl Into<String>,
        manifest_json: impl Into<String>,
        updated_at: u64,
    ) -> Self {
        Self {
            updated_at,
            analysis_config_hash: analysis_config_hash.into(),
            chain_id: manifest.chain_id,
            producer_schema_version: manifest.producer_schema_version,
            producer_git_commit: manifest.producer_git_commit.clone(),
            replay_semantics: manifest.replay_semantics.clone(),
            manifest_json: manifest_json.into(),
        }
    }
}

/// One row for `gas_analysis_block_coverage`.
#[allow(missing_docs)]
#[derive(Debug, Clone, Serialize)]
pub struct CoverageRow {
    pub updated_at: u64,
    pub row_id: String,
    pub analysis_config_hash: String,
    pub chain_id: u64,
    pub producer_schema_version: u32,
    pub producer_git_commit: String,
    pub replay_semantics: String,
    pub schedule_name: String,
    pub schedule_config_hash: String,
    pub block_number: u64,
    pub block_hash: String,
    pub parent_hash: String,
    pub block_timestamp: u64,
    pub tx_count: u32,
    pub tx_count_unchanged: u32,
    pub tx_count_gas_only: u32,
    pub tx_count_stored: u32,
    pub block_gas_used: u64,
    pub block_gas_limit: u64,
    pub block_base_fee_per_gas: Option<u64>,
    pub expected_drill_in_count: u32,
    pub retained_drill_in_count: u32,
    pub drill_ins_truncated: bool,
}

/// One row for `gas_analysis_tx_gas_result` — the unconditional per-tx gas
/// spine, one row per (schedule, block, tx). See the `tx_gas_results` `SQLite`
/// DDL for the post-refund vs pre-refund gas distinction.
#[allow(missing_docs)]
#[derive(Debug, Clone, Serialize)]
pub struct TxGasResultExportRow {
    pub updated_at: u64,
    pub row_id: String,
    pub analysis_config_hash: String,
    pub chain_id: u64,
    pub producer_schema_version: u32,
    pub producer_git_commit: String,
    pub replay_semantics: String,
    pub schedule_name: String,
    pub schedule_config_hash: String,
    pub block_number: u64,
    pub block_hash: String,
    pub block_timestamp: u64,
    pub tx_index: u32,
    pub tx_hash: String,
    pub tx_type: u8,
    pub tx_gas_limit: u64,
    /// U256 decimal strings, matching the `SQLite` representation.
    pub max_fee_per_gas: String,
    pub max_priority_fee_per_gas: Option<String>,
    pub baseline_success: bool,
    pub baseline_gas_used: u64,
    pub baseline_total_gas_spent: u64,
    pub schedule_success: bool,
    pub schedule_gas_used: u64,
    pub schedule_total_gas_spent: u64,
    pub schedule_gas_refunded: u64,
    pub schedule_floor_gas: u64,
    pub schedule_state_gas_spent: u64,
    pub schedule_intrinsic_gas: Option<u64>,
    pub min_multiplier_to_succeed: Option<f64>,
}

/// One row for `gas_analysis_block_summary`.
#[allow(missing_docs)]
#[derive(Debug, Clone, Serialize)]
pub struct SummaryRow {
    pub updated_at: u64,
    pub row_id: String,
    pub analysis_config_hash: String,
    pub chain_id: u64,
    pub producer_schema_version: u32,
    pub producer_git_commit: String,
    pub replay_semantics: String,
    pub schedule_name: String,
    pub schedule_config_hash: String,
    pub block_number: u64,
    pub block_hash: String,
    pub block_timestamp: u64,
    pub class: String,
    pub tx_count: u32,
    pub gas_delta_sum: Option<i64>,
    pub gas_delta_sum_sq: Option<f64>,
    pub gas_delta_min: Option<i64>,
    pub gas_delta_max: Option<i64>,
    pub gas_delta_log2_hist: Vec<i32>,
    pub opcode: Vec<u8>,
    pub opcode_count: Vec<u64>,
    pub opcode_gas_baseline: Vec<u64>,
    pub opcode_gas_schedule: Vec<u64>,
    pub state_gas_sum: Option<u64>,
    pub state_gas_spillover_sum: Option<u64>,
    pub multiplier_log2_hist: Vec<i32>,
    pub tx_count_creation: Option<u32>,
    pub tx_count_authorization: Option<u32>,
    pub tx_count_runtime_state: Option<u32>,
    pub tx_count_no_state: Option<u32>,
    pub cold_account_access_count: Option<u64>,
    pub sload_cold_count: Option<u64>,
    pub sload_warm_count: Option<u64>,
    pub sstore_cold_count: Option<u64>,
    pub sstore_set_count: Option<u64>,
    pub sstore_reset_count: Option<u64>,
    pub sstore_clear_count: Option<u64>,
    pub sstore_noop_count: Option<u64>,
    pub sstore_dirty_count: Option<u64>,
    pub warm_account_access_count: Option<u64>,
    pub value_transfer_count: Option<u64>,
    pub create_opcode_count: Option<u64>,
    pub access_list_address_count: Option<u64>,
    pub access_list_storage_key_count: Option<u64>,
    pub tx_count_type_legacy: Option<u32>,
    pub tx_count_type_access_list: Option<u32>,
    pub tx_count_type_dynamic_fee: Option<u32>,
    pub tx_count_type_blob: Option<u32>,
    pub tx_count_type_set_code: Option<u32>,
    pub tx_count_type_other: Option<u32>,
    pub tx_count_simple_transfer: Option<u32>,
    pub tx_count_contract_call: Option<u32>,
    /// 13 closed-left percentage bins of `100*gas_delta/baseline_gas_used`
    /// (edges `[-100,-50,-25,-10,-1,0,1,10,25,50,100,200,500,+inf)`);
    /// empty when the source summary carried no histogram.
    pub gas_delta_pct_hist: Vec<i32>,
    pub baseline_gas_used_sum: Option<u64>,
}

/// One row for `gas_analysis_divergence`. Copies every scalar from
/// [`DivergenceRow`], inherits the block hash and identity columns, and carries
/// the forensic child components as a `trace_payload` JSON blob plus its
/// content hash and component counts.
#[allow(missing_docs)]
#[derive(Debug, Clone, Serialize)]
pub struct DivergenceExportRow {
    pub updated_at: u64,
    pub row_id: String,
    pub analysis_config_hash: String,
    pub chain_id: u64,
    pub producer_schema_version: u32,
    pub producer_git_commit: String,
    pub replay_semantics: String,
    pub schedule_name: String,
    pub schedule_config_hash: String,
    pub block_number: u64,
    pub block_hash: String,
    pub block_timestamp: u64,
    pub tx_index: u32,
    pub tx_hash: String,

    pub sender: String,
    pub recipient: Option<String>,
    pub is_create: bool,
    pub tx_gas_limit: u64,

    pub baseline_success: bool,
    pub schedule_success: bool,
    pub status_changed: bool,
    pub event_logs_changed: bool,
    pub output_changed: bool,
    pub logs_bloom_changed: bool,

    pub baseline_gas_used: u64,
    pub schedule_gas_used: u64,
    pub gas_delta: i64,
    pub baseline_total_gas_spent: Option<u64>,
    pub baseline_gas_refunded: Option<u64>,
    pub schedule_total_gas_spent: Option<u64>,
    pub schedule_gas_refunded: Option<u64>,
    pub schedule_intrinsic_gas: Option<u64>,
    pub schedule_floor_gas: Option<u64>,
    pub outer_limit_only_failure: Option<bool>,
    pub min_multiplier_to_succeed: Option<f64>,

    pub divergence_contract: Option<String>,
    pub divergence_pc: Option<u32>,
    pub divergence_call_depth: Option<i32>,
    pub divergence_opcode: Option<u8>,
    pub oog_contract: Option<String>,
    pub oog_pc: Option<u32>,
    pub oog_call_depth: Option<i32>,
    pub oog_opcode: Option<u8>,
    pub oog_pattern: Option<String>,
    pub oog_gas_remaining: Option<u64>,
    pub oog_chain_proportional: Option<bool>,
    pub oog_bottleneck_depth: Option<i32>,
    pub oog_bottleneck_kind: Option<String>,

    pub schedule_state_gas_spent: Option<u64>,
    pub schedule_state_gas_demanded: Option<u64>,
    pub schedule_initial_state_gas: Option<u64>,
    pub schedule_initial_reservoir: Option<u64>,
    pub runtime_state_gas: Option<u64>,
    pub runtime_state_gas_spillover: Option<u64>,
    pub state_gas_category: Option<String>,
    pub reservoir_exhausted: Option<bool>,
    pub replay_halt_oog: Option<bool>,

    pub cold_account_access_count: Option<u64>,
    pub additional_gas_charged: Option<i64>,
    pub failure_selector_path: Option<String>,

    pub tx_type: Option<u8>,
    pub tx_nonce: Option<u64>,
    pub entry_selector: Option<String>,
    pub input_zero_bytes: Option<u64>,
    pub input_nonzero_bytes: Option<u64>,
    pub has_authorization: Option<bool>,

    pub failure_reason: Option<String>,
    pub revert_data: Option<String>,
    pub revert_decoded: Option<String>,
    pub tx_output: Option<String>,

    pub baseline_frame_success: Option<bool>,
    pub baseline_frame_gas_used: Option<u64>,
    pub baseline_frame_gas_provided: Option<u64>,

    pub surcharge_at_oog: Option<i64>,
    pub gas_div_contract: Option<String>,
    pub gas_div_pc: Option<u32>,
    pub gas_div_call_depth: Option<i32>,
    pub gas_div_opcode: Option<u8>,

    pub tax_second_db_read: Option<i64>,
    pub tax_other: Option<i64>,
    pub tax_intrinsic: Option<i64>,

    pub sload_cold_count: Option<u64>,
    pub sload_warm_count: Option<u64>,
    pub sstore_cold_count: Option<u64>,
    pub sstore_set_count: Option<u64>,
    pub sstore_reset_count: Option<u64>,
    pub sstore_clear_count: Option<u64>,
    pub sstore_noop_count: Option<u64>,
    pub sstore_dirty_count: Option<u64>,
    pub warm_account_access_count: Option<u64>,
    pub value_transfer_count: Option<u64>,
    pub create_opcode_count: Option<u64>,
    pub access_list_address_count: Option<u64>,
    pub access_list_storage_key_count: Option<u64>,

    pub tier1_failure_reason: Option<String>,
    pub tier1_oog_opcode: Option<u8>,
    pub tier1_oog_contract: Option<String>,
    pub tier1_oog_pc: Option<u32>,
    pub tier1_oog_depth: Option<i32>,
    pub tier1_oog_gas_remaining: Option<u64>,
    pub tier1_failing_selector: Option<String>,
    pub tier1_failing_gas_provided: Option<u64>,
    pub tier1_failing_gas_requested: Option<u64>,

    pub trace_payload: String,
    pub trace_content_hash: String,
    pub trace_uncompressed_size_bytes: u64,
    pub trace_format: String,
    pub trace_format_version: u16,
    pub call_frame_count: u32,
    pub opcode_count_row_count: u32,
    pub baseline_event_log_count: u32,
    pub schedule_event_log_count: u32,
    pub opcode_capture_complete: Option<bool>,
}

/// The forensic child components of a divergence, serialized into the
/// `trace_payload` column. Only the children and a small identity header live
/// here; scalar divergence columns are not duplicated inside it.
#[derive(Debug, Clone, Serialize)]
struct TracePayloadV1<'a> {
    format_version: u16,
    export_id: &'a str,
    tx_index: u32,
    tx_hash: String,
    call_frames: &'a [CallFrameRow],
    opcode_counts: &'a [OpcodeCountRow],
    baseline_call_frames: &'a [CallFrameRow],
    baseline_opcode_counts: &'a [OpcodeCountRow],
    baseline_event_logs: &'a [EventLog],
    schedule_event_logs: &'a [EventLog],
}

/// All `ClickHouse` rows derived from a single block's output (the run row is
/// emitted separately, once per dataset).
#[derive(Debug, Clone)]
pub struct BlockClickHouseRows {
    /// The single coverage row.
    pub coverage: CoverageRow,
    /// Zero or more per-bucket summary rows.
    pub summaries: Vec<SummaryRow>,
    /// Zero or more per-tx divergence rows.
    pub divergences: Vec<DivergenceExportRow>,
    /// One row per tx in the block — the unconditional gas spine.
    pub tx_gas_results: Vec<TxGasResultExportRow>,
}

/// Convert a block's output into its `ClickHouse` rows.
///
/// Summary and divergence rows inherit `block_hash`, `block_timestamp`, and the
/// identity columns from the enclosing coverage row, exactly as the schema
/// contract requires.
pub fn block_output_to_rows(
    output: &BlockOutput,
    manifest: &AnalysisManifestV1,
    analysis_config_hash: &str,
    updated_at: u64,
) -> Result<BlockClickHouseRows, ExportModelError> {
    let coverage = &output.coverage;
    let block_hash = coverage.block_hash;
    let block_timestamp = coverage.timestamp;
    let export_id = export_id(analysis_config_hash, &coverage.schedule_name, block_hash);
    let capture = CaptureMetadataV1::from_output(output);

    let coverage_row = build_coverage_row(
        coverage,
        manifest,
        analysis_config_hash,
        &export_id,
        &capture,
        updated_at,
    );

    let summaries = output
        .summaries
        .iter()
        .map(|summary| {
            build_summary_row(
                summary,
                coverage,
                manifest,
                analysis_config_hash,
                &export_id,
                block_timestamp,
                updated_at,
            )
        })
        .collect();

    let divergences = output
        .drill_ins
        .iter()
        .map(|drill_in| {
            build_divergence_row(
                drill_in,
                coverage,
                manifest,
                analysis_config_hash,
                &export_id,
                block_hash,
                block_timestamp,
                updated_at,
            )
        })
        .collect::<Result<Vec<_>, _>>()?;

    let tx_gas_results = output
        .tx_gas_results
        .iter()
        .map(|row| {
            build_tx_gas_result_row(
                row,
                manifest,
                analysis_config_hash,
                &export_id,
                block_hash,
                block_timestamp,
                updated_at,
            )
        })
        .collect();

    Ok(BlockClickHouseRows { coverage: coverage_row, summaries, divergences, tx_gas_results })
}

#[allow(clippy::too_many_arguments)]
fn build_tx_gas_result_row(
    row: &TxGasResultRow,
    manifest: &AnalysisManifestV1,
    analysis_config_hash: &str,
    export_id: &str,
    block_hash: B256,
    block_timestamp: u64,
    updated_at: u64,
) -> TxGasResultExportRow {
    TxGasResultExportRow {
        updated_at,
        row_id: tx_gas_result_row_id(export_id, row.tx_index, row.tx_hash),
        analysis_config_hash: analysis_config_hash.to_string(),
        chain_id: manifest.chain_id,
        producer_schema_version: manifest.producer_schema_version,
        producer_git_commit: manifest.producer_git_commit.clone(),
        replay_semantics: manifest.replay_semantics.clone(),
        schedule_name: row.schedule_name.clone(),
        schedule_config_hash: row.schedule_config_hash.clone(),
        block_number: row.block_number,
        block_hash: hash66(block_hash),
        block_timestamp,
        tx_index: row.tx_index,
        tx_hash: hash66(row.tx_hash),
        tx_type: row.tx_type,
        tx_gas_limit: row.tx_gas_limit,
        max_fee_per_gas: row.max_fee_per_gas.clone(),
        max_priority_fee_per_gas: row.max_priority_fee_per_gas.clone(),
        baseline_success: row.baseline_success,
        baseline_gas_used: row.baseline_gas_used,
        baseline_total_gas_spent: row.baseline_total_gas_spent,
        schedule_success: row.schedule_success,
        schedule_gas_used: row.schedule_gas_used,
        schedule_total_gas_spent: row.schedule_total_gas_spent,
        schedule_gas_refunded: row.schedule_gas_refunded,
        schedule_floor_gas: row.schedule_floor_gas,
        schedule_state_gas_spent: row.schedule_state_gas_spent,
        schedule_intrinsic_gas: row.schedule_intrinsic_gas,
        min_multiplier_to_succeed: row.min_multiplier_to_succeed,
    }
}

fn build_coverage_row(
    coverage: &BlockCoverageRow,
    manifest: &AnalysisManifestV1,
    analysis_config_hash: &str,
    export_id: &str,
    capture: &CaptureMetadataV1,
    updated_at: u64,
) -> CoverageRow {
    CoverageRow {
        updated_at,
        row_id: coverage_row_id(export_id),
        analysis_config_hash: analysis_config_hash.to_string(),
        chain_id: manifest.chain_id,
        producer_schema_version: manifest.producer_schema_version,
        producer_git_commit: manifest.producer_git_commit.clone(),
        replay_semantics: manifest.replay_semantics.clone(),
        schedule_name: coverage.schedule_name.clone(),
        schedule_config_hash: coverage.schedule_config_hash.clone(),
        block_number: coverage.block_number,
        block_hash: hash66(coverage.block_hash),
        parent_hash: hash66(coverage.parent_hash),
        block_timestamp: coverage.timestamp,
        tx_count: coverage.tx_count,
        tx_count_unchanged: coverage.tx_count_unchanged,
        tx_count_gas_only: coverage.tx_count_gas_only,
        tx_count_stored: coverage.tx_count_stored,
        block_gas_used: coverage.block_gas_used,
        block_gas_limit: coverage.block_gas_limit,
        block_base_fee_per_gas: coverage.block_base_fee_per_gas,
        expected_drill_in_count: capture.expected_drill_in_count,
        retained_drill_in_count: capture.retained_drill_in_count,
        drill_ins_truncated: capture.drill_ins_truncated,
    }
}

#[allow(clippy::too_many_arguments)]
fn build_summary_row(
    summary: &BlockSummaryRow,
    coverage: &BlockCoverageRow,
    manifest: &AnalysisManifestV1,
    analysis_config_hash: &str,
    export_id: &str,
    block_timestamp: u64,
    updated_at: u64,
) -> SummaryRow {
    // Split the sparse opcode struct list into equal-length parallel arrays.
    let mut opcode = Vec::with_capacity(summary.opcode_totals.len());
    let mut opcode_count = Vec::with_capacity(summary.opcode_totals.len());
    let mut opcode_gas_baseline = Vec::with_capacity(summary.opcode_totals.len());
    let mut opcode_gas_schedule = Vec::with_capacity(summary.opcode_totals.len());
    for total in &summary.opcode_totals {
        opcode.push(total.opcode);
        opcode_count.push(total.count);
        opcode_gas_baseline.push(total.gas_baseline);
        opcode_gas_schedule.push(total.gas_schedule);
    }

    SummaryRow {
        updated_at,
        row_id: summary_row_id(export_id, summary.class),
        analysis_config_hash: analysis_config_hash.to_string(),
        chain_id: manifest.chain_id,
        producer_schema_version: manifest.producer_schema_version,
        producer_git_commit: manifest.producer_git_commit.clone(),
        replay_semantics: manifest.replay_semantics.clone(),
        // Inherited from the enclosing coverage row.
        schedule_name: coverage.schedule_name.clone(),
        schedule_config_hash: coverage.schedule_config_hash.clone(),
        block_number: coverage.block_number,
        block_hash: hash66(coverage.block_hash),
        block_timestamp,
        class: summary.class.as_str().to_string(),
        tx_count: summary.tx_count,
        gas_delta_sum: summary.gas_delta_sum,
        gas_delta_sum_sq: summary.gas_delta_sum_sq.map(|v| v as f64),
        gas_delta_min: summary.gas_delta_min,
        gas_delta_max: summary.gas_delta_max,
        gas_delta_log2_hist: summary.gas_delta_log2_hist.map(|h| h.to_vec()).unwrap_or_default(),
        opcode,
        opcode_count,
        opcode_gas_baseline,
        opcode_gas_schedule,
        state_gas_sum: summary.state_gas_sum,
        state_gas_spillover_sum: summary.state_gas_spillover_sum,
        multiplier_log2_hist: summary.multiplier_log2_hist.map(|h| h.to_vec()).unwrap_or_default(),
        tx_count_creation: summary.tx_count_creation,
        tx_count_authorization: summary.tx_count_authorization,
        tx_count_runtime_state: summary.tx_count_runtime_state,
        tx_count_no_state: summary.tx_count_no_state,
        cold_account_access_count: summary.cold_account_access_count,
        sload_cold_count: summary.storage_drivers.map(|s| s.sload_cold),
        sload_warm_count: summary.storage_drivers.map(|s| s.sload_warm),
        sstore_cold_count: summary.storage_drivers.map(|s| s.sstore_cold),
        sstore_set_count: summary.storage_drivers.map(|s| s.sstore_set),
        sstore_reset_count: summary.storage_drivers.map(|s| s.sstore_reset),
        sstore_clear_count: summary.storage_drivers.map(|s| s.sstore_clear),
        sstore_noop_count: summary.storage_drivers.map(|s| s.sstore_noop),
        sstore_dirty_count: summary.storage_drivers.map(|s| s.sstore_dirty),
        warm_account_access_count: summary.account_drivers.map(|a| a.warm_account_access),
        value_transfer_count: summary.account_drivers.map(|a| a.value_transfer),
        create_opcode_count: summary.account_drivers.map(|a| a.create_opcode),
        access_list_address_count: summary.account_drivers.map(|a| a.access_list_address),
        access_list_storage_key_count: summary.account_drivers.map(|a| a.access_list_storage_key),
        tx_count_type_legacy: summary.tx_count_type_legacy,
        tx_count_type_access_list: summary.tx_count_type_access_list,
        tx_count_type_dynamic_fee: summary.tx_count_type_dynamic_fee,
        tx_count_type_blob: summary.tx_count_type_blob,
        tx_count_type_set_code: summary.tx_count_type_set_code,
        tx_count_type_other: summary.tx_count_type_other,
        tx_count_simple_transfer: summary.tx_count_simple_transfer,
        tx_count_contract_call: summary.tx_count_contract_call,
        gas_delta_pct_hist: summary.gas_delta_pct_hist.map(|h| h.to_vec()).unwrap_or_default(),
        baseline_gas_used_sum: summary.baseline_gas_used_sum,
    }
}

#[allow(clippy::too_many_arguments)]
fn build_divergence_row(
    drill_in: &crate::database::DrillInRecord,
    coverage: &BlockCoverageRow,
    manifest: &AnalysisManifestV1,
    analysis_config_hash: &str,
    export_id: &str,
    block_hash: B256,
    block_timestamp: u64,
    updated_at: u64,
) -> Result<DivergenceExportRow, ExportModelError> {
    let d: &DivergenceRow = &drill_in.divergence;

    let trace = TracePayloadV1 {
        format_version: TRACE_FORMAT_VERSION,
        export_id,
        tx_index: d.tx_index,
        tx_hash: hash66(d.tx_hash),
        call_frames: &drill_in.call_frames,
        opcode_counts: &drill_in.opcode_counts,
        baseline_call_frames: &drill_in.baseline_call_frames,
        baseline_opcode_counts: &drill_in.baseline_opcode_counts,
        baseline_event_logs: &drill_in.baseline_event_logs,
        schedule_event_logs: &drill_in.schedule_event_logs,
    };
    let trace_payload = serde_json::to_string(&trace)?;
    let trace_content_hash = format!("{:#x}", keccak256(trace_payload.as_bytes()));
    let trace_uncompressed_size_bytes = trace_payload.len() as u64;

    Ok(DivergenceExportRow {
        updated_at,
        row_id: divergence_row_id(export_id, d.tx_index, d.tx_hash),
        analysis_config_hash: analysis_config_hash.to_string(),
        chain_id: manifest.chain_id,
        producer_schema_version: manifest.producer_schema_version,
        producer_git_commit: manifest.producer_git_commit.clone(),
        replay_semantics: manifest.replay_semantics.clone(),
        schedule_name: coverage.schedule_name.clone(),
        schedule_config_hash: coverage.schedule_config_hash.clone(),
        block_number: coverage.block_number,
        block_hash: hash66(block_hash),
        block_timestamp,
        tx_index: d.tx_index,
        tx_hash: hash66(d.tx_hash),

        sender: addr42(d.sender),
        recipient: opt_addr42(d.recipient),
        is_create: d.is_create,
        tx_gas_limit: d.tx_gas_limit,

        baseline_success: d.baseline_success,
        schedule_success: d.schedule_success,
        status_changed: d.status_changed,
        event_logs_changed: d.event_logs_changed,
        output_changed: d.output_changed,
        logs_bloom_changed: d.logs_bloom_changed,

        baseline_gas_used: d.baseline_gas_used,
        schedule_gas_used: d.schedule_gas_used,
        gas_delta: d.gas_delta,
        baseline_total_gas_spent: d.baseline_total_gas_spent,
        baseline_gas_refunded: d.baseline_gas_refunded,
        schedule_total_gas_spent: d.schedule_total_gas_spent,
        schedule_gas_refunded: d.schedule_gas_refunded,
        schedule_intrinsic_gas: d.schedule_intrinsic_gas,
        schedule_floor_gas: d.schedule_floor_gas,
        outer_limit_only_failure: d.outer_limit_only_failure,
        min_multiplier_to_succeed: d.min_multiplier_to_succeed,

        divergence_contract: opt_addr42(d.divergence_contract),
        divergence_pc: d.divergence_pc,
        divergence_call_depth: d.divergence_call_depth,
        divergence_opcode: d.divergence_opcode,
        oog_contract: opt_addr42(d.oog_contract),
        oog_pc: d.oog_pc,
        oog_call_depth: d.oog_call_depth,
        oog_opcode: d.oog_opcode,
        oog_pattern: d.oog_pattern.clone(),
        oog_gas_remaining: d.oog_gas_remaining,
        oog_chain_proportional: d.oog_chain_proportional,
        oog_bottleneck_depth: d.oog_bottleneck_depth,
        oog_bottleneck_kind: d.oog_bottleneck_kind.clone(),

        schedule_state_gas_spent: d.schedule_state_gas_spent,
        schedule_state_gas_demanded: d.schedule_state_gas_demanded,
        schedule_initial_state_gas: d.schedule_initial_state_gas,
        schedule_initial_reservoir: d.schedule_initial_reservoir,
        runtime_state_gas: d.runtime_state_gas,
        runtime_state_gas_spillover: d.runtime_state_gas_spillover,
        state_gas_category: d.state_gas_category.clone(),
        reservoir_exhausted: d.reservoir_exhausted,
        replay_halt_oog: d.replay_halt_oog,

        cold_account_access_count: d.cold_account_access_count,
        additional_gas_charged: d.additional_gas_charged,
        failure_selector_path: d.failure_selector_path.clone(),

        tx_type: d.tx_type,
        tx_nonce: d.tx_nonce,
        entry_selector: opt_selector(d.entry_selector),
        input_zero_bytes: d.input_zero_bytes,
        input_nonzero_bytes: d.input_nonzero_bytes,
        has_authorization: d.has_authorization,

        failure_reason: d.failure_reason.clone(),
        revert_data: opt_bytes_hex(&d.revert_data),
        revert_decoded: d.revert_decoded.clone(),
        tx_output: opt_bytes_hex(&d.tx_output),

        baseline_frame_success: d.baseline_frame_success,
        baseline_frame_gas_used: d.baseline_frame_gas_used,
        baseline_frame_gas_provided: d.baseline_frame_gas_provided,

        surcharge_at_oog: d.surcharge_at_oog,
        gas_div_contract: opt_addr42(d.gas_div_contract),
        gas_div_pc: d.gas_div_pc,
        gas_div_call_depth: d.gas_div_call_depth,
        gas_div_opcode: d.gas_div_opcode,

        tax_second_db_read: d.tax_second_db_read,
        tax_other: d.tax_other,
        tax_intrinsic: d.tax_intrinsic,

        sload_cold_count: d.storage_drivers.map(|s| s.sload_cold),
        sload_warm_count: d.storage_drivers.map(|s| s.sload_warm),
        sstore_cold_count: d.storage_drivers.map(|s| s.sstore_cold),
        sstore_set_count: d.storage_drivers.map(|s| s.sstore_set),
        sstore_reset_count: d.storage_drivers.map(|s| s.sstore_reset),
        sstore_clear_count: d.storage_drivers.map(|s| s.sstore_clear),
        sstore_noop_count: d.storage_drivers.map(|s| s.sstore_noop),
        sstore_dirty_count: d.storage_drivers.map(|s| s.sstore_dirty),
        warm_account_access_count: d.account_drivers.map(|a| a.warm_account_access),
        value_transfer_count: d.account_drivers.map(|a| a.value_transfer),
        create_opcode_count: d.account_drivers.map(|a| a.create_opcode),
        access_list_address_count: d.account_drivers.map(|a| a.access_list_address),
        access_list_storage_key_count: d.account_drivers.map(|a| a.access_list_storage_key),

        tier1_failure_reason: d.tier1_failure_reason.clone(),
        tier1_oog_opcode: d.tier1_oog_opcode,
        tier1_oog_contract: opt_addr42(d.tier1_oog_contract),
        tier1_oog_pc: d.tier1_oog_pc,
        tier1_oog_depth: d.tier1_oog_depth,
        tier1_oog_gas_remaining: d.tier1_oog_gas_remaining,
        tier1_failing_selector: opt_selector(d.tier1_failing_selector),
        tier1_failing_gas_provided: d.tier1_failing_gas_provided,
        tier1_failing_gas_requested: d.tier1_failing_gas_requested,

        trace_payload,
        trace_content_hash,
        trace_uncompressed_size_bytes,
        trace_format: TRACE_FORMAT.to_string(),
        trace_format_version: TRACE_FORMAT_VERSION,
        call_frame_count: drill_in.call_frames.len() as u32,
        opcode_count_row_count: drill_in.opcode_counts.len() as u32,
        baseline_event_log_count: drill_in.baseline_event_logs.len() as u32,
        schedule_event_log_count: drill_in.schedule_event_logs.len() as u32,
        opcode_capture_complete: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        database::{BlockCoverageRow, BlockOutput, DrillInRecord, OpcodeBucketTotal},
        schedule::{AmsterdamSchedule, MultiplierSchedule, ScheduleRegistry},
    };
    use alloy_primitives::{Address, Bytes, B256};

    fn registry_two_lanes() -> ScheduleRegistry {
        let mut r = ScheduleRegistry::new();
        r.register(AmsterdamSchedule::new()).unwrap();
        r.register(MultiplierSchedule::new("128x".to_string(), 128)).unwrap();
        r
    }

    fn manifest(reg: &ScheduleRegistry, tiers: Vec<u64>) -> AnalysisManifestV1 {
        AnalysisManifestV1::build(reg, normalize_gas_tiers(&tiers), Some(50), 1, 10, "deadbeef")
    }

    #[test]
    fn manifest_hash_is_stable_for_identical_input() {
        let reg = registry_two_lanes();
        let a = manifest(&reg, vec![1, 2, 4]).analysis_config_hash().unwrap();
        let b = manifest(&reg, vec![1, 2, 4]).analysis_config_hash().unwrap();
        assert_eq!(a, b);
        assert_eq!(a.len(), 66);
    }

    #[test]
    fn manifest_hash_ignores_registry_order() {
        let mut reg_a = ScheduleRegistry::new();
        reg_a.register(AmsterdamSchedule::new()).unwrap();
        reg_a.register(MultiplierSchedule::new("128x".to_string(), 128)).unwrap();

        let mut reg_b = ScheduleRegistry::new();
        reg_b.register(MultiplierSchedule::new("128x".to_string(), 128)).unwrap();
        reg_b.register(AmsterdamSchedule::new()).unwrap();

        assert_eq!(
            manifest(&reg_a, vec![1]).analysis_config_hash().unwrap(),
            manifest(&reg_b, vec![1]).analysis_config_hash().unwrap(),
        );
    }

    #[test]
    fn gas_tier_order_and_duplicates_normalize_to_same_hash() {
        let reg = registry_two_lanes();
        let a = manifest(&reg, vec![4, 2, 1, 2]).analysis_config_hash().unwrap();
        let b = manifest(&reg, vec![1, 2, 4]).analysis_config_hash().unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn normalize_clamps_sorts_dedups_and_defaults_empty() {
        assert_eq!(normalize_gas_tiers(&[0, 0, 3, 1, 1]), vec![1, 3]);
        assert_eq!(normalize_gas_tiers(&[]), vec![1]);
        assert_eq!(normalize_gas_tiers(&[8, 4, 2, 1]), vec![1, 2, 4, 8]);
    }

    #[test]
    fn changing_gas_tiers_changes_hash() {
        let reg = registry_two_lanes();
        assert_ne!(
            manifest(&reg, vec![1]).analysis_config_hash().unwrap(),
            manifest(&reg, vec![1, 2]).analysis_config_hash().unwrap(),
        );
    }

    #[test]
    fn changing_drill_in_cap_changes_hash() {
        let reg = registry_two_lanes();
        let tiers = normalize_gas_tiers(&[1]);
        let a = AnalysisManifestV1::build(&reg, tiers.clone(), Some(10), 1, 10, "c")
            .analysis_config_hash()
            .unwrap();
        let b = AnalysisManifestV1::build(&reg, tiers, Some(20), 1, 10, "c")
            .analysis_config_hash()
            .unwrap();
        assert_ne!(a, b);
    }

    #[test]
    fn changing_commit_or_chain_changes_hash() {
        let reg = registry_two_lanes();
        let tiers = normalize_gas_tiers(&[1]);
        let base = AnalysisManifestV1::build(&reg, tiers.clone(), None, 1, 10, "aaaa")
            .analysis_config_hash()
            .unwrap();
        let diff_commit = AnalysisManifestV1::build(&reg, tiers.clone(), None, 1, 9, "bbbb")
            .analysis_config_hash()
            .unwrap();
        let diff_chain = AnalysisManifestV1::build(&reg, tiers, None, 11155111, 9, "aaaa")
            .analysis_config_hash()
            .unwrap();
        assert_ne!(base, diff_commit);
        assert_ne!(base, diff_chain);
    }

    #[test]
    fn changing_schedule_set_changes_hash() {
        let reg_small = {
            let mut r = ScheduleRegistry::new();
            r.register(AmsterdamSchedule::new()).unwrap();
            r
        };
        let reg_big = registry_two_lanes();
        assert_ne!(
            manifest(&reg_small, vec![1]).analysis_config_hash().unwrap(),
            manifest(&reg_big, vec![1]).analysis_config_hash().unwrap(),
        );
    }

    #[test]
    fn schedule_config_hash_matches_producer_derivation() {
        // The manifest's per-schedule config_hash must equal the value the
        // producer writes into block_coverage: keccak256(config_fingerprint).
        let schedule = MultiplierSchedule::new("128x".to_string(), 128);
        let m = ScheduleManifestV1::from_schedule(&schedule);
        let expected = format!("{:#x}", keccak256(schedule.config_fingerprint().as_bytes()));
        assert_eq!(m.config_hash, expected);
        assert_eq!(m.config_hash.len(), 66);
    }

    #[test]
    fn deterministic_row_ids_are_stable_and_distinct() {
        let ach = "0x".to_string() + &"ab".repeat(32);
        let bh = B256::repeat_byte(0x11);
        let eid = export_id(&ach, "eip-2780", bh);
        assert_eq!(eid, export_id(&ach, "eip-2780", bh));
        assert_eq!(eid.len(), 66);

        // Different schedule → different export id.
        assert_ne!(eid, export_id(&ach, "eip-8037", bh));

        // Child ids derive from the export id and are distinct per kind.
        let cov = coverage_row_id(&eid);
        let sum = summary_row_id(&eid, AggregateClass::GasOnly);
        let div = divergence_row_id(&eid, 3, B256::repeat_byte(0x22));
        assert_ne!(cov, sum);
        assert_ne!(cov, div);
        assert_ne!(
            summary_row_id(&eid, AggregateClass::GasOnly),
            summary_row_id(&eid, AggregateClass::Unchanged)
        );
        assert_ne!(
            divergence_row_id(&eid, 3, B256::repeat_byte(0x22)),
            divergence_row_id(&eid, 4, B256::repeat_byte(0x22)),
        );
    }

    fn sample_coverage(drill_in_buckets: u32) -> BlockCoverageRow {
        BlockCoverageRow {
            schedule_name: "eip-2780".to_string(),
            schedule_config_hash: "0x".to_string() + &"cd".repeat(32),
            block_number: 100,
            block_hash: B256::repeat_byte(0xaa),
            parent_hash: B256::repeat_byte(0xbb),
            timestamp: 1_700_000_000,
            tx_count: 10,
            tx_count_unchanged: 5,
            tx_count_gas_only: 1,
            tx_count_stored: drill_in_buckets,
            block_gas_used: 21000,
            block_gas_limit: 30_000_000,
            block_base_fee_per_gas: Some(1_000_000_000),
        }
    }

    fn sample_divergence() -> DivergenceRow {
        DivergenceRow {
            schedule_name: "eip-2780".to_string(),
            schedule_config_hash: "0x".to_string() + &"cd".repeat(32),
            block_number: 100,
            tx_index: 2,
            tx_hash: B256::repeat_byte(0x33),
            timestamp: 1_700_000_000,
            sender: Address::repeat_byte(0x01),
            recipient: Some(Address::repeat_byte(0x02)),
            is_create: false,
            tx_gas_limit: 100_000,
            baseline_success: true,
            schedule_success: true,
            status_changed: false,
            event_logs_changed: true,
            output_changed: false,
            logs_bloom_changed: true,
            baseline_gas_used: 21000,
            schedule_gas_used: 21010,
            gas_delta: 10,
            baseline_total_gas_spent: Some(21000),
            baseline_gas_refunded: None,
            schedule_total_gas_spent: Some(21010),
            schedule_gas_refunded: None,
            schedule_intrinsic_gas: Some(21000),
            schedule_floor_gas: None,
            outer_limit_only_failure: Some(true),
            min_multiplier_to_succeed: None,
            divergence_contract: None,
            divergence_pc: None,
            divergence_call_depth: None,
            divergence_opcode: None,
            oog_contract: None,
            oog_pc: None,
            oog_call_depth: None,
            oog_opcode: None,
            oog_pattern: None,
            oog_gas_remaining: None,
            oog_chain_proportional: None,
            oog_bottleneck_depth: None,
            oog_bottleneck_kind: None,
            schedule_state_gas_spent: None,
            schedule_state_gas_demanded: None,
            schedule_initial_state_gas: None,
            schedule_initial_reservoir: None,
            runtime_state_gas: None,
            runtime_state_gas_spillover: None,
            state_gas_category: None,
            reservoir_exhausted: None,
            replay_halt_oog: None,
            ..Default::default()
        }
    }

    fn sample_summary() -> BlockSummaryRow {
        BlockSummaryRow {
            schedule_name: "eip-2780".to_string(),
            block_number: 100,
            class: AggregateClass::GasOnly,
            tx_count: 1,
            gas_delta_sum: Some(10),
            gas_delta_sum_sq: Some(100),
            gas_delta_min: Some(10),
            gas_delta_max: Some(10),
            gas_delta_log2_hist: Some([0; 12]),
            opcode_totals: vec![
                OpcodeBucketTotal { opcode: 0x01, count: 3, gas_baseline: 9, gas_schedule: 12 },
                OpcodeBucketTotal { opcode: 0x20, count: 1, gas_baseline: 30, gas_schedule: 45 },
            ],
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
            tx_count_type_legacy: Some(0),
            tx_count_type_access_list: Some(0),
            tx_count_type_dynamic_fee: Some(1),
            tx_count_type_blob: Some(0),
            tx_count_type_set_code: Some(0),
            tx_count_type_other: Some(0),
            tx_count_simple_transfer: Some(0),
            tx_count_contract_call: Some(1),
            gas_delta_pct_hist: Some([0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0]),
            baseline_gas_used_sum: Some(21_000),
        }
    }

    fn sample_output() -> BlockOutput {
        BlockOutput {
            coverage: sample_coverage(1),
            summaries: vec![sample_summary()],
            drill_ins: vec![DrillInRecord {
                divergence: sample_divergence(),
                call_frames: vec![],
                opcode_counts: vec![],
                baseline_call_frames: vec![],
                baseline_opcode_counts: vec![],
                baseline_event_logs: vec![EventLog {
                    log_index: 0,
                    address: Address::repeat_byte(0x02),
                    topics: vec![B256::repeat_byte(0x77)],
                    data: Bytes::from_static(b"abc"),
                }],
                schedule_event_logs: vec![],
            }],
            recipients: vec![],
            tx_gas_results: vec![],
        }
    }

    #[test]
    fn envelope_json_zstd_round_trips() {
        let env = ExportEnvelopeV1::new("0xabc", sample_output());
        let encoded = env.encode().unwrap();
        assert!(encoded.payload_bytes > 0);
        let decoded =
            ExportEnvelopeV1::decode(&encoded.payload_zstd, encoded.payload_hash).unwrap();
        assert_eq!(decoded.analysis_config_hash, "0xabc");
        assert_eq!(decoded.output.coverage.block_number, 100);
        assert_eq!(decoded.output.drill_ins.len(), 1);
    }

    #[test]
    fn tx_gas_result_rows_are_built_for_every_tx() {
        let mut output = sample_output();
        output.tx_gas_results = (0..3)
            .map(|i| TxGasResultRow {
                schedule_name: "eip-2780".to_string(),
                schedule_config_hash: "0x".to_string() + &"cd".repeat(32),
                block_number: 100,
                tx_index: i,
                tx_hash: B256::repeat_byte(0x50 + i as u8),
                tx_type: 2,
                tx_gas_limit: 300_000,
                max_fee_per_gas: "30000000000".to_string(),
                max_priority_fee_per_gas: Some("1000000000".to_string()),
                baseline_success: true,
                baseline_gas_used: 21_000,
                baseline_total_gas_spent: 21_000,
                schedule_success: true,
                schedule_gas_used: 33_000,
                schedule_total_gas_spent: 35_000,
                schedule_gas_refunded: 2_000,
                schedule_floor_gas: 21_000,
                schedule_state_gas_spent: 0,
                schedule_intrinsic_gas: Some(15_000),
                min_multiplier_to_succeed: Some(0.11),
            })
            .collect();

        let reg = registry_two_lanes();
        let manifest = manifest(&reg, vec![1, 2, 4]);
        let rows = block_output_to_rows(&output, &manifest, "0xabc", 1_700_000_100).unwrap();
        assert_eq!(rows.tx_gas_results.len(), 3);
        // Coverage now carries the base fee that completes the fee-market view.
        assert_eq!(rows.coverage.block_base_fee_per_gas, Some(1_000_000_000));

        // row_id must be distinct per tx, and distinct from the divergence
        // row_id for the same (export_id, tx_index, tx_hash) — the two tables
        // describe the same tx and must not collide.
        let ids: std::collections::HashSet<_> =
            rows.tx_gas_results.iter().map(|r| r.row_id.clone()).collect();
        assert_eq!(ids.len(), 3);
        let eid = export_id("0xabc", "eip-2780", B256::repeat_byte(0xaa));
        assert_ne!(
            tx_gas_result_row_id(&eid, 0, B256::repeat_byte(0x50)),
            divergence_row_id(&eid, 0, B256::repeat_byte(0x50)),
        );

        // The pre-refund figure survives the hop distinctly from the
        // sender-facing one.
        let first = &rows.tx_gas_results[0];
        assert_eq!(first.schedule_gas_used, 33_000);
        assert_eq!(first.schedule_total_gas_spent, 35_000);
        assert_eq!(first.max_fee_per_gas, "30000000000");
    }

    #[test]
    fn envelope_decode_rejects_wrong_hash() {
        let env = ExportEnvelopeV1::new("0xabc", sample_output());
        let encoded = env.encode().unwrap();
        let err = ExportEnvelopeV1::decode(&encoded.payload_zstd, B256::ZERO).unwrap_err();
        assert!(matches!(err, ExportModelError::PayloadHashMismatch));
    }

    #[test]
    fn capture_metadata_infers_truncation_from_coverage() {
        // Coverage claims 3 drill-in-bucket txs but only 1 retained → truncated.
        let mut output = sample_output();
        output.coverage.tx_count_stored = 3;
        let capture = CaptureMetadataV1::from_output(&output);
        assert_eq!(capture.expected_drill_in_count, 3);
        assert_eq!(capture.retained_drill_in_count, 1);
        assert!(capture.drill_ins_truncated);
    }

    #[test]
    fn export_row_fields_match_required_columns() {
        // Guards the three-way contract: each export row struct's serialized field
        // set MUST equal its `required_columns()` allow-list, so the startup
        // DESCRIBE check (and the ClickHouse DDL it gates) can never silently drift
        // from what the producer actually writes.
        use crate::export::clickhouse::DestinationTable;
        use std::collections::BTreeSet;

        let reg = registry_two_lanes();
        let m = manifest(&reg, vec![1, 2, 4]);
        let ach = m.analysis_config_hash().unwrap();
        let rows = block_output_to_rows(&sample_output(), &m, &ach, 1_700_000_000).unwrap();

        let assert_match = |table: DestinationTable, value: serde_json::Value| {
            let serialized: BTreeSet<String> = value
                .as_object()
                .expect("row serializes to a JSON object")
                .keys()
                .cloned()
                .collect();
            let required: BTreeSet<String> =
                table.required_columns().iter().map(|s| (*s).to_string()).collect();
            assert_eq!(
                serialized, required,
                "{table:?}: export struct fields drift from required_columns()"
            );
        };

        assert_match(DestinationTable::Coverage, serde_json::to_value(&rows.coverage).unwrap());
        assert_match(DestinationTable::Summary, serde_json::to_value(&rows.summaries[0]).unwrap());
        assert_match(
            DestinationTable::Divergence,
            serde_json::to_value(&rows.divergences[0]).unwrap(),
        );
    }

    #[test]
    fn conversion_inherits_block_and_config_hashes() {
        let reg = registry_two_lanes();
        let m = manifest(&reg, vec![1]);
        let ach = m.analysis_config_hash().unwrap();
        let output = sample_output();
        let rows = block_output_to_rows(&output, &m, &ach, 1_700_000_123).unwrap();

        // Summary inherits the coverage block hash + config hash.
        let s = &rows.summaries[0];
        assert_eq!(s.block_hash, hash66(output.coverage.block_hash));
        assert_eq!(s.analysis_config_hash, ach);
        assert_eq!(s.schedule_config_hash, output.coverage.schedule_config_hash);
        // Parallel opcode arrays are equal length.
        assert_eq!(s.opcode.len(), 2);
        assert_eq!(s.opcode_count.len(), 2);
        assert_eq!(s.opcode_gas_baseline.len(), 2);
        assert_eq!(s.opcode_gas_schedule.len(), 2);

        // Divergence inherits the block hash and carries component counts.
        let d = &rows.divergences[0];
        assert_eq!(d.block_hash, hash66(output.coverage.block_hash));
        assert_eq!(d.baseline_event_log_count, 1);
        assert_eq!(d.schedule_event_log_count, 0);
        assert_eq!(d.call_frame_count, 0);
        // trace_content_hash matches a re-hash of the payload.
        assert_eq!(d.trace_content_hash, format!("{:#x}", keccak256(d.trace_payload.as_bytes())),);
        assert_eq!(d.trace_uncompressed_size_bytes, d.trace_payload.len() as u64);
    }

    #[test]
    fn summary_new_columns_map_through() {
        let reg = registry_two_lanes();
        let m = manifest(&reg, vec![1]);
        let ach = m.analysis_config_hash().unwrap();
        let rows = block_output_to_rows(&sample_output(), &m, &ach, 1).unwrap();
        let s = &rows.summaries[0];
        // v11 taxonomy passes through verbatim from the DB row.
        assert_eq!(s.tx_count_type_dynamic_fee, Some(1));
        assert_eq!(s.tx_count_type_other, Some(0));
        assert_eq!(s.tx_count_simple_transfer, Some(0));
        assert_eq!(s.tx_count_contract_call, Some(1));
        assert_eq!(s.gas_delta_pct_hist, vec![0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0]);
        assert_eq!(s.baseline_gas_used_sum, Some(21_000));

        // A summary without a histogram exports an empty array — the missing
        // marker for ClickHouse Array columns (Nullable(Array) is illegal) —
        // never null.
        let mut output = sample_output();
        output.summaries[0].gas_delta_pct_hist = None;
        let rows = block_output_to_rows(&output, &m, &ach, 1).unwrap();
        assert!(rows.summaries[0].gas_delta_pct_hist.is_empty());
        let json = serde_json::to_string(&rows.summaries[0]).unwrap();
        assert!(json.contains("\"gas_delta_pct_hist\":[]"));
    }

    #[test]
    fn conversion_optionals_stay_null_not_zero() {
        let reg = registry_two_lanes();
        let m = manifest(&reg, vec![1]);
        let ach = m.analysis_config_hash().unwrap();
        let rows = block_output_to_rows(&sample_output(), &m, &ach, 1).unwrap();
        let d = &rows.divergences[0];
        // A None Rust optional must serialize to JSON null, not 0.
        let json = serde_json::to_string(d).unwrap();
        assert!(json.contains("\"min_multiplier_to_succeed\":null"));
        assert!(json.contains("\"oog_pattern\":null"));
    }

    #[test]
    fn jsoneachrow_one_object_per_line() {
        let reg = registry_two_lanes();
        let m = manifest(&reg, vec![1]);
        let ach = m.analysis_config_hash().unwrap();
        let rows = block_output_to_rows(&sample_output(), &m, &ach, 1).unwrap();
        let line = serde_json::to_string(&rows.coverage).unwrap();
        assert!(!line.contains('\n'));
        let parsed: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert!(parsed.is_object());
    }
}
