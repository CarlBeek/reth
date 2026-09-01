//! Multi-Schedule Research Mode Execution Extension
//!
//! This ExEx performs multi-schedule execution analysis to detect divergences caused by
//! modified gas costs across multiple gas pricing experiments simultaneously.
//!
//! Supported schedules:
//! - Amsterdam: the Glamsterdam repricing stack (EIP-2780 + 7976 + 7981 + 8037 + 8038) via revm's
//!   native `SpecId::AMSTERDAM`
//! - CSV Pricing: Per-opcode/precompile gas repricing from CSV files
//! - Multiplier: Uniform gas cost multiplication
//!
//! Run with:
//!
//! ```sh
//! cargo run --release -p reth-research-bin node --dev --dev.block-time 5s \
//!   --research.amsterdam \
//!   --research.csv 7904-prelim=./schedules/7904_prelim.csv \
//!   --research.gas-limit-multipliers 1,2,4,8 \
//!   --research.db-path ./divergences.db
//! ```

use alloy_consensus::{
    constants::KECCAK_EMPTY, transaction::TxHashRef, BlockHeader, Transaction, Typed2718,
};
use alloy_eips::BlockHashOrNumber;
use alloy_primitives::{keccak256, logs_bloom, Address, Bloom, Bytes, B256, U256};
use clap::Parser;
use futures::{stream::FuturesUnordered, FutureExt, StreamExt, TryStreamExt};
use reth_ethereum::{
    chainspec::EthChainSpec,
    exex::{ExExContext, ExExEvent, ExExNotification},
    node::EthereumNode,
};
use reth_evm::{block::BlockExecutorFactory, ConfigureEvm, Evm, EvmFactory, TransactionEnvMut};
use reth_node_api::{BlockTy, FullNodeComponents};
use reth_node_core::{args::ResearchArgs, version::version_metadata};
use reth_primitives_traits::BlockBody;
use reth_provider::{BlockNumReader, BlockReader, StateProviderFactory, TransactionVariant};
use reth_research::{
    block_aggregator::{BlockAggregator, BlockMeta},
    database::{
        AnalysisManifestRecord, BlockOutput, CallFrameRow, DivergenceDatabase, DivergenceRow,
        DrillInRecord, EncodedExportEnvelope, OpcodeCountRow, SCHEMA_VERSION,
    },
    divergence::{
        AccountDrivers, CallFrame, CallType as ResCallType, DivergenceFacts, DivergenceLocation,
        EventLog, OutOfGasInfo, StorageDrivers, Tier1Diagnostics,
    },
    export::{
        export_id, normalize_gas_tiers, run_export_worker, AnalysisManifestV1, ExportConfig,
        ExportEnvelopeV1, ExportError, REPLAY_SEMANTICS,
    },
    oog_chain::classify_oog_chain,
    schedule::{GasSchedule, RecipientInfo, ScheduleKind, ScheduleRegistry, TxContext},
    step_trace::{first_divergence, StepTraceInspector},
    ScheduleInspector, TrackingInspector,
};
use reth_revm::{database::StateProviderDatabase, db::State, Database, DatabaseCommit};
use reth_tracing::tracing::{debug, info, warn};
use revm::{
    context::BlockEnv, context_interface::Cfg, interpreter::gas::calculate_initial_tx_gas,
    primitives::hardfork::SpecId,
};
use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    thread::JoinHandle,
};
use tokio::{
    sync::{mpsc, watch},
    task::JoinHandle as TokioJoinHandle,
};

/// One unit of work for the DB writer thread. Each `BlockProcessed`
/// command is a fully-built per-(schedule, block) output and gets written
/// as a single DuckDB transaction. `DeleteRange` clears all per-block
/// tables in a contiguous block range — used on chain reorg / revert.
enum DbCommand {
    BlockProcessed(BlockOutput),
    DeleteRange { from_block: u64, to_block: u64 },
}

/// Upper bound on the per-replay gas limit during the tier-multiplier sweep.
///
/// At tiers > 1 the sweep lifts `tx_gas_limit_cap` so it can measure how much
/// extra gas a tx would need. Without a ceiling, a tx that loops until it runs
/// out of gas executes up to `tx_gas_limit × max_tier` gas (e.g. 8 × a 36M-gas
/// tx ≈ 288M) — tens of millions of traced opcodes per replay, taking minutes
/// to hours while holding a long-lived MDBX read transaction that stalls the
/// whole node (the memory/MDBX runaway observed in production).
///
/// Capping here bounds per-tx replay time and read-txn lifetime. Beyond the
/// ceiling a tx is simply recorded as failing the tier (it needs more gas than
/// the cap); for repricing analysis "needs > 200M gas" is as actionable as the
/// exact figure. 200M comfortably fits a true 10× bump of the largest legitimate
/// tx (EIP-7825 caps real txs at ~16.7M → 10×16.7M = 167M < 200M) so the 8037
/// conditional-bump tier is honored exactly, while still bounding runaway loops.
const TIER_REPLAY_GAS_CEILING: u64 = 200_000_000;

/// Per-schedule metadata cached once at startup.
///
/// Previously also held kind / description / affected_opcodes /
/// affected_precompiles, used by the legacy SQLite `ScheduleDivergence`
/// rows. The DuckDB schema records those (where relevant) via
/// `block_summaries` + `divergences.schedule_name`, so only the
/// dedupe-keyed `config_hash` is still cached here.
#[derive(Clone)]
struct ScheduleMetadata {
    config_hash: String,
}

/// Stable string form of the inspector's `CallType` enum, matching the
/// values the consumer SQL expects in `divergence_call_frames.call_type`.
fn format_call_type(ct: &ResCallType) -> String {
    match ct {
        ResCallType::Call => "CALL",
        ResCallType::DelegateCall => "DELEGATECALL",
        ResCallType::StaticCall => "STATICCALL",
        ResCallType::CallCode => "CALLCODE",
        ResCallType::Create => "CREATE",
        ResCallType::Create2 => "CREATE2",
    }
    .to_string()
}

/// First four bytes of the call input, if present. NULL otherwise (e.g.
/// value-only transfers, fallback function calls without selectors).
fn extract_selector_bytes(input: &Option<Bytes>) -> Option<[u8; 4]> {
    input.as_ref().filter(|b| b.len() >= 4).map(|b| [b[0], b[1], b[2], b[3]])
}

/// Whether the EIP-150 63/64 cap was binding at a CALL: the caller's
/// stack-gas request was at least the cap. `None` when the inputs aren't
/// available (CREATE/CREATE2, root frame, or a baseline-only capture).
fn eip150_cap_binding(stack_gas: Option<u64>, parent_gas: Option<u64>) -> Option<bool> {
    let (s, p) = (stack_gas?, parent_gas?);
    let cap = p.saturating_mul(63) / 64;
    // Tolerance of 100 absorbs EIP-150 rounding + call-cost overhead;
    // same constant the OOG-chain classifier uses.
    Some(s >= cap.saturating_sub(100))
}

/// Truncate returndata to a bounded prefix for storage (F2). 132 bytes holds a
/// 4-byte error selector plus one 32-byte head word and the start of the
/// payload — enough to identify the error and read short revert strings — while
/// bounding a pathological multi-KB return.
const REVERT_DATA_CAP: usize = 132;

fn cap_bytes(data: &[u8]) -> Vec<u8> {
    data[..data.len().min(REVERT_DATA_CAP)].to_vec()
}

/// Best-effort decode of Solidity revert returndata (F2). Recognises the two
/// canonical errors plus custom 4-byte selectors; the raw (capped) bytes are
/// stored alongside so downstream can re-decode fully.
fn decode_revert(data: &[u8]) -> String {
    if data.is_empty() {
        return "empty".to_string();
    }
    if data.len() < 4 {
        return format!("short:0x{}", alloy_primitives::hex::encode(data));
    }
    match [data[0], data[1], data[2], data[3]] {
        // Error(string): selector + offset(32) + len(32) + utf8 bytes.
        [0x08, 0xc3, 0x79, 0xa0] => {
            if data.len() >= 68 {
                let len = u32::from_be_bytes([data[64], data[65], data[66], data[67]]) as usize;
                let end = 68usize.saturating_add(len).min(data.len());
                let s = String::from_utf8_lossy(&data[68..end]);
                format!("Error(string): {s}")
            } else {
                "Error(string)".to_string()
            }
        }
        // Panic(uint256): selector + 32-byte code (low byte holds the code).
        [0x4e, 0x48, 0x7b, 0x71] => {
            let code = data.get(35).copied().unwrap_or(0);
            format!("Panic(0x{code:02x})")
        }
        sel => format!("custom:0x{}", alloy_primitives::hex::encode(sel)),
    }
}

/// Reconstruct each frame's parent index from the post-order DFS sequence
/// the inspector emits.
///
/// For a frame at array index `i` with depth `d > 0`, its parent is the
/// first frame after `i` whose depth is exactly `d - 1`. Frames at depth 0
/// (the root) have no parent, so the returned slot is `None`. The result
/// is indexed by array position, not by `CallFrame.call_index` — the
/// caller pairs them up via `frames.iter().enumerate()`.
///
/// Output: `parent_call_indices[i] = Some(frames[parent_array_idx].call_index)`,
/// pre-mapped to the schema's `call_index` value so writers don't have to
/// look it up again.
fn derive_parent_call_indices(frames: &[reth_research::divergence::CallFrame]) -> Vec<Option<u32>> {
    let mut parents = vec![None; frames.len()];
    for i in 0..frames.len() {
        let d = frames[i].depth;
        if d == 0 {
            continue;
        }
        for j in (i + 1)..frames.len() {
            if frames[j].depth == d - 1 {
                parents[i] = Some(frames[j].call_index as u32);
                break;
            }
        }
    }
    parents
}

/// Convert an inspector's `CallFrame` trace into the persisted `CallFrameRow`
/// shape: parent indices, per-frame codehash pulled from historical state
/// (`db`), selector, EIP-150 binding, deployed-bytecode length, repricing.
/// Shared by the schedule trace and the baseline trace (F15); `codehash_cache`
/// is shared across both so a repeat target costs one DB read.
fn build_call_frame_rows<D: Database>(
    frames: &[CallFrame],
    db: &mut D,
    codehash_cache: &mut HashMap<Address, Option<B256>>,
    divergent_call_index: Option<u32>,
) -> Vec<CallFrameRow> {
    let parent_call_indices = derive_parent_call_indices(frames);
    frames
        .iter()
        .enumerate()
        .map(|(i, f)| {
            let codehash = f.to.and_then(|addr| {
                *codehash_cache.entry(addr).or_insert_with(|| match db.basic(addr) {
                    Ok(Some(info)) if info.code_hash != KECCAK_EMPTY => Some(info.code_hash),
                    _ => None,
                })
            });
            CallFrameRow {
                call_index: f.call_index as u32,
                parent_call_index: parent_call_indices[i],
                depth: f.depth as u32,
                from_address: f.from,
                // F3: `to_address` is the call/storage target (the proxy under
                // DELEGATECALL); `code_address` is the code holder (the
                // implementation, revm `bytecode_address`). For ordinary calls
                // they're equal. `codehash` is fetched from the code holder, so
                // it identifies the executed code rather than the proxy.
                to_address: f.storage_target.or(f.to).unwrap_or_default(),
                code_address: f.to,
                codehash,
                call_type: format_call_type(&f.call_type),
                selector: extract_selector_bytes(&f.input),
                value_wei: f.value_wei.map(|v| v.to_string()),
                gas_provided: f.gas_provided,
                gas_used: f.gas_used,
                gas_margin: Some((f.gas_provided as i64).saturating_sub(f.gas_used as i64)),
                success: f.success,
                parent_gas_at_call: f.parent_gas_at_call,
                gas_requested_on_stack: f.gas_requested_on_stack,
                eip150_cap_binding: eip150_cap_binding(
                    f.gas_requested_on_stack,
                    f.parent_gas_at_call,
                ),
                // For successful CREATE/CREATE2 frames the inspector stashes the
                // deployed runtime code in `output`; its length is the
                // deployed-bytecode size EIP-8037 charges CPSB per byte for. All
                // other frames leave this NULL.
                deployed_bytecode_len: match f.call_type {
                    ResCallType::Create | ResCallType::Create2 if f.success => {
                        f.output.as_ref().map(|b| b.len() as u32)
                    }
                    _ => None,
                },
                // F4: per-frame repricing surcharge the inspector accumulated.
                repricing_gas_delta: f.repricing_gas_delta,
                // F9: failing-frame context.
                caller_pc: f.caller_pc.map(|p| p as u32),
                was_precompile: f.was_precompile,
                precompile_address: f.precompile_address,
                gas_remaining_at_fail: f.gas_remaining_at_fail,
                is_divergent_frame: divergent_call_index == Some(f.call_index as u32),
            }
        })
        .collect()
}

/// Encode a block output into a durable export envelope. Runs on the DB writer
/// thread before the SQLite connection lock is taken: the payload is
/// JSON-serialized, hashed, and ZSTD-compressed here so the lock is held only
/// for the insert.
fn encode_export_envelope(
    analysis_config_hash: &str,
    output: &BlockOutput,
) -> Result<EncodedExportEnvelope, reth_research::export::ExportModelError> {
    let export_id =
        export_id(analysis_config_hash, &output.coverage.schedule_name, output.coverage.block_hash);
    let schedule_config_hash = output.coverage.schedule_config_hash.clone();
    let envelope = ExportEnvelopeV1::new(analysis_config_hash.to_string(), output.clone());
    let encoded = envelope.encode()?;
    Ok(EncodedExportEnvelope {
        export_id,
        analysis_config_hash: analysis_config_hash.to_string(),
        schedule_config_hash,
        payload_version: encoded.payload_version,
        payload_zstd: encoded.payload_zstd,
        payload_hash: encoded.payload_hash,
        payload_bytes: encoded.payload_bytes,
    })
}

/// Analyzer state shared between the live arm and concurrent backfill workers.
///
/// This struct holds everything needed to analyze a single block: the node
/// components (provider + EVM config), the configured schedules and their
/// metadata, and the channel into the DB writer thread. All fields are either
/// `Clone` (cheaply, via `Arc`-backed types) or `Copy`, so the whole struct is
/// safe to wrap in `Arc` and hand to `tokio::task::spawn_blocking` workers.
struct Analyzer<Node: FullNodeComponents> {
    /// Node components — provides access to provider() and evm_config().
    components: Node,
    /// Schedule registry containing all configured experiments
    registry: Arc<ScheduleRegistry>,
    /// All schedules in deterministic order
    all_schedules: Vec<Arc<dyn reth_research::schedule::GasSchedule>>,
    /// Execution-modifying schedules only
    execution_schedules: Vec<Arc<dyn GasSchedule>>,
    /// Lookup index into execution schedule results by schedule name.
    execution_schedule_indices: HashMap<String, usize>,
    /// Whether any configured schedule modifies intrinsic gas.
    has_intrinsic_schedules: bool,
    /// Static formatted schedule metadata reused across blocks
    schedule_metadata: HashMap<String, ScheduleMetadata>,
    /// Maximum divergence rows to persist per block.
    max_divergences_per_block: Option<usize>,
    /// Whether to collect the per-tx gas spine (`tx_gas_results`). Opt-in: it's
    /// the largest table the producer writes, and only repricing simulation
    /// needs a row for every tx rather than just the divergent tail.
    collect_tx_gas_results: bool,
    /// Tiered gas-limit-multiplier sweep applied during schedule replay.
    /// Each tier is tried in order; the first tier whose replay succeeds is
    /// accepted. Defaults to `[1, 2, 4, 8]` from the CLI layer.
    gas_limit_multipliers: Vec<u64>,
    /// Channel sender for async database writes
    db_tx: Option<mpsc::UnboundedSender<DbCommand>>,
    /// Total divergences emitted across all workers. Atomic because backfill
    /// workers run concurrently with the live arm.
    divergences_found: AtomicU64,
}

/// Research ExEx that performs multi-schedule execution analysis on committed blocks.
struct ResearchExEx<Node: FullNodeComponents> {
    /// ExEx context (owns the notifications stream + events sender).
    ctx: ExExContext<Node>,
    /// Shared analyzer state. Cloned into each backfill worker via `Arc::clone`.
    analyzer: Arc<Analyzer<Node>>,
    /// Handle for async database writer task
    db_writer_task: Option<JoinHandle<()>>,
    /// Read-side handle to the divergence DB. Used by the backfill arm to
    /// check whether a block is already covered under the current schedule
    /// configuration. Shares the underlying `Arc<Mutex<Connection>>` with the
    /// writer thread, so reads briefly block writes (and vice versa).
    divergence_db: Option<DivergenceDatabase>,
    /// Start block for live analysis
    start_block: u64,
    /// Whether to backfill historical blocks during idle windows.
    backfill_enabled: bool,
    /// Inclusive lower bound for backfill.
    backfill_min_block: u64,
    /// Inclusive upper bound for backfill; the cursor starts here (clamped to
    /// `head - 1`) instead of the chain tip. `None` starts at `head - 1`.
    backfill_max_block: Option<u64>,
    /// Maximum concurrent backfill workers (>= 1 when backfill is enabled).
    backfill_concurrency: usize,
    /// Cursor for the next backfill block, lazily initialized to `head - 1` on
    /// the first idle tick.
    next_backfill_block: Option<u64>,
    /// Set once the cursor has walked below `backfill_min_block` (or there is
    /// nothing left to dispatch), so the run loop stops trying to spawn workers.
    backfill_exhausted: bool,
    /// In-flight backfill workers. Each entry is the join handle of a
    /// `spawn_blocking` task running `Analyzer::analyze_block_by_number`.
    in_flight_backfill: FuturesUnordered<TokioJoinHandle<eyre::Result<bool>>>,
    /// Total blocks processed (live + backfill).
    blocks_processed: u64,
    /// Shutdown signal for the embedded ClickHouse export worker. `None` when
    /// export is disabled.
    export_shutdown: Option<watch::Sender<bool>>,
    /// Join handle for the export worker task. `None` when export is disabled.
    export_task: Option<TokioJoinHandle<Result<(), ExportError>>>,
    /// Fatal-error channel from the export worker. A message here (invariant or
    /// backlog-limit breach) stops the ExEx visibly; transient ClickHouse
    /// failures stay in the outbox retry loop and never use this channel.
    export_fatal_rx: Option<mpsc::Receiver<ExportError>>,
}

impl<Node> Analyzer<Node>
where
    Node: FullNodeComponents,
    Node::Evm: ConfigureEvm<
        BlockExecutorFactory: BlockExecutorFactory<
            EvmFactory: EvmFactory<Spec = SpecId, BlockEnv = BlockEnv>,
        >,
    >,
{
    fn hash_bytes(bytes: &[u8]) -> String {
        format!("{:#x}", keccak256(bytes))
    }

    fn hex_address(address: Address) -> String {
        format!("{address:#x}")
    }

    fn hex_bloom(bloom: Bloom) -> String {
        format!("{bloom:#x}")
    }

    /// Whether two call-frame sequences differ structurally.
    ///
    /// Compares the *shape* of each tx's call tree — depth, caller/callee
    /// addresses, call type, success, and calldata — while ignoring fields
    /// that always differ for gas-modifying schedules (`gas_used`,
    /// `gas_provided`, `repricing_gas_delta`, `output`). A naive `Vec`
    /// equality check flagged every gas-only repricing as a "call tree
    /// changed" divergence even when the call sequence was identical, which
    /// drowned out the genuine structural changes downstream consumers care
    /// about.
    fn call_trees_structurally_differ(left: &[CallFrame], right: &[CallFrame]) -> bool {
        if left.len() != right.len() {
            return true;
        }
        left.iter().zip(right.iter()).any(|(l, r)| {
            l.depth != r.depth ||
                l.from != r.from ||
                l.to != r.to ||
                l.call_type != r.call_type ||
                l.success != r.success ||
                l.input != r.input
        })
    }

    fn output_hash_and_len<HR>(
        result: &revm::context_interface::result::ExecutionResult<HR>,
    ) -> (Option<String>, Option<u64>) {
        result
            .output()
            .map(|output| (Some(Self::hash_bytes(output.as_ref())), Some(output.len() as u64)))
            .unwrap_or((None, None))
    }

    fn logs_bloom_hex<HR>(result: &revm::context_interface::result::ExecutionResult<HR>) -> String {
        Self::hex_bloom(logs_bloom(result.logs().iter()))
    }

    fn count_input_bytes(input: &Bytes) -> (u64, u64) {
        let zero = input.iter().filter(|&&byte| byte == 0).count() as u64;
        (zero, input.len() as u64 - zero)
    }

    fn baseline_intrinsic_gas(
        input: &Bytes,
        is_create: bool,
        access_list_accounts: u64,
        access_list_storage_slots: u64,
        authorization_list_num: u64,
        spec_id: impl Into<revm::primitives::hardfork::SpecId>,
    ) -> u64 {
        // `None`: this is the *baseline* intrinsic, computed under the block's
        // own (pre-Amsterdam) spec, where EIP-2780's decomposed base does not
        // apply. Replaying a block whose native spec is already Amsterdam would
        // need the real `Eip2780TxInfo` here.
        calculate_initial_tx_gas(
            spec_id.into(),
            input,
            is_create,
            access_list_accounts,
            access_list_storage_slots,
            authorization_list_num,
            None,
        )
        .initial_total_gas()
    }

    fn apply_signed_gas_delta(gas: u64, delta: i128) -> u64 {
        if delta >= 0 {
            gas.saturating_add(delta as u64)
        } else {
            gas.saturating_sub((-delta) as u64)
        }
    }
}

impl<Node> ResearchExEx<Node>
where
    Node: FullNodeComponents,
    Node::Evm: ConfigureEvm<
        BlockExecutorFactory: BlockExecutorFactory<
            EvmFactory: EvmFactory<Spec = SpecId, BlockEnv = BlockEnv>,
        >,
    >,
{
    /// Create a new research ExEx.
    #[allow(clippy::too_many_arguments)]
    fn new(
        ctx: ExExContext<Node>,
        registry: ScheduleRegistry,
        db_path: std::path::PathBuf,
        start_block: u64,
        max_divergences_per_block: Option<usize>,
        collect_tx_gas_results: bool,
        gas_limit_multipliers: Vec<u64>,
        backfill: bool,
        backfill_min_block: u64,
        backfill_max_block: Option<u64>,
        backfill_concurrency: usize,
        metadata_backfill_interval_secs: u64,
        contract_labels_interval_secs: u64,
        function_signatures_interval_secs: u64,
        label_config_path: Option<std::path::PathBuf>,
        export_config: Option<ExportConfig>,
    ) -> eyre::Result<Self> {
        let registry = Arc::new(registry);
        let all_schedules = registry.all();
        let execution_schedules = registry.execution_schedules();
        let execution_schedule_indices: HashMap<String, usize> = execution_schedules
            .iter()
            .enumerate()
            .map(|(idx, schedule)| (schedule.name().to_string(), idx))
            .collect();
        let has_intrinsic_schedules =
            all_schedules.iter().any(|schedule| schedule.modifies_intrinsic());
        let schedule_metadata: HashMap<String, ScheduleMetadata> = all_schedules
            .iter()
            .map(|schedule| {
                let config_hash =
                    Analyzer::<Node>::hash_bytes(schedule.config_fingerprint().as_bytes());
                (schedule.name().to_string(), ScheduleMetadata { config_hash })
            })
            .collect();

        // Normalize the gas-limit-multiplier sweep exactly once and reuse it for
        // both the deterministic export manifest and the analyzer, so the
        // dataset identity matches the data actually produced.
        let normalized_gas_tiers = normalize_gas_tiers(&gas_limit_multipliers);
        let chain_id = ctx.config.chain.chain_id();

        // Initialize database and async writer. `export_ach` is the deterministic
        // analysis-config hash to stamp on outbox rows, present only when export
        // is enabled against a real on-disk database.
        let (divergence_db, db_tx, db_writer_task, export_ach) = if db_path.to_str() !=
            Some(":memory:")
        {
            let divergence_db = DivergenceDatabase::open(&db_path)?;

            info!(
                target: "exex::research",
                path = ?db_path,
                schedules = registry.len(),
                "Research ExEx initialized with {} schedule(s)",
                registry.len()
            );

            // Log configured schedules
            for name in registry.names() {
                if let Some(schedule) = registry.get(&name) {
                    info!(
                        target: "exex::research",
                        schedule = name,
                        kind = ?schedule.kind(),
                        description = schedule.description(),
                        "Schedule configured"
                    );
                }
            }

            // Construct the deterministic export manifest and persist it as the
            // immutable dataset descriptor before any analysis runs. A durable
            // outbox row can outlive the current configuration, so the manifest
            // it was produced under must be retrievable independently.
            let export_ach = if let Some(_cfg) = export_config.as_ref() {
                let producer_git_commit = version_metadata().vergen_git_sha_long.to_string();
                let manifest = AnalysisManifestV1::build(
                    &registry,
                    normalized_gas_tiers.clone(),
                    max_divergences_per_block,
                    collect_tx_gas_results,
                    chain_id,
                    SCHEMA_VERSION,
                    producer_git_commit,
                );
                let ach = manifest
                    .analysis_config_hash()
                    .map_err(|e| eyre::eyre!("failed to derive analysis_config_hash: {e}"))?;
                let manifest_json = manifest
                    .to_json()
                    .map_err(|e| eyre::eyre!("failed to serialize export manifest: {e}"))?;
                divergence_db
                    .upsert_analysis_manifest(&AnalysisManifestRecord {
                        analysis_config_hash: ach.clone(),
                        schema_version: SCHEMA_VERSION,
                        chain_id,
                        producer_git_commit: manifest.producer_git_commit.clone(),
                        replay_semantics: REPLAY_SEMANTICS.to_string(),
                        manifest_json,
                    })
                    .map_err(|e| eyre::eyre!("failed to upsert export manifest: {e}"))?;
                info!(
                    target: "exex::research::export",
                    analysis_config_hash = %ach,
                    chain_id,
                    "ClickHouse export enabled"
                );
                Some(ach)
            } else {
                None
            };

            // Spawn database writer task. Each `BlockProcessed` command
            // lands in a single DuckDB transaction (per-block commit) so
            // a crash mid-block doesn't leave half-written aggregates. When
            // export is enabled, the block-output envelope is JSON-encoded and
            // ZSTD-compressed on this thread before the connection lock is
            // taken, then enqueued in the outbox in the same transaction.
            let (tx, mut rx) = mpsc::unbounded_channel::<DbCommand>();
            let writer_db = divergence_db.clone();
            let writer_ach = export_ach.clone();
            let writer_task = std::thread::Builder::new()
                .name("reth-research-db-writer".to_string())
                .spawn(move || {
                    let divergence_db = writer_db;
                    let mut blocks_written = 0u64;
                    while let Some(cmd) = rx.blocking_recv() {
                        match cmd {
                            DbCommand::BlockProcessed(output) => {
                                let block_number = output.coverage.block_number;
                                let schedule_name = output.coverage.schedule_name.clone();
                                let result = match writer_ach.as_deref() {
                                    Some(ach) => match encode_export_envelope(ach, &output) {
                                        Ok(export) => divergence_db
                                            .record_block_output_with_export(&output, &export),
                                        Err(error) => {
                                            // Encoding a block should never fail; if it
                                            // does, persist analytics anyway so the local
                                            // source of truth is never lost, and skip the
                                            // export for this block.
                                            warn!(
                                                target: "exex::research::db_writer",
                                                block = block_number,
                                                schedule = schedule_name,
                                                %error,
                                                "Failed to encode export envelope; recording without export"
                                            );
                                            divergence_db.record_block_output(&output)
                                        }
                                    },
                                    None => divergence_db.record_block_output(&output),
                                };
                                if let Err(error) = result {
                                    warn!(
                                        target: "exex::research::db_writer",
                                        block = block_number,
                                        schedule = schedule_name,
                                        %error,
                                        "Failed to record block output"
                                    );
                                    continue;
                                }
                                blocks_written += 1;
                                if blocks_written % 100 == 0 {
                                    debug!(
                                        target: "exex::research::db_writer",
                                        blocks_written,
                                        "Database writer progress"
                                    );
                                }
                            }
                            DbCommand::DeleteRange { from_block, to_block } => {
                                match divergence_db.delete_block_range(from_block, to_block) {
                                    Ok(counts) => {
                                        info!(
                                            target: "exex::research::db_writer",
                                            from_block,
                                            to_block,
                                            coverage = counts.coverage,
                                            summaries = counts.summaries,
                                            recipients = counts.recipients,
                                            divergences = counts.divergences,
                                            call_frames = counts.call_frames,
                                            opcode_counts = counts.opcode_counts,
                                            event_logs = counts.event_logs,
                                            outbox_pending = counts.outbox_pending,
                                            "Deleted rows for non-canonical block range"
                                        );
                                    }
                                    Err(e) => {
                                        warn!(
                                            target: "exex::research::db_writer",
                                            from_block,
                                            to_block,
                                            error = %e,
                                            "Failed to delete non-canonical rows"
                                        );
                                    }
                                }
                            }
                        }
                    }
                    info!(
                        target: "exex::research::db_writer",
                        blocks_written,
                        "Database writer task exiting"
                    );
                })
                .map_err(|err| eyre::eyre!("failed to spawn db writer thread: {err}"))?;

            (Some(divergence_db), Some(tx), Some(writer_task), export_ach)
        } else {
            if export_config.is_some() {
                warn!(
                    target: "exex::research",
                    "ClickHouse export requested but disabled: db-path is in-memory"
                );
            }
            (None, None, None, None)
        };

        // Backfill requires a real DB — there's nowhere to record dedupe
        // information against an in-memory database that disappears at shutdown.
        let backfill_enabled = backfill && divergence_db.is_some();
        if backfill && divergence_db.is_none() {
            warn!(
                target: "exex::research",
                "Backfill requested but disabled: db-path is in-memory"
            );
        }

        // Resolve concurrency: 0 means "auto" → leave one core for the live arm.
        let backfill_concurrency = if !backfill_enabled {
            0
        } else if backfill_concurrency == 0 {
            std::thread::available_parallelism()
                .map(|n| n.get().saturating_sub(1).max(1))
                .unwrap_or(1)
        } else {
            backfill_concurrency
        };

        let analyzer = Arc::new(Analyzer {
            components: ctx.components.clone(),
            registry,
            all_schedules,
            execution_schedules,
            execution_schedule_indices,
            has_intrinsic_schedules,
            schedule_metadata,
            max_divergences_per_block,
            collect_tx_gas_results,
            // Normalized once above (clamp ≥1, sort, dedup, never empty) and
            // shared with the export manifest so the dataset identity matches
            // the tiers actually replayed.
            gas_limit_multipliers: normalized_gas_tiers,
            db_tx,
            divergences_found: AtomicU64::new(0),
        });

        // Periodic contract-metadata backfill. Decoupled from block flow:
        // a tokio task wakes every N seconds and runs the SQL-prefiltered
        // incremental backfill so `contract_metadata` fills as new
        // addresses appear. Requires a real DB; in-memory mode skips it.
        if metadata_backfill_interval_secs > 0 {
            if let Some(db) = divergence_db.clone() {
                let provider = ctx.components.provider().clone();
                let interval = std::time::Duration::from_secs(metadata_backfill_interval_secs);
                tokio::spawn(periodic_metadata_backfill(db, provider, interval));
                info!(
                    target: "exex::research",
                    interval_secs = metadata_backfill_interval_secs,
                    "Periodic contract-metadata backfill enabled"
                );
            }
        }

        // Load the external-label config once. Missing path / file is
        // tolerated: defaults give a Blockscout + Sourcify chain with no
        // Etherscan rung.
        let label_config = match label_config_path.as_ref() {
            Some(p) => reth_research::external_labels::LabelBackfillConfig::from_path(p)
                .map_err(|e| eyre::eyre!("failed to load label config from {p:?}: {e}"))?,
            None => reth_research::external_labels::LabelBackfillConfig::default(),
        };

        // Periodic external contract-label backfill (Blockscout → Sourcify
        // → Etherscan). Network-bound, so runs entirely in the async
        // runtime with no spawn_blocking.
        if contract_labels_interval_secs > 0 {
            if let Some(db) = divergence_db.clone() {
                let fetcher = std::sync::Arc::new(
                    reth_research::external_labels::ContractLabelFetcher::new(&label_config),
                );
                let interval = std::time::Duration::from_secs(contract_labels_interval_secs);
                tokio::spawn(periodic_contract_label_backfill(db, fetcher, interval));
                info!(
                    target: "exex::research",
                    interval_secs = contract_labels_interval_secs,
                    etherscan_enabled = label_config.etherscan_api_key.is_some(),
                    "Periodic contract-label backfill enabled"
                );
            }
        }

        // Periodic function-signature backfill (OpenChain).
        if function_signatures_interval_secs > 0 {
            if let Some(db) = divergence_db.clone() {
                let fetcher =
                    std::sync::Arc::new(reth_research::external_labels::OpenChainFetcher::new(
                        label_config.openchain_base_url.as_deref(),
                    ));
                let interval = std::time::Duration::from_secs(function_signatures_interval_secs);
                tokio::spawn(periodic_function_signature_backfill(db, fetcher, interval));
                info!(
                    target: "exex::research",
                    interval_secs = function_signatures_interval_secs,
                    "Periodic function-signature backfill enabled"
                );
            }
        }

        // Start the embedded ClickHouse export worker when export is configured
        // against a real database. The worker drains the outbox and ships rows;
        // it shares the `DivergenceDatabase` handle (SQLite WAL) with the writer.
        let (export_shutdown, export_task, export_fatal_rx) =
            match (divergence_db.as_ref(), export_config, export_ach.as_ref()) {
                (Some(db), Some(cfg), Some(_ach)) => {
                    let (shutdown_tx, shutdown_rx) = watch::channel(false);
                    let (fatal_tx, fatal_rx) = mpsc::channel(4);
                    let task =
                        tokio::spawn(run_export_worker(db.clone(), cfg, shutdown_rx, fatal_tx));
                    (Some(shutdown_tx), Some(task), Some(fatal_rx))
                }
                _ => (None, None, None),
            };

        Ok(Self {
            ctx,
            analyzer,
            db_writer_task,
            divergence_db,
            start_block,
            backfill_enabled,
            backfill_min_block,
            backfill_max_block,
            backfill_concurrency,
            next_backfill_block: None,
            backfill_exhausted: false,
            in_flight_backfill: FuturesUnordered::new(),
            blocks_processed: 0,
            export_shutdown,
            export_task,
            export_fatal_rx,
        })
    }

    /// Run the ExEx.
    async fn run(mut self) -> eyre::Result<()> {
        info!(
            target: "exex::research",
            start_block = self.start_block,
            schedule_count = self.analyzer.registry.len(),
            backfill_enabled = self.backfill_enabled,
            backfill_min_block = self.backfill_min_block,
            backfill_max_block = ?self.backfill_max_block,
            backfill_concurrency = self.backfill_concurrency,
            "Multi-schedule Research ExEx started"
        );

        // Run loop:
        // 1. Drain any ready notifications (highest priority).
        // 2. Drain any finished backfill workers.
        // 3. Spawn new backfill workers up to `backfill_concurrency`.
        // 4. Block on whichever comes first: a notification, or a worker completing. The
        //    notification arm is biased.
        'outer: loop {
            // 1. Drain whatever notifications are immediately ready.
            loop {
                match self.ctx.notifications.try_next().now_or_never() {
                    Some(Ok(Some(notification))) => {
                        self.handle_notification(notification).await?;
                    }
                    Some(Ok(None)) => break 'outer,
                    Some(Err(e)) => return Err(e),
                    None => break,
                }
            }

            // 2. Reap finished backfill workers without blocking.
            self.drain_finished_backfill_workers();

            // 3. Spawn new backfill workers up to the configured concurrency.
            //    `next_uncovered_backfill_block` may set `backfill_exhausted`, in which case the
            //    inner loop terminates.
            while self.backfill_enabled &&
                !self.backfill_exhausted &&
                self.in_flight_backfill.len() < self.backfill_concurrency
            {
                let block_number = match self.next_uncovered_backfill_block() {
                    Ok(Some(n)) => n,
                    Ok(None) => break, // no work right now, or exhausted
                    Err(e) => {
                        warn!(target: "exex::research::backfill", error = %e, "next_uncovered_backfill_block failed");
                        break;
                    }
                };
                let analyzer = self.analyzer.clone();
                let task = tokio::task::spawn_blocking(move || {
                    analyzer.analyze_block_by_number(block_number)
                });
                self.in_flight_backfill.push(task);
            }

            // 4. If nothing is in flight and backfill is exhausted/disabled, just block on the next
            //    notification. Otherwise race notifications against worker completions.
            if self.in_flight_backfill.is_empty() {
                // Borrow disjoint fields out before select! to keep the macro happy.
                let notifications = &mut self.ctx.notifications;
                let export_fatal_rx = &mut self.export_fatal_rx;
                let outcome: SelectOutcome<<Node::Evm as ConfigureEvm>::Primitives> = tokio::select! {
                    biased;
                    notif = notifications.try_next() => {
                        match notif? {
                            Some(n) => SelectOutcome::Notification(n),
                            None => SelectOutcome::StreamClosed,
                        }
                    }
                    fatal = next_export_fatal(export_fatal_rx) => {
                        SelectOutcome::ExportFatal(fatal)
                    }
                };
                match outcome {
                    SelectOutcome::Notification(n) => self.handle_notification(n).await?,
                    SelectOutcome::StreamClosed => break,
                    SelectOutcome::ExportFatal(err) => {
                        return Err(eyre::eyre!("export worker fatal error: {err}"));
                    }
                    SelectOutcome::WorkerCompleted(_) => {}
                }
            } else {
                // Borrow disjoint fields out before select! to keep the macro happy.
                let notifications = &mut self.ctx.notifications;
                let in_flight = &mut self.in_flight_backfill;
                let export_fatal_rx = &mut self.export_fatal_rx;
                let outcome: SelectOutcome<<Node::Evm as ConfigureEvm>::Primitives> = tokio::select! {
                    biased;
                    notif = notifications.try_next() => {
                        match notif? {
                            Some(n) => SelectOutcome::Notification(n),
                            None => SelectOutcome::StreamClosed,
                        }
                    }
                    completed = in_flight.next() => {
                        SelectOutcome::WorkerCompleted(completed)
                    }
                    fatal = next_export_fatal(export_fatal_rx) => {
                        SelectOutcome::ExportFatal(fatal)
                    }
                };
                match outcome {
                    SelectOutcome::Notification(n) => self.handle_notification(n).await?,
                    SelectOutcome::StreamClosed => break,
                    SelectOutcome::ExportFatal(err) => {
                        return Err(eyre::eyre!("export worker fatal error: {err}"));
                    }
                    SelectOutcome::WorkerCompleted(Some(result)) => {
                        self.handle_backfill_completion(result);
                    }
                    SelectOutcome::WorkerCompleted(None) => {
                        // FuturesUnordered drained between `is_empty` check and select!.
                        // Nothing to do — the next iteration will re-evaluate.
                    }
                }
            }
        }

        // Shutdown: stop spawning new workers, drain in-flight ones, then
        // close the DB writer channel by dropping the analyzer (which holds
        // the only main-task-side `db_tx` clone).
        self.backfill_exhausted = true;
        while let Some(result) = self.in_flight_backfill.next().await {
            self.handle_backfill_completion(result);
        }
        drop(self.analyzer);
        if let Some(task) = self.db_writer_task.take() {
            if let Err(err) = task.join() {
                warn!(target: "exex::research", error = ?err, "Database writer task join failed during shutdown");
            }
        }

        // Stop the export worker only after the DB writer has joined, so every
        // produced outbox row is committed. Let it finish its current request
        // but don't wait for the whole backlog; pending rows stay durable.
        if let Some(shutdown) = self.export_shutdown.take() {
            let _ = shutdown.send(true);
        }
        if let Some(task) = self.export_task.take() {
            match tokio::time::timeout(std::time::Duration::from_secs(10), task).await {
                Ok(Ok(Ok(()))) => {
                    info!(target: "exex::research::export", "Export worker stopped cleanly");
                }
                Ok(Ok(Err(err))) => {
                    warn!(target: "exex::research::export", %err, "Export worker exited with error");
                }
                Ok(Err(join_err)) => {
                    warn!(target: "exex::research::export", error = %join_err, "Export worker join failed");
                }
                Err(_) => {
                    warn!(
                        target: "exex::research::export",
                        "Export worker did not stop within timeout; aborting. Pending outbox rows remain durable."
                    );
                }
            }
        }

        Ok(())
    }

    /// Reap any backfill workers that have finished, without blocking.
    fn drain_finished_backfill_workers(&mut self) {
        loop {
            let polled = self.in_flight_backfill.next().now_or_never();
            match polled {
                Some(Some(result)) => self.handle_backfill_completion(result),
                Some(None) => break, // FuturesUnordered is empty
                None => break,       // none ready
            }
        }
    }

    /// Update counters and emit logs for a finished backfill worker.
    fn handle_backfill_completion(
        &mut self,
        result: Result<eyre::Result<bool>, tokio::task::JoinError>,
    ) {
        match result {
            Ok(Ok(true)) => {
                self.blocks_processed += 1;
            }
            Ok(Ok(false)) => {
                // Worker hit a transient skip (state unavailable, block
                // missing); already logged in analyze_block_by_number.
            }
            Ok(Err(e)) => {
                warn!(
                    target: "exex::research::backfill",
                    error = %e,
                    "Backfill worker returned error"
                );
            }
            Err(join_err) => {
                warn!(
                    target: "exex::research::backfill",
                    error = %join_err,
                    "Backfill worker panicked or was cancelled"
                );
            }
        }
    }

    /// Find the next block number whose coverage rows are missing for the
    /// current schedule configuration, advancing the cursor past any blocks
    /// that are already fully covered. Returns:
    /// - `Ok(Some(n))` — block `n` is the next backfill candidate (cursor advanced past it).
    /// - `Ok(None)` — no work right now (either skipped the per-tick budget or exhausted).
    fn next_uncovered_backfill_block(&mut self) -> eyre::Result<Option<u64>> {
        // Lazily initialize the cursor on the first call.
        if self.next_backfill_block.is_none() {
            let head = self.analyzer.components.provider().best_block_number()?;
            if head == 0 {
                self.backfill_exhausted = true;
                return Ok(None);
            }
            let start = initial_backfill_cursor(head, self.backfill_max_block);
            if start < self.backfill_min_block {
                self.backfill_exhausted = true;
                info!(
                    target: "exex::research::backfill",
                    head,
                    backfill_min_block = self.backfill_min_block,
                    "Backfill exhausted before starting (head below min)"
                );
                return Ok(None);
            }
            info!(
                target: "exex::research::backfill",
                cursor = start,
                backfill_min_block = self.backfill_min_block,
                backfill_max_block = ?self.backfill_max_block,
                concurrency = self.backfill_concurrency,
                "Backfill cursor initialized"
            );
            self.next_backfill_block = Some(start);
        }

        let Some(divergence_db) = self.divergence_db.clone() else {
            self.backfill_exhausted = true;
            return Ok(None);
        };

        // Cap how far we walk per call when skipping already-covered blocks,
        // so a long covered streak still yields back to the run loop.
        const MAX_SKIPS_PER_CALL: u64 = 1024;
        let mut skipped = 0u64;

        while let Some(cursor) = self.next_backfill_block {
            if cursor < self.backfill_min_block {
                self.backfill_exhausted = true;
                info!(
                    target: "exex::research::backfill",
                    backfill_min_block = self.backfill_min_block,
                    "Backfill cursor reached lower bound — stopping"
                );
                return Ok(None);
            }
            if skipped >= MAX_SKIPS_PER_CALL {
                return Ok(None);
            }

            // Strict dedupe: every configured schedule must have a coverage
            // row at this block under its current config_hash.
            let mut all_covered = true;
            for schedule in &self.analyzer.all_schedules {
                let metadata = self
                    .analyzer
                    .schedule_metadata
                    .get(schedule.name())
                    .expect("metadata exists for every schedule");
                match divergence_db.has_block_coverage_with_config(
                    schedule.name(),
                    cursor,
                    &metadata.config_hash,
                ) {
                    Ok(true) => {}
                    Ok(false) => {
                        all_covered = false;
                        break;
                    }
                    Err(e) => {
                        warn!(
                            target: "exex::research::backfill",
                            block = cursor,
                            schedule = schedule.name(),
                            error = %e,
                            "Coverage lookup failed; treating block as uncovered"
                        );
                        all_covered = false;
                        break;
                    }
                }
            }

            // Advance cursor regardless — we either return this block as the
            // next candidate, or it's fully covered and we move on.
            self.next_backfill_block = cursor.checked_sub(1);

            if all_covered {
                skipped += 1;
                continue;
            }

            return Ok(Some(cursor));
        }

        self.backfill_exhausted = true;
        Ok(None)
    }

    /// Handle a single live `ExExNotification`.
    async fn handle_notification(
        &mut self,
        notification: ExExNotification<<Node::Evm as ConfigureEvm>::Primitives>,
    ) -> eyre::Result<()> {
        match &notification {
            ExExNotification::ChainCommitted { new } => {
                let mut highest_finished = None;
                for (_block_number, block) in new.blocks() {
                    let block_number = block.number();

                    if block_number < self.start_block {
                        highest_finished = Some(block.num_hash());
                        continue;
                    }

                    debug!(
                        target: "exex::research",
                        block = block_number,
                        tx_count = block.body().transactions().len(),
                        "Analyzing block with {} schedules",
                        self.analyzer.registry.len()
                    );

                    match self.analyzer.analyze_block(block) {
                        Ok(true) => {
                            self.blocks_processed += 1;
                            highest_finished = Some(block.num_hash());
                        }
                        Ok(false) => {
                            debug!(
                                target: "exex::research",
                                block = block_number,
                                "Deferred block analysis until historical state is available"
                            );
                            break;
                        }
                        Err(e) => {
                            warn!(
                                target: "exex::research",
                                block = block_number,
                                error = %e,
                                "Failed to analyze block"
                            );
                            break;
                        }
                    }
                }

                if let Some(num_hash) = highest_finished {
                    self.ctx.events.send(ExExEvent::FinishedHeight(num_hash))?;
                }
            }
            ExExNotification::ChainReorged { old, new } => {
                let range = old.range();
                let from_block = (*range.start()).max(self.start_block);
                let to_block = *range.end();
                if from_block <= to_block {
                    self.analyzer.send_delete_block_range(from_block, to_block);
                }

                info!(
                    target: "exex::research",
                    "Chain reorg detected, processing new chain"
                );

                let mut highest_finished = None;
                for (_block_number, block) in new.blocks() {
                    let block_number = block.number();
                    if block_number < self.start_block {
                        highest_finished = Some(block.num_hash());
                        continue;
                    }

                    match self.analyzer.analyze_block(block) {
                        Ok(true) => {
                            self.blocks_processed += 1;
                            highest_finished = Some(block.num_hash());
                        }
                        Ok(false) => {
                            debug!(
                                target: "exex::research",
                                block = block_number,
                                "Deferred reorg block analysis until historical state is available"
                            );
                            break;
                        }
                        Err(e) => {
                            warn!(
                                target: "exex::research",
                                block = block_number,
                                error = %e,
                                "Failed to analyze block after reorg"
                            );
                            break;
                        }
                    }
                }

                if let Some(num_hash) = highest_finished {
                    self.ctx.events.send(ExExEvent::FinishedHeight(num_hash))?;
                }
            }
            ExExNotification::ChainReverted { old } => {
                let range = old.range();
                let from_block = (*range.start()).max(self.start_block);
                let to_block = *range.end();
                if from_block <= to_block {
                    self.analyzer.send_delete_block_range(from_block, to_block);
                }

                info!(
                    target: "exex::research",
                    reverted_tip = old.tip().number(),
                    "Chain reverted"
                );
            }
        }
        Ok(())
    }
}

/// Carries one of three loop outcomes from `tokio::select!` so the run loop
/// can re-borrow `self` mutably to dispatch each case.
enum SelectOutcome<P: reth_node_api::NodePrimitives> {
    Notification(ExExNotification<P>),
    StreamClosed,
    WorkerCompleted(Option<Result<eyre::Result<bool>, tokio::task::JoinError>>),
    /// The export worker reported a fatal error (invariant or backlog breach).
    ExportFatal(ExportError),
}

/// Await the next fatal export error, or never resolve when export is disabled
/// or the worker's sender has been dropped. Used as a `tokio::select!` arm so a
/// fatal export failure wakes the run loop even with no backfill workers.
async fn next_export_fatal(rx: &mut Option<mpsc::Receiver<ExportError>>) -> ExportError {
    match rx.as_mut() {
        Some(receiver) => match receiver.recv().await {
            Some(err) => err,
            None => std::future::pending().await,
        },
        None => std::future::pending().await,
    }
}

impl<Node> Analyzer<Node>
where
    Node: FullNodeComponents,
    Node::Evm: ConfigureEvm<
        BlockExecutorFactory: BlockExecutorFactory<
            EvmFactory: EvmFactory<Spec = SpecId, BlockEnv = BlockEnv>,
        >,
    >,
{
    /// Load a recovered block by number and run multi-schedule analysis.
    /// Used by backfill workers — returns `Ok(false)` for transient skips
    /// (state unavailable, block missing) so the run loop can continue.
    fn analyze_block_by_number(&self, block_number: u64) -> eyre::Result<bool> {
        let recovered = match self
            .components
            .provider()
            .recovered_block(BlockHashOrNumber::Number(block_number), TransactionVariant::WithHash)
        {
            Ok(Some(block)) => block,
            Ok(None) => {
                debug!(
                    target: "exex::research::backfill",
                    block = block_number,
                    "recovered_block returned None; skipping"
                );
                return Ok(false);
            }
            Err(e) => {
                warn!(
                    target: "exex::research::backfill",
                    block = block_number,
                    error = %e,
                    "Failed to load recovered block"
                );
                return Ok(false);
            }
        };

        debug!(
            target: "exex::research::backfill",
            block = block_number,
            tx_count = recovered.body().transactions().len(),
            "Analyzing backfill block"
        );

        match self.analyze_block(&recovered) {
            Ok(true) => Ok(true),
            Ok(false) => {
                debug!(
                    target: "exex::research::backfill",
                    block = block_number,
                    "Historical state unavailable for backfill block; skipping"
                );
                Ok(false)
            }
            Err(e) => Err(e),
        }
    }

    /// Analyze a single block using multi-schedule execution.
    fn analyze_block(
        &self,
        block: &reth_primitives_traits::RecoveredBlock<BlockTy<Node::Types>>,
    ) -> eyre::Result<bool> {
        let block_number = block.number();
        let block_hash = block.hash();
        let parent_hash = block.parent_hash();
        let block_start = std::time::Instant::now();
        let provider = self.components.provider();
        let block_timestamp = block.timestamp();

        let evm_env = match self.components.evm_config().evm_env(block.header()) {
            Ok(env) => env,
            Err(e) => {
                warn!(
                    target: "exex::research",
                    block = block_number,
                    error = ?e,
                    "Failed to build EVM environment"
                );
                return Err(e.into());
            }
        };
        let spec_id = evm_env.cfg_env.spec;

        // Get state for both baseline and inspected execution once per block.
        // During initial pipeline sync the Finish stage checkpoint lags behind Execution,
        // so history_by_block_number may reject blocks that have been executed but whose
        // checkpoint hasn't been committed yet. Skip the block and let subsequent
        // notifications pick it up once the pipeline run completes.
        let baseline_state = if block_number > 0 {
            match provider.history_by_block_number(block_number - 1) {
                Ok(state) => state,
                Err(err) => {
                    debug!(
                        target: "exex::research",
                        block = block_number,
                        %err,
                        "Historical state not yet available, skipping block \
                         (expected during initial pipeline sync)"
                    );
                    return Ok(false);
                }
            }
        } else {
            provider.latest()?
        };
        let mut normal_db =
            State::builder().with_database(StateProviderDatabase::new(baseline_state)).build();

        // One BlockAggregator per schedule. Each gets fed every tx's
        // bucket classification + optional drill-in record, and we flush
        // them at the end of the block as `BlockOutput` commands.
        let mut aggregators: HashMap<String, BlockAggregator> = self
            .all_schedules
            .iter()
            .map(|schedule| {
                let metadata = self
                    .schedule_metadata
                    .get(schedule.name())
                    .expect("schedule metadata is populated for every schedule");
                let meta = BlockMeta {
                    schedule_name: schedule.name().to_string(),
                    schedule_config_hash: metadata.config_hash.clone(),
                    block_number,
                    block_hash,
                    parent_hash,
                    timestamp: block_timestamp,
                    gas_used: block.header().gas_used(),
                    gas_limit: block.header().gas_limit(),
                    base_fee_per_gas: block.header().base_fee_per_gas(),
                };
                (
                    schedule.name().to_string(),
                    BlockAggregator::start_block(meta, block.body().transactions().len()),
                )
            })
            .collect();
        let mut drill_ins_recorded = 0usize;
        let block_gas_limit = block.header().gas_limit();

        for (tx_idx, tx) in block.transactions_recovered().enumerate() {
            let tx_env = self.components.evm_config().tx_env(tx);

            // Extract transaction fields using Transaction trait
            // Use kind() to get TxKind which tells us if it's create or call
            let tx_kind = tx.kind();
            let is_create = tx_kind.is_create();
            let recipient: Option<Address> = tx_kind.to().copied();
            let gas_limit: u64 = tx.gas_limit();
            let sender = tx.signer();
            let value: U256 = tx.value();
            let input: Bytes = tx.input().clone();
            let access_list_accounts =
                tx.access_list().map(|list| list.len()).unwrap_or_default() as u64;
            let access_list_storage_slots =
                tx.access_list().map(|list| list.storage_keys_count()).unwrap_or_default() as u64;
            let authorization_count = tx.authorization_count().unwrap_or_default();
            // Fee caps for `tx_gas_results`. Rendered as decimal strings rather
            // than narrowed to u64/i64: they're u128 here and U256 in principle,
            // and a fee-market simulation should never silently see a truncated
            // cap. `max_priority_fee_per_gas` is None for legacy / access-list.
            let max_fee_per_gas = tx.max_fee_per_gas().to_string();
            let max_priority_fee_per_gas = tx.max_priority_fee_per_gas().map(|fee| fee.to_string());
            // `schedule_execution_gas_limit` is now per-tier; computed inside
            // the tier-sweep loop in the schedule replay block below.
            let baseline_intrinsic_gas = Self::baseline_intrinsic_gas(
                &input,
                is_create,
                access_list_accounts,
                access_list_storage_slots,
                authorization_count,
                spec_id,
            );
            let (input_zero_bytes, input_nonzero_bytes) = Self::count_input_bytes(&input);
            // Top-level 4-byte selector for calls with >=4 calldata bytes; None
            // for creations (init code, no selector). Computed once per tx and
            // reused by the divergence row and the drill-in record below.
            let entry_selector: Option<[u8; 4]> = (!is_create && input.len() >= 4).then(|| {
                let mut s = [0u8; 4];
                s.copy_from_slice(&input[..4]);
                s
            });
            let tx_context = if self.has_intrinsic_schedules {
                let recipient_info = match recipient {
                    Some(recipient_addr) => match normal_db.basic(recipient_addr) {
                        Ok(Some(account)) => Some(RecipientInfo {
                            exists: true,
                            has_code: account.code_hash != KECCAK_EMPTY,
                            balance: account.balance,
                            nonce: account.nonce,
                        }),
                        Ok(None) => Some(RecipientInfo {
                            exists: false,
                            has_code: false,
                            balance: U256::ZERO,
                            nonce: 0,
                        }),
                        Err(err) => {
                            debug!(
                                target: "exex::research",
                                block = block_number,
                                tx_idx,
                                recipient = ?recipient_addr,
                                %err,
                                "Failed to fetch recipient account info for transaction classification"
                            );
                            None
                        }
                    },
                    None => None,
                };

                Some(TxContext {
                    baseline_intrinsic_gas,
                    sender,
                    recipient,
                    value,
                    input: input.clone(),
                    gas_limit,
                    is_create,
                    recipient_info,
                    access_list_accounts,
                    access_list_storage_slots,
                    authorization_count,
                })
            } else {
                None
            };

            // --- EXECUTION: Baseline ---
            let mut baseline_inspector = TrackingInspector::new();
            let mut normal_evm = self.components.evm_config().evm_with_env_and_inspector(
                &mut normal_db,
                evm_env.clone(),
                &mut baseline_inspector,
            );
            let normal_result = match normal_evm.transact(tx_env.clone()) {
                Ok(result) => result,
                Err(e) => {
                    debug!(
                        target: "exex::research",
                        block = block_number,
                        tx_idx,
                        error = ?e,
                        "Normal execution failed"
                    );
                    continue;
                }
            };
            drop(normal_evm);
            let baseline_call_frames = baseline_inspector.call_frames().to_vec();
            // Baseline per-frame opcode counts (F11) — stored with
            // `trace_kind="baseline"` for drill-ins whose call tree diverged.
            let baseline_frame_opcode_counts =
                baseline_inspector.frame_opcode_counts().frames.clone();
            let baseline_event_logs = baseline_inspector.event_logs().to_vec();
            let (baseline_output_hash, baseline_output_len) =
                Self::output_hash_and_len(&normal_result.result);
            let baseline_created_address =
                normal_result.result.created_address().map(Self::hex_address);
            let baseline_logs_bloom = Self::logs_bloom_hex(&normal_result.result);
            let baseline_total_gas_spent = normal_result.result.gas().total_gas_spent();
            let baseline_gas_refunded = normal_result.result.gas().inner_refunded();

            // --- EXECUTION: Per-schedule re-execution with gas modifications ---
            // Each execution-modifying schedule gets its own full execution pass
            // so gas changes propagate naturally through subcalls.
            //
            // IMPORTANT: re-execution happens BEFORE committing baseline state so
            // that schedule runs see the same pre-tx state the baseline saw.
            // The baseline commit is deferred until after all schedule runs.
            struct PerScheduleResult {
                success: bool,
                gas_used: u64,
                total_gas_spent: u64,
                state_gas_spent: u64,
                /// State gas the tx *attempted* across all frames, including a
                /// charge that OOG'd (so it's nonzero even when state_gas_spent
                /// is 0 because the state op ran out of gas). Diagnostic.
                state_gas_demanded: u64,
                initial_state_gas: u64,
                initial_reservoir: u64,
                floor_gas: u64,
                gas_refunded: u64,
                /// `Some(true)` if the schedule replay halted with an
                /// OOG-class halt reason at the inflated replay gas limit
                /// (search exhausted — true minimum multiplier exceeds the
                /// configured `--research.gas-limit-multiplier`). `Some(false)`
                /// if it halted for a non-gas reason or reverted (no amount of
                /// gas would resolve the failure). `None` when the replay
                /// succeeded.
                replay_halt_oog: Option<bool>,
                /// Structured OOG depth (1-based), `None` for non-OOG runs.
                /// Captured from `OutOfGasInfo.call_depth` before the info is
                /// debug-formatted into the SQL `oog_info` column. Used by the
                /// chain-walk classifier to find the OOG frame in
                /// `call_frames`.
                oog_call_depth: Option<usize>,
                /// Structured `OutOfGasInfo` captured from the inspector so
                /// the DivergenceRow can mirror its fields without parsing
                /// the debug-formatted string.
                oog_info_structured: Option<reth_research::divergence::OutOfGasInfo>,
                /// Structured `DivergenceLocation`. Same rationale as above.
                divergence_location_structured: Option<DivergenceLocation>,
                /// F10: location where the cumulative surcharge first went
                /// positive (the inspector's `first_gas_divergence`).
                first_gas_divergence: Option<DivergenceLocation>,
                /// Per-frame opcode counts captured by the inspector. Used
                /// to populate `divergence_opcode_counts` rows for drill-in
                /// buckets.
                frame_opcode_counts: Vec<reth_research::divergence::FrameOpcodeCounts>,
                /// Cold account accesses this tx made (account-access opcodes),
                /// captured from the inspector's operation counts. `None` when
                /// the replay was rejected before execution (so the counts are
                /// unknown, not a real zero).
                cold_account_access_count: Option<u64>,
                /// F4: cumulative repricing surcharge the inspector charged
                /// this tx (`ScheduleResult::additional_gas`). Signed.
                additional_gas: i64,
                /// F12: per-category decomposition of `additional_gas` (their
                /// sum reconciles to it).
                tax_second_db_read: i64,
                tax_other: i64,
                /// F8: storage-reprice drivers; `None` on the reject path.
                storage_drivers: Option<StorageDrivers>,
                /// F2/F3: account-side gas drivers; `None` on the reject path.
                account_drivers: Option<AccountDrivers>,
                /// F1: tier-1 failure forensics, attached after the tier sweep.
                /// `None` unless tier-1 failed.
                tier1_diag: Option<Tier1Diagnostics>,
                /// F1: structured failure reason — the `HaltReason` discriminant
                /// (`OutOfGas`, `StackOverflow`, …), `"Revert"`, or `None` on
                /// success.
                failure_reason: Option<String>,
                /// F2: raw revert returndata (capped), best-effort decode of it
                /// (`Error(string)` / `Panic(0xNN)` / `custom:0x…` / `empty`),
                /// and the top-level tx output bytes (capped) — today only a
                /// hash is kept.
                revert_data: Option<Vec<u8>>,
                revert_decoded: Option<String>,
                tx_output: Option<Vec<u8>>,
                call_frames: Vec<CallFrame>,
                event_logs: Vec<EventLog>,
                output_hash: Option<String>,
                output_len: Option<u64>,
                created_address: Option<String>,
                logs_bloom: String,
            }

            // Indexed parallel to self.execution_schedules — accessed by schedule
            // name lookup during the analysis phase below.
            let mut schedule_results: Vec<PerScheduleResult> =
                Vec::with_capacity(self.execution_schedules.len());

            // Baseline step trace for the trace-diff divergence fallback,
            // computed lazily at most once per tx and reused across schedules.
            // Outer Option = "have we tried"; inner = "usable trace or not"
            // (None when the baseline replay errored or the trace truncated).
            let mut baseline_step_trace: Option<
                Option<Vec<reth_research::step_trace::StepRecord>>,
            > = None;

            // Each schedule executes against the pre-tx state (normal_db before
            // baseline commit) so that re-execution sees exactly the same state
            // the baseline saw. We do NOT commit schedule results — each
            // transaction is evaluated independently, so schedule-induced state
            // drift from tx N never contaminates tx N+1's analysis. The baseline
            // commit is deferred until after the re-execution loop.
            for schedule in self.execution_schedules.iter() {
                let mut schedule_evm_env = evm_env.clone();
                let native_env_configured = schedule.configure_evm_env(&mut schedule_evm_env);
                let uses_schedule_eip8037 = native_env_configured &&
                    schedule.uses_native_intrinsic_gas() &&
                    schedule_evm_env.cfg_env.is_amsterdam_eip8037_enabled();

                // Capture the per-tx state-gas reservoir budget under the schedule's
                // EVM env. For non-EIP-8037 schedules, both values stay 0 (no
                // reservoir). For EIP-8037 schedules, we recompute against the
                // *original* gas_limit so the recorded reservoir reflects what a
                // mainnet tx would have, not the inflated replay limit.
                //
                // EIP-8037 reservoir semantics (combined with EIP-7825's
                // `TX_GAS_LIMIT_CAP` of 16,777,216):
                //
                //     reservoir = max(0, tx_gas_limit - intrinsic_regular_gas
                //                        - (TX_GAS_LIMIT_CAP - intrinsic_regular_gas))
                //               = max(0, tx_gas_limit - TX_GAS_LIMIT_CAP)
                //                 [adjusted for state-gas deduction + 7702 refund]
                //
                // The reservoir is ONLY non-zero when a transaction's
                // declared gas_limit exceeds `TX_GAS_LIMIT_CAP`. Current
                // mainnet (EIP-7825 active) caps txs at 16.7M, so historical
                // replays will always observe `schedule_initial_reservoir == 0`
                // — and consequently every byte of `runtime_state_gas` will
                // appear as `runtime_state_gas_spillover`. That is the spec.
                // The reservoir mechanism only starts producing variance once
                // EIP-8037 ships and contracts opt into higher gas_limits to
                // use the state-gas budget. Until then, downstream forensics
                // should treat reservoir-utilisation panels as "all rows fall
                // in the overflow bucket" by design, not as a data bug.
                let (schedule_initial_state_gas, schedule_initial_reservoir) =
                    if uses_schedule_eip8037 {
                        let ctx = tx_context.as_ref().expect(
                            "EIP-8037 schedule modifies intrinsic gas and requires tx context",
                        );
                        let init_gas = schedule
                            .initial_and_floor_gas(ctx)
                            .expect("EIP-8037 schedule must expose its initial gas split");
                        let (_limit, reservoir) = init_gas.initial_gas_and_reservoir(
                            gas_limit,
                            revm::primitives::eip7825::TX_GAS_LIMIT_CAP,
                        );
                        (init_gas.initial_state_gas, reservoir)
                    } else {
                        (0, 0)
                    };

                // Tier-sweep loop. Try each multiplier in
                // `self.gas_limit_multipliers` (sorted ascending in the
                // constructor) and accept the first whose replay succeeds.
                // If none succeed, keep the highest tier's result so the
                // call tree / OOG info / replay_halt_oog all reflect the
                // most-funded attempt — that's the most-informative failure
                // signature for downstream forensics.
                //
                // `schedule_evm_env` was set up above with schedule-specific
                // flags; we clone it per tier so each attempt is
                // independent. The `disable_block_gas_limit /
                // disable_balance_check / tx_gas_limit_cap` overrides only
                // apply at tiers > 1, mirroring the legacy single-shot
                // behavior.
                let mut accepted: Option<PerScheduleResult> = None;
                let mut last_attempt: Option<PerScheduleResult> = None;
                // F1: tier-1 (mainnet-equivalent) failure forensics, captured
                // the moment tier-1's result is built and preserved even when a
                // later tier is accepted (and its psr discarded).
                let mut tier1_diag: Option<Tier1Diagnostics> = None;
                // Tier-1 (mainnet-equivalent) env + tx, stashed for the
                // trace-diff divergence fallback so it can reproduce the
                // failure at the original gas limit without rebuilding the
                // schedule env setup.
                let mut tier1_envs: Option<(_, _)> = None;
                // Per-schedule conditional bump: a schedule may opt into a single
                // `[1, n]` sweep (replay at 1×; only on failure, retry once at n×)
                // instead of the global multiplier sweep — EIP-8037 uses 10,
                // EIP-8038 uses 4. See `GasSchedule::replay_bump_multiplier`.
                let tiers: Vec<u64> = match schedule.replay_bump_multiplier() {
                    Some(bump) => vec![1, bump],
                    None => self.gas_limit_multipliers.clone(),
                };
                for &tier in &tiers {
                    // Cap the per-replay gas so a single unbounded-gas tx can't
                    // execute for minutes/hours (and pin a long-lived MDBX read
                    // txn); see `TIER_REPLAY_GAS_CEILING`.
                    let schedule_execution_gas_limit =
                        gas_limit.saturating_mul(tier).min(TIER_REPLAY_GAS_CEILING);

                    let mut tier_evm_env = schedule_evm_env.clone();
                    if tier > 1 {
                        tier_evm_env.cfg_env.disable_block_gas_limit = true;
                        tier_evm_env.cfg_env.disable_balance_check = true;
                        if !tier_evm_env.cfg_env.is_amsterdam_eip8037_enabled() {
                            tier_evm_env.cfg_env.tx_gas_limit_cap = Some(u64::MAX);
                        }
                    }

                    // For "Both" schedules (intrinsic + execution), adjust
                    // gas_limit so execution gets the correct budget under
                    // the new intrinsic. Non-native schedules compensate
                    // against baseline intrinsic; native EIP-8037 compensates
                    // against revm's built-in Amsterdam intrinsic because
                    // `cfg.gas_params` overrides are not used by revm's
                    // initial transaction gas helper.
                    let mut sched_tx_env = tx_env.clone();
                    sched_tx_env.set_gas_limit(schedule_execution_gas_limit);
                    // A native-intrinsic schedule needs no compensation: the EVM
                    // charges the schedule's own intrinsic, because the schedule
                    // *is* the spec table revm builds from. (Earlier revisions
                    // overlaid overrides that revm's initial-tx-gas helper
                    // ignored, so the replay limit had to absorb the difference.)
                    if schedule.modifies_intrinsic() && !schedule.uses_native_intrinsic_gas() {
                        if let Some(ref ctx) = tx_context {
                            if let Some(schedule_intrinsic) = schedule.intrinsic_gas(ctx) {
                                let intrinsic_delta = i128::from(schedule_intrinsic) -
                                    i128::from(baseline_intrinsic_gas);
                                let replay_limit = i128::from(schedule_execution_gas_limit);
                                let raw_adjusted = replay_limit - intrinsic_delta;
                                let adjusted = raw_adjusted.clamp(0, replay_limit) as u64;
                                if raw_adjusted < 0 || raw_adjusted > replay_limit {
                                    debug!(
                                        target: "exex::research",
                                        block = block_number,
                                        tx_idx,
                                        schedule = schedule.name(),
                                        tier,
                                        %intrinsic_delta,
                                        %gas_limit,
                                        %schedule_execution_gas_limit,
                                        %adjusted,
                                        "Gas limit clamped for 'Both' schedule — execution \
                                         budget may be conservative"
                                    );
                                }
                                sched_tx_env.set_gas_limit(adjusted);
                            }
                        }
                    }

                    // Stash the first tier's env (the mainnet-equivalent run)
                    // for the trace-diff fallback before the EVM consumes it.
                    if tier1_envs.is_none() {
                        tier1_envs = Some((tier_evm_env.clone(), sched_tx_env.clone()));
                    }

                    let mut inspector = ScheduleInspector::new(schedule.clone());
                    let mut evm = self.components.evm_config().evm_with_env_and_inspector(
                        &mut normal_db,
                        tier_evm_env,
                        &mut inspector,
                    );
                    let transact_result = evm.transact(sched_tx_env);
                    drop(evm);
                    let result = match transact_result {
                        Ok(result) => result,
                        Err(e) => {
                            debug!(
                                target: "exex::research",
                                block = block_number,
                                tx_idx,
                                schedule = schedule.name(),
                                tier,
                                native_env_configured,
                                error = ?e,
                                "Schedule execution failed at tier"
                            );
                            // EVM rejected the tx outright (e.g. adjusted
                            // gas_limit < schedule intrinsic at this tier).
                            // Synthesize a 1-based root-halt OOG record so
                            // downstream classification sees this as a
                            // wallet-fixable gas-class halt rather than a
                            // contract-broken signal-less revert. Without
                            // this, EIP-8037 intrinsic blow-ups (the
                            // dominant non-OOG cohort in the dashboard's
                            // 8037 contract_broken count) fall through to
                            // `oog_call_depth = None` and classify as
                            // ContractBroken even though a higher
                            // gas_limit is the only thing needed.
                            // Rejected before execution — no surcharge applied.
                            let synth_oog = OutOfGasInfo::synthetic_root("evm_reject_intrinsic");
                            last_attempt = Some(PerScheduleResult {
                                success: false,
                                gas_used: gas_limit,
                                total_gas_spent: 0,
                                state_gas_spent: 0,
                                state_gas_demanded: 0,
                                initial_state_gas: schedule_initial_state_gas,
                                initial_reservoir: schedule_initial_reservoir,
                                floor_gas: 0,
                                gas_refunded: 0,
                                replay_halt_oog: Some(true),
                                oog_call_depth: Some(synth_oog.call_depth),
                                oog_info_structured: Some(synth_oog),
                                divergence_location_structured: None,
                                first_gas_divergence: None,
                                frame_opcode_counts: Vec::new(),
                                // Rejected replay: counts unknown (not zero).
                                cold_account_access_count: None,
                                // Rejected replay never executed an opcode → no
                                // surcharge applied.
                                additional_gas: 0,
                                tax_second_db_read: 0,
                                tax_other: 0,
                                // Rejected replay: storage drivers unknown.
                                storage_drivers: None,
                                // Rejected replay: account drivers unknown.
                                account_drivers: None,
                                // Attached after the tier sweep (this is a
                                // tier-1 reject only when `tier == 1`).
                                tier1_diag: None,
                                // Rejected before execution: revm refused the tx
                                // outright.
                                failure_reason: Some("Rejected".to_string()),
                                revert_data: None,
                                revert_decoded: None,
                                tx_output: None,
                                call_frames: Vec::new(),
                                event_logs: Vec::new(),
                                output_hash: None,
                                output_len: None,
                                created_address: None,
                                logs_bloom: Self::hex_bloom(Bloom::ZERO),
                            });
                            continue;
                        }
                    };

                    let sched_success = result.result.is_success();
                    let mut sched_gas_used = result.result.tx_gas_used();
                    let mut sched_total_gas_spent = result.result.gas().total_gas_spent();
                    let sched_state_gas_spent = result.result.gas().state_gas_spent_final();
                    // Runtime state gas the tx attempted (survives OOG); raw —
                    // not intrinsic-normalized, since it's a sum of runtime
                    // record_state_cost charges, not the tx-start intrinsic.
                    let sched_state_gas_demanded = result.result.gas().state_gas_demanded();
                    // A native-intrinsic schedule needs no normalization: revm
                    // already reported these totals against the schedule's own
                    // intrinsic, since the schedule is the spec table revm built
                    // from. Only a schedule that reprices the intrinsic *outside*
                    // the EVM env needs the correction below.
                    if schedule.modifies_intrinsic() &&
                        !schedule.uses_native_intrinsic_gas() &&
                        let Some(ctx) = tx_context.as_ref() &&
                        let Some(schedule_intrinsic) = schedule.intrinsic_gas(ctx)
                    {
                        // Non-native "Both" schedule (one that reprices
                        // intrinsic gas via the inspector rather than the EVM
                        // env): revm deducted the block's *native* intrinsic
                        // during execution, so the reported regular gas reflects
                        // baseline intrinsic, not the schedule's. Normalize by
                        // the intrinsic delta so `gas_delta` includes the
                        // schedule's intrinsic repricing. State gas is
                        // unaffected (zero here).
                        let intrinsic_delta =
                            i128::from(schedule_intrinsic) - i128::from(baseline_intrinsic_gas);
                        sched_gas_used =
                            Self::apply_signed_gas_delta(sched_gas_used, intrinsic_delta);
                        sched_total_gas_spent =
                            Self::apply_signed_gas_delta(sched_total_gas_spent, intrinsic_delta);
                    }
                    let sched_floor_gas = result.result.gas().floor_gas();
                    let sched_gas_refunded = result.result.gas().inner_refunded();
                    let insp_result = inspector.result();
                    let halt_reason_debug = match &result.result {
                        revm::context_interface::result::ExecutionResult::Halt {
                            reason, ..
                        } => Some(format!("{reason:?}")),
                        _ => None,
                    };
                    // F1: structured failure reason — halt discriminant, Revert,
                    // or None on success.
                    let failure_reason = match &result.result {
                        revm::context_interface::result::ExecutionResult::Halt {
                            reason, ..
                        } => Some(format!("{reason:?}")),
                        revm::context_interface::result::ExecutionResult::Revert { .. } => {
                            Some("Revert".to_string())
                        }
                        revm::context_interface::result::ExecutionResult::Success { .. } => None,
                    };
                    // F2: revert returndata (capped) + best-effort decode, and
                    // the top-level tx output bytes (capped — today hash-only).
                    let revert_data = match &result.result {
                        revm::context_interface::result::ExecutionResult::Revert {
                            output, ..
                        } => Some(cap_bytes(output)),
                        _ => None,
                    };
                    let revert_decoded = revert_data.as_deref().map(decode_revert);
                    let tx_output = result.result.output().map(|o| cap_bytes(o));
                    // Halt classification at this tier:
                    //   Halt { OutOfGas* } → Some(true)  (more gas might help)
                    //   Halt { other }     → Some(false) (no amount of gas helps)
                    //   Revert             → Some(false)
                    //   Success            → None        (no halt at all)
                    let replay_halt_oog = match &result.result {
                        revm::context_interface::result::ExecutionResult::Halt { .. } => {
                            let is_oog = halt_reason_debug
                                .as_deref()
                                .is_some_and(|reason| reason.starts_with("OutOfGas"));
                            Some(is_oog)
                        }
                        revm::context_interface::result::ExecutionResult::Revert { .. } => {
                            Some(false)
                        }
                        revm::context_interface::result::ExecutionResult::Success { .. } => None,
                    };
                    let (output_hash, output_len) = Self::output_hash_and_len(&result.result);
                    let created_address = result.result.created_address().map(Self::hex_address);
                    let logs_bloom = Self::logs_bloom_hex(&result.result);
                    let call_frames = inspector.call_frames().to_vec();
                    let event_logs = inspector.event_logs().to_vec();

                    // Synthesize a root-frame OOG record when the tx halted with
                    // OutOfGas but the inspector didn't capture any per-frame
                    // oog_info. See the equivalent legacy comment for the full
                    // motivation; nothing tier-specific.
                    let inspector_oog_info = insp_result.oog_info.clone().or_else(|| {
                        // Synthesized at the producer — surcharge unknown here.
                        (replay_halt_oog == Some(true))
                            .then(|| OutOfGasInfo::synthetic_root("root_halt"))
                    });

                    let psr = PerScheduleResult {
                        success: sched_success,
                        gas_used: sched_gas_used,
                        total_gas_spent: sched_total_gas_spent,
                        state_gas_spent: sched_state_gas_spent,
                        state_gas_demanded: sched_state_gas_demanded,
                        initial_state_gas: schedule_initial_state_gas,
                        initial_reservoir: schedule_initial_reservoir,
                        floor_gas: sched_floor_gas,
                        gas_refunded: sched_gas_refunded,
                        replay_halt_oog,
                        oog_call_depth: inspector_oog_info.as_ref().map(|oog| oog.call_depth),
                        oog_info_structured: inspector_oog_info.clone(),
                        divergence_location_structured: insp_result.divergence_location.clone(),
                        first_gas_divergence: insp_result.first_gas_divergence.clone(),
                        frame_opcode_counts: inspector.frame_opcode_counts().frames.clone(),
                        cold_account_access_count: Some(
                            inspector.operation_counts().cold_account_access_count,
                        ),
                        additional_gas: insp_result.additional_gas,
                        tax_second_db_read: inspector.operation_counts().tax_second_db_read,
                        tax_other: inspector.operation_counts().tax_other,
                        storage_drivers: Some(StorageDrivers::from_counts(
                            inspector.operation_counts(),
                        )),
                        // F2/F3: account-side drivers (inspector counts + the
                        // tx-envelope access-list counts).
                        account_drivers: Some(AccountDrivers::from_parts(
                            inspector.operation_counts(),
                            access_list_accounts,
                            access_list_storage_slots,
                        )),
                        // F1: filled after the tier sweep (see below) only for
                        // the chosen result, and only when tier-1 failed.
                        tier1_diag: None,
                        failure_reason,
                        revert_data,
                        revert_decoded,
                        tx_output,
                        call_frames,
                        event_logs,
                        output_hash,
                        output_len,
                        created_address,
                        logs_bloom,
                    };

                    // F1: capture tier-1's failure forensics before psr is moved.
                    // Read entirely from psr (which already folded in the
                    // inspector's oog_info + call frames). The innermost failing
                    // frame is the deepest `!success` frame — the same rule that
                    // marks the divergent/bottleneck frame downstream.
                    if tier == 1 && !sched_success {
                        let failing =
                            psr.call_frames.iter().filter(|f| !f.success).max_by_key(|f| f.depth);
                        tier1_diag = Some(Tier1Diagnostics::from_parts(
                            psr.failure_reason.clone(),
                            psr.oog_info_structured.as_ref(),
                            failing,
                        ));
                    }

                    if sched_success {
                        // Smallest tier that succeeded — accept and stop.
                        // `replay_halt_oog` is None for a successful tier,
                        // matching the DB-column contract.
                        accepted = Some(psr);
                        break;
                    }
                    // Keep the highest-tier failed attempt as the fallback
                    // (loop tries tiers in ascending order, so each
                    // assignment overwrites a lower-tier failure).
                    last_attempt = Some(psr);
                }

                // At least one tier always runs (constructor enforces non-empty),
                // so unwrapping is safe. Successful sweep wins; otherwise we
                // keep the highest-tier failure to carry the OOG / halt signal.
                let mut chosen = accepted.or(last_attempt).expect("tier loop ran at least once");
                // F1: attach tier-1's failure forensics to whichever attempt was
                // chosen (a rescue's psr describes the bumped tier, not the 1×
                // failure; this preserves the 1× break). `None` when tier-1
                // succeeded.
                chosen.tier1_diag = tier1_diag.take();

                // Trace-diff divergence for the "non-OOG schedule revert"
                // cohort: a status flip to failure that the inspector left
                // undiagnosed (no per-opcode divergence fired, no OOG). Only
                // native-revm schedules qualify — their gas changes live in the
                // EVM env, so re-running under that env with a plain step-tracer
                // reproduces the schedule's execution exactly, and diffing
                // against the baseline trace pinpoints the first step whose
                // control flow changed. Non-native schedules (e.g. 7904) apply
                // gas via the inspector, so they already record a divergence and
                // never reach here.
                if chosen.divergence_location_structured.is_none() &&
                    chosen.replay_halt_oog != Some(true) &&
                    native_env_configured &&
                    normal_result.result.is_success() &&
                    !chosen.success
                {
                    if let Some((diff_evm_env, diff_tx_env)) = tier1_envs.take() {
                        let root_contract = recipient.unwrap_or(Address::ZERO);

                        // Baseline trace (once per tx; pre-tx state is unchanged
                        // across schedules since baseline commit is deferred).
                        if baseline_step_trace.is_none() {
                            let mut insp = StepTraceInspector::new(root_contract);
                            let mut evm = self.components.evm_config().evm_with_env_and_inspector(
                                &mut normal_db,
                                evm_env.clone(),
                                &mut insp,
                            );
                            let ok = evm.transact(tx_env.clone()).is_ok();
                            drop(evm);
                            baseline_step_trace =
                                Some((ok && !insp.truncated()).then(|| insp.steps().to_vec()));
                        }

                        // Schedule trace under the tier-1 (mainnet-equivalent) env.
                        let mut sched_insp = StepTraceInspector::new(root_contract);
                        let mut sched_evm =
                            self.components.evm_config().evm_with_env_and_inspector(
                                &mut normal_db,
                                diff_evm_env,
                                &mut sched_insp,
                            );
                        let sched_ok = sched_evm.transact(diff_tx_env).is_ok();
                        drop(sched_evm);

                        if let Some(Some(base_steps)) = baseline_step_trace.as_ref() {
                            if sched_ok && !sched_insp.truncated() {
                                if let Some(loc) = first_divergence(base_steps, sched_insp.steps())
                                {
                                    chosen.divergence_location_structured = Some(loc);
                                }
                            }
                        }
                    }
                }

                schedule_results.push(chosen);
            }

            // Commit baseline state AFTER all schedule re-executions so that
            // (a) schedule runs saw pre-tx state, and (b) the next tx's baseline
            // sees the correct post-tx state.
            let normal_gas_used = normal_result.result.tx_gas_used();
            let normal_success = normal_result.result.is_success();
            let tx_hash = *tx.tx_hash();
            normal_db.commit(normal_result.state);

            // --- ANALYZE EACH SCHEDULE ---
            for schedule in &self.all_schedules {
                let schedule_name = schedule.name();
                let schedule_kind = schedule.kind();

                // Calculate intrinsic gas for intrinsic-modifying schedules
                let (intrinsic_delta, schedule_intrinsic_gas, tx_category) = match schedule_kind {
                    ScheduleKind::IntrinsicOnly | ScheduleKind::Both => {
                        let tx_context = tx_context.as_ref().expect(
                            "intrinsic schedule requires tx context; context must be present",
                        );
                        let schedule_intrinsic =
                            schedule.intrinsic_gas(tx_context).unwrap_or(baseline_intrinsic_gas);
                        let delta = schedule_intrinsic as i64 - baseline_intrinsic_gas as i64;
                        let category = schedule.tx_category(tx_context);
                        (delta, Some(schedule_intrinsic), category)
                    }
                    ScheduleKind::ExecutionOnly => (0i64, None, None),
                    ScheduleKind::None => continue,
                };

                // Native-intrinsic schedules (EIP-8037, EIP-8038) have the EVM
                // charge the schedule intrinsic directly — the vendored handler's
                // `validate_initial_tx_gas` reads `cfg.gas_params`, which
                // `configure_evm_env` overrode — and EIP-8037's reservoir path
                // already normalized the stored regular gas in the execution loop.
                // So the analyze-path re-add below must fire ONLY for non-native
                // intrinsic schedules; re-adding it for a native one would count
                // the schedule intrinsic twice (C1). The true `intrinsic_delta` is
                // still recorded via `schedule_intrinsic_gas` / `tax_intrinsic`.
                let report_intrinsic_delta =
                    if schedule.uses_native_intrinsic_gas() { 0 } else { intrinsic_delta };

                // Look up the re-execution result for this schedule. For a
                // non-native "Both" schedule the EVM deducted the block's native
                // intrinsic, so we add `report_intrinsic_delta` to recover the
                // schedule's true gas_used; for native schedules it is already
                // included (report_intrinsic_delta == 0).
                let exec_result = self
                    .execution_schedule_indices
                    .get(schedule_name)
                    .and_then(|&idx| schedule_results.get(idx));
                let (
                    schedule_gas,
                    schedule_success,
                    schedule_replay_success,
                    schedule_total_gas_spent,
                    schedule_state_gas_spent,
                    schedule_state_gas_demanded,
                    schedule_initial_state_gas,
                    schedule_initial_reservoir,
                    schedule_floor_gas,
                    schedule_gas_refunded,
                    replay_halt_oog,
                    call_tree_diverged,
                    event_logs_diverged,
                    output_changed,
                    created_address_changed,
                    logs_bloom_changed,
                ) = match exec_result {
                    Some(r) => {
                        // Non-native "Both" schedules: the EVM deducted the
                        // block's native intrinsic, so replace it with the
                        // schedule intrinsic (`gas_used + report_intrinsic_delta`).
                        // Native schedules (EIP-8037/8038) already have the
                        // schedule intrinsic in `r.gas_used`, so
                        // `report_intrinsic_delta == 0` and gas_used is unchanged.
                        let gas = if report_intrinsic_delta != 0 {
                            (r.gas_used as i64 + report_intrinsic_delta).max(0) as u64
                        } else {
                            r.gas_used
                        };
                        let success = r.success && gas <= gas_limit;
                        let call_tree_diverged = Self::call_trees_structurally_differ(
                            &r.call_frames,
                            &baseline_call_frames,
                        );
                        let event_logs_diverged = r.event_logs != baseline_event_logs;
                        let output_changed = r.output_hash != baseline_output_hash ||
                            r.output_len != baseline_output_len;
                        let created_address_changed = r.created_address != baseline_created_address;
                        let logs_bloom_changed = r.logs_bloom != baseline_logs_bloom;
                        (
                            gas,
                            success,
                            r.success,
                            r.total_gas_spent,
                            r.state_gas_spent,
                            r.state_gas_demanded,
                            r.initial_state_gas,
                            r.initial_reservoir,
                            r.floor_gas,
                            r.gas_refunded,
                            r.replay_halt_oog,
                            call_tree_diverged,
                            event_logs_diverged,
                            output_changed,
                            created_address_changed,
                            logs_bloom_changed,
                        )
                    }
                    None => {
                        // Intrinsic-only schedule: no execution result, so the
                        // schedule-side state-gas / reservoir / refund counters
                        // stay zero — only the baseline equivalents are meaningful.
                        let gas = (normal_gas_used as i64 + report_intrinsic_delta).max(0) as u64;
                        let success = normal_success && gas <= gas_limit;
                        (
                            gas,
                            success,
                            normal_success,
                            0,
                            0,
                            0, // schedule_state_gas_demanded (no schedule run)
                            0,
                            0,
                            0,
                            0,
                            None, // replay_halt_oog (no schedule run)
                            false,
                            false,
                            false,
                            false,
                            false,
                        )
                    }
                };

                // Use the actual gas difference as the total delta rather
                // than the inspector's cumulative opcode deltas, since
                // cascading effects (different execution paths) make the
                // true difference diverge from the sum of per-opcode
                // adjustments.
                //
                // NOTE: when the tx failed at 1× but was rescued at a bumped
                // tier (`schedule_success=0 AND replay_halt_oog IS NULL`),
                // `schedule_gas` is the rescued (N×) tier's gas, so `total_delta`
                // is NOT a meaningful repricing measure here — and it's
                // meaningless whenever `baseline_success=0`. Downstream repricing
                // aggregates must filter on `schedule_success=1` (and
                // `baseline_success=1`).
                let total_delta = schedule_gas as i64 - normal_gas_used as i64;
                // Decompose the outcome flip into its two directions: a break
                // (baseline succeeded, schedule failed) drives the
                // `outer_limit_only_failure` witness, while `status_changed`
                // records the bidirectional flip on the `DivergenceRow`.
                let baseline_to_schedule_break = normal_success && !schedule_success;
                let baseline_to_schedule_rescue = !normal_success && schedule_success;
                let status_changed = baseline_to_schedule_break || baseline_to_schedule_rescue;
                let observable_trace_matches_baseline = !event_logs_diverged &&
                    !call_tree_diverged &&
                    !output_changed &&
                    !created_address_changed &&
                    !logs_bloom_changed;
                let outer_limit_only_failure = baseline_to_schedule_break &&
                    schedule_replay_success &&
                    schedule_gas > gas_limit &&
                    schedule_gas <= block_gas_limit &&
                    observable_trace_matches_baseline;

                // Min gas-limit multiplier required for the schedule to
                // succeed. Only meaningful when the replay actually
                // completed under the (possibly inflated) replay limit;
                // otherwise the gas_used we'd divide by is the OOG cap,
                // not the true minimum.
                let min_multiplier_to_succeed = if schedule_replay_success && gas_limit > 0 {
                    Some(schedule_gas as f64 / gas_limit as f64)
                } else {
                    None
                };

                // Classify the OOG chain (root → OOG frame) so the
                // dashboard can distinguish wallet-fixable failures (every
                // hop received gas via the EIP-150 63/64 rule) from
                // contract-bottlenecked ones (some frame throttled gas
                // with `.transfer()` 2300 stipend, fixed constant, or
                // fractional pattern). Only meaningful for OOG-class
                // divergences — non-OOG rows leave these fields NULL.
                let (oog_chain_proportional, oog_bottleneck_depth, oog_bottleneck_kind) =
                    match exec_result.and_then(|r| {
                        r.oog_call_depth.and_then(|depth| classify_oog_chain(&r.call_frames, depth))
                    }) {
                        Some(analysis) => (
                            Some(analysis.proportional),
                            analysis.bottleneck_depth,
                            analysis.bottleneck_kind.map(|k| k.as_str().to_string()),
                        ),
                        None => (None, None, None),
                    };

                // Reduce the comparison to raw execution facts. The producer no
                // longer applies an editorial taxonomy (wallet-fixable /
                // contract-broken / aa-reestimation / …); it stores a full
                // per-tx forensic row for every failure and trace divergence and
                // rolls the byte-identical / gas-only remainder into a 2-value
                // aggregate class. Downstream re-derives the old cohorts from the
                // stored facts.
                let facts = DivergenceFacts {
                    baseline_success: normal_success,
                    schedule_success,
                    gas_delta: total_delta,
                    event_logs_changed: event_logs_diverged,
                    call_tree_changed: call_tree_diverged,
                    output_changed,
                    created_address_changed,
                    logs_bloom_changed,
                };
                let store_full_forensics = facts.store_full_forensics();
                let aggregate_class = facts.aggregate_class();

                // For stored txs (failures + trace divergences), build the full
                // per-tx record.
                let drill_in = if store_full_forensics {
                    let cap_reached = self
                        .max_divergences_per_block
                        .map(|max| drill_ins_recorded >= max)
                        .unwrap_or(false);
                    drill_ins_recorded += 1;
                    if cap_reached {
                        if drill_ins_recorded - 1 ==
                            self.max_divergences_per_block.unwrap_or_default()
                        {
                            warn!(
                                target: "exex::research",
                                block = block_number,
                                max_divergences_per_block = self.max_divergences_per_block,
                                "Reached drill-in persistence limit for block; additional drill-ins are counted in coverage only"
                            );
                        }
                        None
                    } else {
                        self.divergences_found.fetch_add(1, Ordering::Relaxed);
                        let metadata = self
                            .schedule_metadata
                            .get(schedule_name)
                            .cloned()
                            .expect("all schedules should have static metadata");
                        let div_loc =
                            exec_result.and_then(|r| r.divergence_location_structured.clone());
                        let gas_div = exec_result.and_then(|r| r.first_gas_divergence.clone());
                        let oog_info_s = exec_result.and_then(|r| r.oog_info_structured.clone());
                        // F1: tier-1 failure forensics, pre-bound once for the
                        // nine `tier1_*` columns below (None unless tier-1 failed).
                        let tier1 = exec_result.and_then(|r| r.tier1_diag.as_ref());
                        let frames_ref: &[CallFrame] =
                            exec_result.map(|r| r.call_frames.as_slice()).unwrap_or(&[]);
                        let opcode_frames_ref =
                            exec_result.map(|r| r.frame_opcode_counts.as_slice()).unwrap_or(&[]);
                        let sched_logs_ref: &[EventLog] =
                            exec_result.map(|r| r.event_logs.as_slice()).unwrap_or(&[]);

                        // Cold-account-access count for this tx under this
                        // schedule (F4). `Some(0)` is a *measured* zero (the
                        // replay ran and made no cold access); NULL is reserved
                        // for the unmeasured reject path (`exec_result` /
                        // `cold_account_access_count` is None). Do NOT collapse
                        // measured-zero to NULL — that conflates the two.
                        let cold_account_access_count =
                            exec_result.and_then(|r| r.cold_account_access_count);

                        // F7: baseline counterpart of the frame that failed under
                        // the schedule. The innermost failing frame (deepest
                        // `!success`) is the bottleneck; its baseline twin —
                        // matched by call_index when the pre-divergence structure
                        // lines up (always true for pure-OOG failures) — shows
                        // whether baseline ran that frame and with how much gas.
                        // The cleanest wallet-fixable (baseline succeeded, more
                        // gas would too) vs contract-broken discriminator.
                        let failing_frame =
                            frames_ref.iter().filter(|f| !f.success).max_by_key(|f| f.depth);
                        // F9: the innermost failing frame's call_index marks the
                        // divergent (bottleneck) frame on the schedule side.
                        let failing_call_index = failing_frame.map(|f| f.call_index as u32);
                        let baseline_twin = failing_frame.and_then(|ff| {
                            baseline_call_frames.iter().find(|bf| {
                                bf.call_index == ff.call_index &&
                                    bf.depth == ff.depth &&
                                    bf.to == ff.to
                            })
                        });

                        let divergence_row = DivergenceRow {
                            schedule_name: schedule_name.to_string(),
                            schedule_config_hash: metadata.config_hash.clone(),
                            block_number,
                            tx_index: tx_idx as u32,
                            tx_hash,
                            timestamp: block_timestamp,
                            // Witness so a tx that only failed because the
                            // schedule pushed gas_used past the *original* limit
                            // (succeeded at the bumped tier, trace matched
                            // baseline) is distinguishable downstream from a
                            // genuine OOG break.
                            outer_limit_only_failure: Some(outer_limit_only_failure),
                            sender,
                            recipient,
                            is_create,
                            tx_gas_limit: gas_limit,
                            baseline_success: normal_success,
                            schedule_success,
                            status_changed,
                            event_logs_changed: event_logs_diverged,
                            output_changed,
                            logs_bloom_changed,
                            baseline_gas_used: normal_gas_used,
                            schedule_gas_used: schedule_gas,
                            gas_delta: total_delta,
                            baseline_total_gas_spent: Some(baseline_total_gas_spent),
                            baseline_gas_refunded: Some(baseline_gas_refunded),
                            schedule_total_gas_spent: Some(schedule_total_gas_spent),
                            schedule_gas_refunded: Some(schedule_gas_refunded),
                            schedule_intrinsic_gas,
                            schedule_floor_gas: Some(schedule_floor_gas),
                            min_multiplier_to_succeed,
                            divergence_contract: div_loc.as_ref().map(|l| l.contract),
                            divergence_pc: div_loc.as_ref().map(|l| l.pc as u32),
                            divergence_call_depth: div_loc.as_ref().map(|l| l.call_depth as i32),
                            divergence_opcode: div_loc.as_ref().map(|l| l.opcode),
                            oog_contract: oog_info_s.as_ref().map(|o| o.contract),
                            oog_pc: oog_info_s.as_ref().map(|o| o.pc as u32),
                            oog_call_depth: oog_info_s.as_ref().map(|o| o.call_depth as i32),
                            oog_opcode: oog_info_s.as_ref().map(|o| o.opcode),
                            oog_pattern: oog_info_s.as_ref().map(|o| o.pattern.to_string()),
                            oog_gas_remaining: oog_info_s.as_ref().map(|o| o.gas_remaining),
                            oog_chain_proportional,
                            oog_bottleneck_depth: oog_bottleneck_depth.map(|d| d as i32),
                            oog_bottleneck_kind: oog_bottleneck_kind.clone(),
                            schedule_state_gas_spent: Some(schedule_state_gas_spent),
                            schedule_state_gas_demanded: Some(schedule_state_gas_demanded),
                            schedule_initial_state_gas: Some(schedule_initial_state_gas),
                            schedule_initial_reservoir: Some(schedule_initial_reservoir),
                            // Derived 8037 figures, mirroring what
                            // `BlockAggregator` computes per bucket:
                            //   runtime_state_gas = state_gas_spent − initial_state_gas
                            //   spillover         = runtime_state_gas − initial_reservoir
                            //   reservoir_exhausted = spillover > 0
                            // All saturate to zero on underflow.
                            runtime_state_gas: Some(
                                schedule_state_gas_spent.saturating_sub(schedule_initial_state_gas),
                            ),
                            runtime_state_gas_spillover: Some(
                                schedule_state_gas_spent
                                    .saturating_sub(schedule_initial_state_gas)
                                    .saturating_sub(schedule_initial_reservoir),
                            ),
                            state_gas_category: tx_category.map(|s| s.to_string()),
                            reservoir_exhausted: Some(
                                schedule_state_gas_spent
                                    .saturating_sub(schedule_initial_state_gas)
                                    .saturating_sub(schedule_initial_reservoir) >
                                    0,
                            ),
                            // Set from the tier-sweep loop above: Some(true/false)
                            // when no tier succeeded, None when at least one did.
                            replay_halt_oog,
                            cold_account_access_count,
                            // F4: total repricing surcharge across all frames.
                            additional_gas_charged: exec_result.map(|r| r.additional_gas),
                            // F6: root→divergence 4-byte selector path as a JSON
                            // array of nullable hex strings.
                            failure_selector_path: div_loc.as_ref().and_then(|l| {
                                let path: Vec<Option<String>> = l
                                    .function_selectors
                                    .iter()
                                    .map(|s| s.map(|b| format!("0x{:08x}", u32::from_be_bytes(b))))
                                    .collect();
                                serde_json::to_string(&path).ok()
                            }),
                            // F5: top-level tx identity.
                            tx_type: Some(tx.ty()),
                            tx_nonce: Some(tx.nonce()),
                            entry_selector,
                            input_zero_bytes: Some(input_zero_bytes),
                            input_nonzero_bytes: Some(input_nonzero_bytes),
                            has_authorization: Some(authorization_count > 0),
                            // F1/F2: structured failure reason + revert/return data.
                            failure_reason: exec_result.and_then(|r| r.failure_reason.clone()),
                            revert_data: exec_result.and_then(|r| r.revert_data.clone()),
                            revert_decoded: exec_result.and_then(|r| r.revert_decoded.clone()),
                            tx_output: exec_result.and_then(|r| r.tx_output.clone()),
                            // F7: baseline counterpart of the failing frame.
                            baseline_frame_success: baseline_twin.map(|bf| bf.success),
                            baseline_frame_gas_used: baseline_twin.map(|bf| bf.gas_used),
                            baseline_frame_gas_provided: baseline_twin.map(|bf| bf.gas_provided),
                            // F13: repricing surcharge at the OOG instant.
                            surcharge_at_oog: oog_info_s.as_ref().map(|o| o.additional_gas_at_oog),
                            // F10: first opcode where cumulative gas exceeded baseline.
                            gas_div_contract: gas_div.as_ref().map(|l| l.contract),
                            gas_div_pc: gas_div.as_ref().map(|l| l.pc as u32),
                            gas_div_call_depth: gas_div.as_ref().map(|l| l.call_depth as i32),
                            gas_div_opcode: gas_div.as_ref().map(|l| l.opcode),
                            // F12: per-category tax breakdown. The opcode-delta
                            // categories sum to additional_gas_charged; intrinsic
                            // is the separate tx-level intrinsic delta.
                            tax_second_db_read: exec_result.map(|r| r.tax_second_db_read),
                            tax_other: exec_result.map(|r| r.tax_other),
                            tax_intrinsic: schedule_intrinsic_gas
                                .map(|s| s as i64 - baseline_intrinsic_gas as i64),
                            // F8: storage-reprice drivers.
                            storage_drivers: exec_result.and_then(|r| r.storage_drivers),
                            // F2/F3: account-side gas drivers.
                            account_drivers: exec_result.and_then(|r| r.account_drivers),
                            // F1: tier-1 failure forensics (None unless tier-1
                            // failed). The accepted attempt's oog_*/frame columns
                            // describe the chosen tier; these describe the 1× run.
                            tier1_failure_reason: tier1.and_then(|d| d.failure_reason.clone()),
                            tier1_oog_opcode: tier1.and_then(|d| d.oog_opcode),
                            tier1_oog_contract: tier1.and_then(|d| d.oog_contract),
                            tier1_oog_pc: tier1.and_then(|d| d.oog_pc),
                            tier1_oog_depth: tier1.and_then(|d| d.oog_depth),
                            tier1_oog_gas_remaining: tier1.and_then(|d| d.oog_gas_remaining),
                            tier1_failing_selector: tier1.and_then(|d| d.failing_selector),
                            tier1_failing_gas_provided: tier1.and_then(|d| d.failing_gas_provided),
                            tier1_failing_gas_requested: tier1
                                .and_then(|d| d.failing_gas_requested),
                        };

                        // Codehashes come from the historical state we already
                        // loaded (`normal_db`); the cache is shared across the
                        // schedule and baseline traces so a repeat target costs
                        // one `basic()` read. Self-destructed / missing accounts
                        // leave the column NULL rather than aborting the record.
                        let mut codehash_cache: HashMap<Address, Option<B256>> = HashMap::new();
                        let call_frames_rows = build_call_frame_rows(
                            frames_ref,
                            &mut normal_db,
                            &mut codehash_cache,
                            failing_call_index,
                        );
                        let opcode_count_rows = OpcodeCountRow::from_frames(opcode_frames_ref);

                        // F11/F15: keep the baseline trace too, but only when the
                        // call tree actually diverged (pure-OOG failures already
                        // carry F7's baseline-frame twin, so the full baseline
                        // tree would be redundant there). Baseline frames carry no
                        // divergent-frame flag (None).
                        let (baseline_call_frame_rows, baseline_opcode_rows) = if call_tree_diverged
                        {
                            (
                                build_call_frame_rows(
                                    &baseline_call_frames,
                                    &mut normal_db,
                                    &mut codehash_cache,
                                    None,
                                ),
                                OpcodeCountRow::from_frames(&baseline_frame_opcode_counts),
                            )
                        } else {
                            (Vec::new(), Vec::new())
                        };

                        Some(DrillInRecord {
                            divergence: divergence_row,
                            call_frames: call_frames_rows,
                            opcode_counts: opcode_count_rows,
                            baseline_call_frames: baseline_call_frame_rows,
                            baseline_opcode_counts: baseline_opcode_rows,
                            baseline_event_logs: baseline_event_logs.clone(),
                            schedule_event_logs: sched_logs_ref.to_vec(),
                        })
                    }
                } else {
                    None
                };

                // Derive the runtime state-gas decomposition for the
                // 8037 aggregates: `runtime_state_gas = state_gas_spent -
                // initial_state_gas`, and the spillover is whatever
                // exceeded the per-tx reservoir.
                let runtime_state_gas =
                    schedule_state_gas_spent.saturating_sub(schedule_initial_state_gas);
                let state_gas_spillover =
                    runtime_state_gas.saturating_sub(schedule_initial_reservoir);
                let has_runtime_state = runtime_state_gas > 0;

                // `opcode_frames_ref` is built inside the drill-in branch but
                // the aggregator wants it on every tx so per-class
                // `opcode_totals` covers the aggregate cohorts, not just
                // drill-ins. Recompute the slice here from the same source.
                let opcode_frames_for_agg =
                    exec_result.map(|r| r.frame_opcode_counts.as_slice()).unwrap_or(&[]);

                // Per-tx gas row, emitted for all txs rather than the
                // `store_full_forensics` minority and never truncated by
                // `max_divergences_per_block`. Every field is already computed
                // above for the drill-in path, so the only cost of collecting it
                // is the row itself — skipped entirely when the run didn't opt
                // in, since it's the largest table the producer writes.
                let tx_gas_result =
                    self.collect_tx_gas_results.then(|| reth_research::database::TxGasResultRow {
                        schedule_name: schedule_name.to_string(),
                        schedule_config_hash: self
                            .schedule_metadata
                            .get(schedule_name)
                            .expect("all schedules should have static metadata")
                            .config_hash
                            .clone(),
                        block_number,
                        tx_index: tx_idx as u32,
                        tx_hash,
                        tx_type: tx.ty(),
                        tx_gas_limit: gas_limit,
                        max_fee_per_gas: max_fee_per_gas.clone(),
                        max_priority_fee_per_gas: max_priority_fee_per_gas.clone(),
                        baseline_success: normal_success,
                        baseline_gas_used: normal_gas_used,
                        baseline_total_gas_spent,
                        schedule_success,
                        schedule_gas_used: schedule_gas,
                        schedule_total_gas_spent,
                        schedule_gas_refunded,
                        schedule_floor_gas,
                        schedule_state_gas_spent,
                        schedule_intrinsic_gas,
                        min_multiplier_to_succeed,
                    });

                aggregators
                    .get_mut(schedule_name)
                    .expect("aggregator exists for every schedule")
                    .observe_tx(
                        reth_research::block_aggregator::TxObservation {
                            class: aggregate_class,
                            store_full_forensics,
                            gas_delta: total_delta,
                            state_gas_spent: schedule_state_gas_spent,
                            state_gas_spillover,
                            min_multiplier_to_succeed,
                            is_creation: is_create,
                            has_authorization: authorization_count > 0,
                            has_runtime_state,
                            // F4: `Some(0)` is a measured zero (folds harmlessly
                            // in the aggregator); `None` only on the unmeasured
                            // reject path.
                            cold_account_access_count: exec_result
                                .and_then(|r| r.cold_account_access_count),
                            storage_drivers: exec_result.and_then(|r| r.storage_drivers),
                            account_drivers: exec_result.and_then(|r| r.account_drivers),
                            drill_in_record: drill_in,
                            recipient,
                            // 4-byte selector for calls with >=4 calldata bytes;
                            // None for creations (computed once per tx above).
                            selector: entry_selector,
                            // gas_delta is only clean when the schedule replay
                            // fit the original limit; OOG-at-higher-tier txs
                            // carry halt-gas deltas and are excluded from the
                            // recipient gas sum.
                            succeeded_within_limit: schedule_replay_success &&
                                schedule_gas <= gas_limit,
                            tx_type: tx.ty(),
                            has_calldata: (input_zero_bytes + input_nonzero_bytes) > 0,
                            baseline_gas_used: normal_gas_used,
                            tx_gas_result,
                        },
                        opcode_frames_for_agg,
                    );
            }
        }

        // Flush each per-schedule aggregator at block end. Coverage,
        // summaries, and drill-ins go to DuckDB in a single transaction
        // per (schedule, block).
        for schedule in &self.all_schedules {
            if let Some(agg) = aggregators.remove(schedule.name()) {
                let output = agg.finish_block();
                self.send_block_output(output);
            }
        }

        let block_duration = block_start.elapsed().as_secs_f64();
        let tx_count = block.body().transactions().len();

        info!(
            target: "exex::research",
            block = block_number,
            tx_count,
            schedules = self.registry.len(),
            duration_ms = block_duration * 1000.0,
            "Block analyzed with multi-schedule research mode"
        );

        Ok(true)
    }

    /// Send a fully-built per-(schedule, block) output to the DuckDB
    /// writer thread for atomic persistence.
    fn send_block_output(&self, output: BlockOutput) {
        let Some(ref tx) = self.db_tx else {
            // No DB configured (e.g. `:memory:` mode) — log a one-liner.
            info!(
                target: "exex::research",
                block = output.coverage.block_number,
                schedule = output.coverage.schedule_name,
                tx_count = output.coverage.tx_count,
                drill_ins = output.drill_ins.len(),
                "Block output produced (no database configured)"
            );
            return;
        };
        let block_number = output.coverage.block_number;
        let schedule_name = output.coverage.schedule_name.clone();
        let drill_ins = output.drill_ins.len();
        if let Err(e) = tx.send(DbCommand::BlockProcessed(output)) {
            warn!(
                target: "exex::research",
                block = block_number,
                schedule = schedule_name,
                error = %e,
                "Failed to send block output to database writer"
            );
        } else {
            debug!(
                target: "exex::research",
                block = block_number,
                schedule = schedule_name,
                drill_ins,
                "Block output queued for database"
            );
        }
    }

    /// Queue a delete of every per-block row in the inclusive range.
    /// Used on chain reorg / revert.
    fn send_delete_block_range(&self, from_block: u64, to_block: u64) {
        if let Some(ref tx) = self.db_tx {
            if let Err(e) = tx.send(DbCommand::DeleteRange { from_block, to_block }) {
                warn!(
                    target: "exex::research",
                    from_block,
                    to_block,
                    error = %e,
                    "Failed to queue non-canonical cleanup"
                );
            } else {
                info!(
                    target: "exex::research",
                    from_block,
                    to_block,
                    "Queued non-canonical cleanup"
                );
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn research_exex<Node: FullNodeComponents>(
    ctx: ExExContext<Node>,
    registry: ScheduleRegistry,
    db_path: std::path::PathBuf,
    start_block: u64,
    max_divergences_per_block: Option<usize>,
    collect_tx_gas_results: bool,
    gas_limit_multipliers: Vec<u64>,
    backfill: bool,
    backfill_min_block: u64,
    backfill_max_block: Option<u64>,
    backfill_concurrency: usize,
    metadata_backfill_interval_secs: u64,
    contract_labels_interval_secs: u64,
    function_signatures_interval_secs: u64,
    label_config_path: Option<std::path::PathBuf>,
    export_config: Option<ExportConfig>,
) -> eyre::Result<()>
where
    Node::Evm: ConfigureEvm<
        BlockExecutorFactory: BlockExecutorFactory<
            EvmFactory: EvmFactory<Spec = SpecId, BlockEnv = BlockEnv>,
        >,
    >,
{
    ResearchExEx::new(
        ctx,
        registry,
        db_path,
        start_block,
        max_divergences_per_block,
        collect_tx_gas_results,
        gas_limit_multipliers,
        backfill,
        backfill_min_block,
        backfill_max_block,
        backfill_concurrency,
        metadata_backfill_interval_secs,
        contract_labels_interval_secs,
        function_signatures_interval_secs,
        label_config_path,
        export_config,
    )?
    .run()
    .await
}

/// Adapter that turns the ExEx context's provider into a
/// [`BytecodeFetcher`]. Uses the latest canonical state — codehashes of
/// non-self-destructed contracts don't change, so "latest" is the right
/// snapshot for a one-shot metadata backfill. Self-destructed accounts
/// return `Ok(None)` and are skipped by the orchestration loop.
struct ProviderBytecodeFetcher<P>
where
    P: StateProviderFactory,
{
    provider: P,
}

impl<P> reth_research::contract_metadata::BytecodeFetcher for ProviderBytecodeFetcher<P>
where
    P: StateProviderFactory,
{
    fn fetch_bytecode(
        &self,
        address: Address,
    ) -> Result<Option<Vec<u8>>, reth_research::contract_metadata::BackfillError> {
        let state = self.provider.latest().map_err(|e| {
            reth_research::contract_metadata::BackfillError::Fetch { address, source: Box::new(e) }
        })?;
        let code = state.account_code(&address).map_err(|e| {
            reth_research::contract_metadata::BackfillError::Fetch { address, source: Box::new(e) }
        })?;
        Ok(code.map(|bc| bc.bytes().to_vec()))
    }
}

/// Run the SQL-prefiltered incremental metadata backfill every `interval`,
/// forever, on a `spawn_blocking` thread (the orchestration loop hits the
/// DB synchronously and may call into rocksdb / state lookups).
///
/// Skips ticks where the prior tick is still running by configuring the
/// ticker with `MissedTickBehavior::Skip` — relevant after a long restart
/// where the first scan has thousands of fresh addresses to walk.
async fn periodic_metadata_backfill<P>(
    db: reth_research::database::DivergenceDatabase,
    provider: P,
    interval: std::time::Duration,
) where
    P: StateProviderFactory + Clone + Send + Sync + 'static,
{
    let mut ticker = tokio::time::interval(interval);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        ticker.tick().await;
        let db = db.clone();
        let provider = provider.clone();
        let join = tokio::task::spawn_blocking(move || {
            let fetcher = ProviderBytecodeFetcher { provider };
            reth_research::contract_metadata::run_metadata_backfill_incremental(&db, &fetcher)
        })
        .await;
        match join {
            Ok(Ok(stats)) if stats.upserted > 0 || stats.fetch_errors > 0 => {
                info!(
                    target: "exex::research::metadata_backfill",
                    addresses_examined = stats.addresses_examined,
                    upserted = stats.upserted,
                    skipped_existing = stats.skipped_existing,
                    no_bytecode = stats.no_bytecode,
                    fetch_errors = stats.fetch_errors,
                    "Periodic metadata backfill tick"
                );
            }
            Ok(Ok(stats)) => {
                debug!(
                    target: "exex::research::metadata_backfill",
                    addresses_examined = stats.addresses_examined,
                    "Periodic metadata backfill tick (no new addresses)"
                );
            }
            Ok(Err(e)) => {
                warn!(
                    target: "exex::research::metadata_backfill",
                    error = %e,
                    "Periodic metadata backfill failed"
                );
            }
            Err(e) => {
                warn!(
                    target: "exex::research::metadata_backfill",
                    error = %e,
                    "Periodic metadata backfill task panicked"
                );
            }
        }
    }
}

/// Periodically walk the unlabeled-address set and fill `contract_labels`
/// via the Blockscout → Sourcify → Etherscan fallback chain. Runs
/// entirely in the async runtime (HTTP I/O), with no `spawn_blocking`.
async fn periodic_contract_label_backfill(
    db: reth_research::database::DivergenceDatabase,
    fetcher: std::sync::Arc<reth_research::external_labels::ContractLabelFetcher>,
    interval: std::time::Duration,
) {
    let mut ticker = tokio::time::interval(interval);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        ticker.tick().await;
        match reth_research::external_labels::run_contract_label_backfill_incremental(&db, &fetcher)
            .await
        {
            Ok(stats) if stats.upserted_with_label > 0 || stats.fetch_errors > 0 => {
                info!(
                    target: "exex::research::contract_labels",
                    addresses_examined = stats.addresses_examined,
                    upserted_with_label = stats.upserted_with_label,
                    upserted_empty = stats.upserted_empty,
                    fetch_errors = stats.fetch_errors,
                    "Periodic contract-label backfill tick"
                );
            }
            Ok(stats) => {
                debug!(
                    target: "exex::research::contract_labels",
                    addresses_examined = stats.addresses_examined,
                    "Periodic contract-label backfill tick (no new labels)"
                );
            }
            Err(e) => {
                warn!(
                    target: "exex::research::contract_labels",
                    error = %e,
                    "Periodic contract-label backfill failed"
                );
            }
        }
    }
}

/// Periodically walk the unresolved-selector set and fill
/// `function_signatures` via OpenChain. Single batched HTTP call per
/// tick keeps the load tiny.
async fn periodic_function_signature_backfill(
    db: reth_research::database::DivergenceDatabase,
    fetcher: std::sync::Arc<reth_research::external_labels::OpenChainFetcher>,
    interval: std::time::Duration,
) {
    let mut ticker = tokio::time::interval(interval);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        ticker.tick().await;
        match reth_research::external_labels::run_function_signature_backfill_incremental(
            &db, &fetcher,
        )
        .await
        {
            Ok(stats) if stats.resolved > 0 || stats.fetch_errors > 0 => {
                info!(
                    target: "exex::research::function_signatures",
                    selectors_examined = stats.selectors_examined,
                    resolved = stats.resolved,
                    unresolved = stats.unresolved,
                    fetch_errors = stats.fetch_errors,
                    "Periodic function-signature backfill tick"
                );
            }
            Ok(stats) => {
                debug!(
                    target: "exex::research::function_signatures",
                    selectors_examined = stats.selectors_examined,
                    "Periodic function-signature backfill tick (no new resolutions)"
                );
            }
            Err(e) => {
                warn!(
                    target: "exex::research::function_signatures",
                    error = %e,
                    "Periodic function-signature backfill failed"
                );
            }
        }
    }
}

/// One-shot ExEx that opens the producer DuckDB, runs the contract-
/// metadata backfill against the node's `latest` state, logs the
/// resulting counters, and terminates the process. Idempotent — re-runs
/// only fetch bytecode for codehashes that aren't already in
/// `contract_metadata`.
async fn run_metadata_backfill_exex<Node: FullNodeComponents>(
    ctx: ExExContext<Node>,
    db_path: std::path::PathBuf,
) -> eyre::Result<()> {
    use reth_research::{contract_metadata::run_metadata_backfill, database::DivergenceDatabase};

    info!(target: "reth::cli", path = ?db_path, "Opening producer DB for metadata backfill");
    let db = DivergenceDatabase::open(&db_path)?;
    let fetcher = ProviderBytecodeFetcher { provider: ctx.components.provider().clone() };

    info!(target: "reth::cli", "Starting contract-metadata backfill");
    let stats = run_metadata_backfill(&db, &fetcher)?;
    info!(
        target: "reth::cli",
        addresses_examined = stats.addresses_examined,
        upserted = stats.upserted,
        skipped_existing = stats.skipped_existing,
        no_bytecode = stats.no_bytecode,
        fetch_errors = stats.fetch_errors,
        "Contract-metadata backfill complete; exiting"
    );

    // No graceful node-shutdown plumbing from inside an ExEx future, so
    // terminate the process directly. The caller's expectation is a
    // one-shot CLI tool, not a long-running daemon.
    std::process::exit(0);
}

fn main() -> eyre::Result<()> {
    reth_ethereum::cli::Cli::<reth_ethereum::cli::chainspec::EthereumChainSpecParser, ResearchArgs>::parse()
        .run(|builder, research_args: ResearchArgs| async move {
            // Special mode: one-shot contract-metadata backfill. Reads
            // every distinct address from divergence_call_frames, fetches
            // bytecode from reth state, parses the CBOR metadata trailer,
            // and UPSERTs contract_metadata. Exits the process when done.
            if research_args.metadata_backfill {
                let db_path = research_args.db_path.clone();
                let handle = builder
                    .node(EthereumNode::default())
                    .install_exex("contract-metadata-backfill", move |ctx| {
                        let db_path = db_path.clone();
                        async move { Ok(run_metadata_backfill_exex(ctx, db_path)) }
                    })
                    .launch()
                    .await?;
                return handle.wait_for_node_exit().await;
            }

            // Check if any schedules are configured
            if !research_args.has_schedules() {
                return Err(eyre::eyre!(
                    "No research schedules configured. Use --research.amsterdam, --research.csv, or --research.multiplier"
                ));
            }

            // Build the schedule registry from CLI args
            let registry = research_args
                .build_registry()
                .map_err(|e| eyre::eyre!("Failed to build schedule registry: {}", e))?;

            let db_path = research_args.db_path.clone();
            let start_block = research_args.start_block;
            let max_divergences_per_block = research_args.max_divergences_per_block;
            let collect_tx_gas_results = research_args.tx_gas_results;
            let gas_limit_multipliers = research_args.gas_limit_multipliers.clone();
            let backfill = research_args.backfill;
            let backfill_min_block = research_args.backfill_min_block;
            let backfill_max_block = research_args.backfill_max_block;
            let backfill_concurrency = research_args.backfill_concurrency;
            let metadata_backfill_interval_secs =
                research_args.metadata_backfill_interval_secs;
            let contract_labels_interval_secs =
                research_args.contract_labels_interval_secs;
            let function_signatures_interval_secs =
                research_args.function_signatures_interval_secs;
            let label_config_path = research_args.label_config_path.clone();

            // Parse the export config before launching the node so malformed
            // configuration fails fast. Export stays disabled when absent.
            let export_config = match research_args.export_config_path.as_ref() {
                Some(path) => Some(
                    ExportConfig::load(path)
                        .map_err(|e| eyre::eyre!("failed to load ClickHouse export config: {e}"))?,
                ),
                None => None,
            };

            info!(
                target: "reth::cli",
                schedules = registry.len(),
                db_path = ?db_path,
                start_block,
                max_divergences_per_block,
                collect_tx_gas_results,
                gas_limit_multipliers = ?gas_limit_multipliers,
                backfill,
                backfill_min_block,
                backfill_max_block,
                backfill_concurrency,
                metadata_backfill_interval_secs,
                contract_labels_interval_secs,
                function_signatures_interval_secs,
                label_config_path = ?label_config_path,
                "Starting multi-schedule research mode"
            );

            let handle = builder
                .node(EthereumNode::default())
                .install_exex("research", move |ctx| {
                    let registry = registry.clone();
                    let db_path = db_path.clone();
                    let gas_limit_multipliers = gas_limit_multipliers.clone();
                    let label_config_path = label_config_path.clone();
                    let export_config = export_config.clone();
                    async move {
                        Ok(research_exex(
                            ctx,
                            registry,
                            db_path,
                            start_block,
                            max_divergences_per_block,
                            collect_tx_gas_results,
                            gas_limit_multipliers,
                            backfill,
                            backfill_min_block,
                            backfill_max_block,
                            backfill_concurrency,
                            metadata_backfill_interval_secs,
                            contract_labels_interval_secs,
                            function_signatures_interval_secs,
                            label_config_path,
                            export_config,
                        ))
                    }
                })
                // Use the debug-capable launcher (same as the standard `reth`
                // binary) so node debug features work — notably `--dev`, whose
                // `LocalMiner` is only installed by this launcher. Plain
                // `.launch()` would never produce blocks in dev mode.
                .launch_with_debug_capabilities()
                .await?;

            handle.wait_for_node_exit().await
        })
}

/// Initial backfill cursor: the inclusive upper bound `backfill_max_block`
/// clamped to `head - 1`, or `head - 1` when unbounded.
fn initial_backfill_cursor(head: u64, backfill_max_block: Option<u64>) -> u64 {
    let tip = head.saturating_sub(1);
    match backfill_max_block {
        Some(max) => max.min(tip),
        None => tip,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::Address;
    use reth_research::divergence::{CallFrame, CallType};

    #[test]
    fn initial_backfill_cursor_respects_max_and_head() {
        // Unbounded: start at head - 1.
        assert_eq!(initial_backfill_cursor(100, None), 99);
        // Inclusive max below head: start exactly at max.
        assert_eq!(initial_backfill_cursor(100, Some(50)), 50);
        // Max at or above head: clamp to head - 1 (can't analyze unknown blocks).
        assert_eq!(initial_backfill_cursor(100, Some(200)), 99);
        assert_eq!(initial_backfill_cursor(100, Some(99)), 99);
        // Degenerate head 0 saturates to 0 (the head==0 guard short-circuits earlier).
        assert_eq!(initial_backfill_cursor(0, Some(50)), 0);
    }

    fn frame_at(call_index: usize, depth: usize) -> CallFrame {
        CallFrame {
            call_index,
            depth,
            from: Address::ZERO,
            to: None,
            call_type: CallType::Call,
            gas_provided: 0,
            gas_used: 0,
            success: true,
            input: None,
            output: None,
            repricing_gas_delta: 0,
            gas_requested_on_stack: None,
            parent_gas_at_call: None,
            value_wei: None,
            caller_pc: None,
            was_precompile: false,
            precompile_address: None,
            gas_remaining_at_fail: None,
            storage_target: None,
        }
    }

    /// F2 revert decode: the two canonical Solidity errors, custom selectors,
    /// and the empty/short edge cases.
    #[test]
    fn decode_revert_recognises_canonical_errors() {
        // Error(string) "boom": selector + offset(32) + len(4) + utf8 + pad.
        let mut err = vec![0x08, 0xc3, 0x79, 0xa0];
        err.extend_from_slice(&[0u8; 31]);
        err.push(0x20); // offset word = 32
        err.extend_from_slice(&[0u8; 31]);
        err.push(0x04); // length word = 4
        err.extend_from_slice(b"boom");
        err.extend_from_slice(&[0u8; 28]); // pad to a full 32-byte word
        assert_eq!(decode_revert(&err), "Error(string): boom");

        // Panic(uint256) 0x11 (arithmetic overflow): selector + 32-byte code.
        let mut panic = vec![0x4e, 0x48, 0x7b, 0x71];
        panic.extend_from_slice(&[0u8; 31]);
        panic.push(0x11);
        assert_eq!(decode_revert(&panic), "Panic(0x11)");

        // Unknown custom-error selector is surfaced as 4 bytes.
        assert_eq!(decode_revert(&[0xde, 0xad, 0xbe, 0xef]), "custom:0xdeadbeef");

        // Empty / short returndata.
        assert_eq!(decode_revert(&[]), "empty");
        assert_eq!(decode_revert(&[0x01, 0x02]), "short:0x0102");
    }

    /// F2 cap: returndata is truncated to the bounded prefix.
    #[test]
    fn cap_bytes_truncates_to_cap() {
        assert_eq!(cap_bytes(&[0u8; 500]).len(), REVERT_DATA_CAP);
        assert_eq!(cap_bytes(&[1u8, 2, 3]), vec![1, 2, 3]);
    }

    /// Single-frame tx: root only. No parent.
    #[test]
    fn parent_call_indices_for_single_root() {
        let frames = vec![frame_at(0, 0)];
        let parents = derive_parent_call_indices(&frames);
        assert_eq!(parents, vec![None]);
    }

    /// Post-order DFS for a tx that did one sub-call: child completes
    /// first (call_index 0, depth 1), then root (call_index 1, depth 0).
    /// The child's parent is the root's call_index (1).
    #[test]
    fn parent_call_indices_for_root_with_one_subcall() {
        let frames = vec![frame_at(0, 1), frame_at(1, 0)];
        let parents = derive_parent_call_indices(&frames);
        assert_eq!(parents, vec![Some(1), None]);
    }

    /// Three-deep chain: leaf → middle → root. Post-order is
    /// [leaf, middle, root]; call_indices are [0, 1, 2].
    /// Leaf's parent is middle (call_index 1); middle's parent is root
    /// (call_index 2); root has no parent.
    #[test]
    fn parent_call_indices_for_three_deep_chain() {
        let frames = vec![frame_at(0, 2), frame_at(1, 1), frame_at(2, 0)];
        let parents = derive_parent_call_indices(&frames);
        assert_eq!(parents, vec![Some(1), Some(2), None]);
    }

    /// Two siblings under one root. Order of completion:
    /// sibling A (idx 0, depth 1), sibling B (idx 1, depth 1), root (idx 2, depth 0).
    /// Both siblings' parent = root.
    #[test]
    fn parent_call_indices_for_two_siblings() {
        let frames = vec![frame_at(0, 1), frame_at(1, 1), frame_at(2, 0)];
        let parents = derive_parent_call_indices(&frames);
        assert_eq!(parents, vec![Some(2), Some(2), None]);
    }

    /// Mixed-depth tree:
    /// root calls A; A calls A1; A returns; root calls B.
    /// Completion order: A1 (depth 2), A (depth 1), B (depth 1), root (depth 0).
    /// call_indices match completion: [0, 1, 2, 3].
    /// A1's parent is A (the first subsequent depth-1 frame, idx 1, call_index 1).
    /// A's parent is root (the first subsequent depth-0 frame, idx 3, call_index 3).
    /// B's parent is also root (the first subsequent depth-0 frame, idx 3, call_index 3).
    #[test]
    fn parent_call_indices_for_mixed_tree() {
        let frames = vec![
            frame_at(0, 2), // A1
            frame_at(1, 1), // A
            frame_at(2, 1), // B
            frame_at(3, 0), // root
        ];
        let parents = derive_parent_call_indices(&frames);
        assert_eq!(parents, vec![Some(1), Some(3), Some(3), None]);
    }
}
