//! Multi-Schedule Research Mode Execution Extension
//!
//! This ExEx performs multi-schedule execution analysis to detect divergences caused by
//! modified gas costs across multiple gas pricing experiments simultaneously.
//!
//! Supported schedules:
//! - EIP-2780: Reduced intrinsic gas based on transaction category
//! - EIP-8037: Native state creation gas and reservoir accounting
//! - CSV Pricing: Per-opcode/precompile gas repricing from CSV files
//! - Multiplier: Uniform gas cost multiplication
//!
//! Run with:
//!
//! ```sh
//! cargo run --release -p reth-research-bin node --dev --dev.block-time 5s \
//!   --research.eip2780 \
//!   --research.eip8037 \
//!   --research.csv 7904-prelim=./schedules/7904_prelim.csv \
//!   --research.gas-limit-multiplier 8 \
//!   --research.db-path ./divergences.db
//! ```

use alloy_consensus::{constants::KECCAK_EMPTY, transaction::TxHashRef, BlockHeader, Transaction};
use alloy_eips::BlockHashOrNumber;
use alloy_primitives::{keccak256, logs_bloom, Address, Bloom, Bytes, B256, U256};
use clap::Parser;
use futures::{stream::FuturesUnordered, FutureExt, StreamExt, TryStreamExt};
use reth_ethereum::{
    exex::{ExExContext, ExExEvent, ExExNotification},
    node::EthereumNode,
};
use reth_evm::{block::BlockExecutorFactory, ConfigureEvm, Evm, EvmFactory, TransactionEnvMut};
use reth_node_api::{BlockTy, FullNodeComponents};
use reth_node_core::args::ResearchArgs;
use reth_primitives_traits::BlockBody;
use reth_provider::{BlockNumReader, BlockReader, StateProviderFactory, TransactionVariant};
use reth_research::{
    block_aggregator::{BlockAggregator, BlockMeta},
    database::{
        BlockOutput, CallFrameRow, DivergenceDatabase, DivergenceRow, DrillInRecord, OpcodeCountRow,
    },
    divergence::{
        classify_bucket, BucketInput, CallFrame, CallType as ResCallType, DivergenceLocation,
        EventLog, OogPattern, OutOfGasInfo,
    },
    oog_chain::classify_oog_chain,
    schedule::{GasSchedule, RecipientInfo, ScheduleKind, ScheduleRegistry, TxContext},
    step_trace::{first_divergence, StepTraceInspector},
    ScheduleInspector, TrackingInspector,
};
use reth_revm::{database::StateProviderDatabase, db::State, Database, DatabaseCommit};
use reth_tracing::tracing::{debug, info, warn};
use revm::{context::BlockEnv, context_interface::Cfg, primitives::hardfork::SpecId};
use revm_interpreter::gas::calculate_initial_tx_gas;
use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    thread::JoinHandle,
};
use tokio::{sync::mpsc, task::JoinHandle as TokioJoinHandle};

/// One unit of work for the DB writer thread. Each `BlockProcessed`
/// command is a fully-built per-(schedule, block) output and gets written
/// as a single DuckDB transaction. `DeleteRange` clears all per-block
/// tables in a contiguous block range — used on chain reorg / revert.
enum DbCommand {
    BlockProcessed(BlockOutput),
    DeleteRange { from_block: u64, to_block: u64 },
}

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
    fn serialize_trace<T: serde::Serialize>(value: &T) -> Option<String> {
        serde_json::to_string(value).ok()
    }

    fn hash_bytes(bytes: &[u8]) -> String {
        format!("{:#x}", keccak256(bytes))
    }

    fn hash_serialized<T: serde::Serialize>(value: &T) -> Option<String> {
        Self::serialize_trace(value).map(|json| Self::hash_bytes(json.as_bytes()))
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

    /// Synthesize a `DivergenceLocation` from the recorded call frames when the
    /// inspector didn't capture one itself.
    ///
    /// The `ScheduleInspector` records a divergence location whenever
    /// `apply_gas_delta` triggers OOG (per-opcode deltas) or when a frame ends
    /// with an OOG-class result (native-revm schedules like EIP-8037). This
    /// fallback handles the remaining cases — non-OOG schedule-induced
    /// failures (e.g. revert from a hard-coded gas check, halt for an unrelated
    /// reason) — by pointing at the deepest failed frame in the schedule run.
    ///
    /// Records emitted from this fallback have `pc = 0` / `opcode = 0xfe` (the
    /// `INVALID` mnemonic, used here as a "post-hoc fallback" sentinel) so
    /// downstream consumers can distinguish synthesized fallback rows from
    /// real per-opcode divergence captures.
    fn derive_divergence_location(call_frames: &[CallFrame]) -> Option<DivergenceLocation> {
        let deepest_failed = call_frames.iter().filter(|f| !f.success).max_by_key(|f| f.depth)?;
        Some(DivergenceLocation {
            contract: deepest_failed.to.unwrap_or(deepest_failed.from),
            function_selectors: Vec::new(),
            pc: 0,
            call_depth: deepest_failed.depth,
            opcode: 0xfe,
            opcode_name: "FALLBACK".to_string(),
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
        spec_id: impl Into<revm_primitives::hardfork::SpecId>,
    ) -> u64 {
        calculate_initial_tx_gas(
            spec_id.into(),
            input,
            is_create,
            access_list_accounts,
            access_list_storage_slots,
            authorization_list_num,
        )
        .initial_total_gas
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
        gas_limit_multipliers: Vec<u64>,
        backfill: bool,
        backfill_min_block: u64,
        backfill_concurrency: usize,
        metadata_backfill_interval_secs: u64,
        contract_labels_interval_secs: u64,
        function_signatures_interval_secs: u64,
        label_config_path: Option<std::path::PathBuf>,
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

        // Initialize database and async writer
        let (divergence_db, db_tx, db_writer_task) = if db_path.to_str() != Some(":memory:") {
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

            // Spawn database writer task. Each `BlockProcessed` command
            // lands in a single DuckDB transaction (per-block commit) so
            // a crash mid-block doesn't leave half-written aggregates.
            let (tx, mut rx) = mpsc::unbounded_channel::<DbCommand>();
            let writer_db = divergence_db.clone();
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
                                if let Err(error) = divergence_db.record_block_output(&output) {
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
                                            divergences = counts.divergences,
                                            call_frames = counts.call_frames,
                                            opcode_counts = counts.opcode_counts,
                                            event_logs = counts.event_logs,
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

            (Some(divergence_db), Some(tx), Some(writer_task))
        } else {
            (None, None, None)
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
            // Guarantee at least one tier and that every tier is ≥ 1, so
            // the replay loop never multiplies by zero and always
            // executes at least the original-limit attempt.
            gas_limit_multipliers: {
                let mut tiers: Vec<u64> =
                    gas_limit_multipliers.into_iter().map(|m| m.max(1)).collect();
                if tiers.is_empty() {
                    tiers.push(1);
                }
                // Ascending so we accept the smallest successful tier.
                tiers.sort_unstable();
                tiers.dedup();
                tiers
            },
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

        Ok(Self {
            ctx,
            analyzer,
            db_writer_task,
            divergence_db,
            start_block,
            backfill_enabled,
            backfill_min_block,
            backfill_concurrency,
            next_backfill_block: None,
            backfill_exhausted: false,
            in_flight_backfill: FuturesUnordered::new(),
            blocks_processed: 0,
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
                match self.ctx.notifications.try_next().await? {
                    Some(notification) => self.handle_notification(notification).await?,
                    None => break,
                }
            } else {
                // Borrow disjoint fields out before select! to keep the macro happy.
                let notifications = &mut self.ctx.notifications;
                let in_flight = &mut self.in_flight_backfill;
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
                };
                match outcome {
                    SelectOutcome::Notification(n) => self.handle_notification(n).await?,
                    SelectOutcome::StreamClosed => break,
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
            let start = head.saturating_sub(1);
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
    ///
    /// The per-schedule destructuring tuple still carries several locals
    /// that were used by the legacy SQLite `ScheduleDivergence` builder
    /// (formatted_op_counts, the *_json call-frame/event-log fields, the
    /// hash fields, the output_* fields, etc.). They're kept in the tuple
    /// so the existing per-schedule analysis remains a one-shot match
    /// expression; future cleanup can drop them once the SQLite-shape
    /// code is fully gone.
    #[allow(unused_variables)]
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
            let baseline_event_logs = baseline_inspector.event_logs().to_vec();
            let (baseline_output_hash, baseline_output_len) =
                Self::output_hash_and_len(&normal_result.result);
            let baseline_created_address =
                normal_result.result.created_address().map(Self::hex_address);
            let baseline_log_count = normal_result.result.logs().len() as u64;
            let baseline_logs_bloom = Self::logs_bloom_hex(&normal_result.result);
            let baseline_call_frames_hash = Self::hash_serialized(&baseline_call_frames);
            let baseline_event_logs_hash = Self::hash_serialized(&baseline_event_logs);
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
                initial_state_gas: u64,
                initial_reservoir: u64,
                floor_gas: u64,
                gas_refunded: u64,
                operation_counts: Option<String>,
                oog_info: Option<String>,
                divergence_location: Option<String>,
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
                /// Per-frame opcode counts captured by the inspector. Used
                /// to populate `divergence_opcode_counts` rows for drill-in
                /// buckets.
                frame_opcode_counts: Vec<reth_research::divergence::FrameOpcodeCounts>,
                call_frames: Vec<CallFrame>,
                event_logs: Vec<EventLog>,
                output_hash: Option<String>,
                output_len: Option<u64>,
                created_address: Option<String>,
                log_count: u64,
                logs_bloom: String,
                call_frames_hash: Option<String>,
                event_logs_hash: Option<String>,
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
                let (
                    schedule_initial_total_gas,
                    schedule_initial_state_gas,
                    schedule_initial_reservoir,
                    native_initial_total_gas,
                    native_initial_state_gas,
                ) = if uses_schedule_eip8037 {
                    let ctx = tx_context
                        .as_ref()
                        .expect("EIP-8037 schedule modifies intrinsic gas and requires tx context");
                    let init_gas = schedule
                        .initial_and_floor_gas(ctx)
                        .expect("EIP-8037 schedule must expose its overridden initial gas split");
                    let native_init_gas = calculate_initial_tx_gas(
                        SpecId::AMSTERDAM,
                        &input,
                        is_create,
                        access_list_accounts,
                        access_list_storage_slots,
                        authorization_count,
                    );
                    let (_limit, reservoir) = init_gas.initial_gas_and_reservoir(
                        gas_limit,
                        revm::primitives::eip7825::TX_GAS_LIMIT_CAP,
                        true,
                    );
                    (
                        init_gas.initial_total_gas,
                        init_gas.initial_state_gas,
                        reservoir,
                        native_init_gas.initial_total_gas,
                        native_init_gas.initial_state_gas,
                    )
                } else {
                    (0, 0, 0, 0, 0)
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
                // Tier-1 (mainnet-equivalent) env + tx, stashed for the
                // trace-diff divergence fallback so it can reproduce the
                // failure at the original gas limit without rebuilding the
                // schedule env setup.
                let mut tier1_envs: Option<(_, _)> = None;
                for &tier in &self.gas_limit_multipliers {
                    let schedule_execution_gas_limit = gas_limit.saturating_mul(tier);

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
                    if uses_schedule_eip8037 {
                        let intrinsic_delta = i128::from(schedule_initial_total_gas) -
                            i128::from(native_initial_total_gas);
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
                                "Gas limit clamped for native EIP-8037 schedule — execution \
                                 budget may be conservative"
                            );
                        }
                        sched_tx_env.set_gas_limit(adjusted);
                    } else if schedule.modifies_intrinsic() && !schedule.uses_native_intrinsic_gas()
                    {
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
                            let synth_oog = OutOfGasInfo {
                                opcode: 0,
                                opcode_name: "evm_reject_intrinsic".to_string(),
                                pc: 0,
                                contract: Address::ZERO,
                                call_depth: 1,
                                gas_remaining: 0,
                                pattern: OogPattern::Unknown,
                            };
                            last_attempt = Some(PerScheduleResult {
                                success: false,
                                gas_used: gas_limit,
                                total_gas_spent: 0,
                                state_gas_spent: 0,
                                initial_state_gas: schedule_initial_state_gas,
                                initial_reservoir: schedule_initial_reservoir,
                                floor_gas: 0,
                                gas_refunded: 0,
                                operation_counts: None,
                                oog_info: Some(format!("EVM transact failed: {e:?}")),
                                divergence_location: None,
                                replay_halt_oog: Some(true),
                                oog_call_depth: Some(synth_oog.call_depth),
                                oog_info_structured: Some(synth_oog),
                                divergence_location_structured: None,
                                frame_opcode_counts: Vec::new(),
                                call_frames: Vec::new(),
                                event_logs: Vec::new(),
                                output_hash: None,
                                output_len: None,
                                created_address: None,
                                log_count: 0,
                                logs_bloom: Self::hex_bloom(Bloom::ZERO),
                                call_frames_hash: None,
                                event_logs_hash: None,
                            });
                            continue;
                        }
                    };

                    let sched_success = result.result.is_success();
                    let mut sched_gas_used = result.result.tx_gas_used();
                    let mut sched_total_gas_spent = result.result.gas().total_gas_spent();
                    let mut sched_state_gas_spent = result.result.gas().state_gas_spent();
                    if uses_schedule_eip8037 {
                        // revm reports totals with its built-in Amsterdam
                        // intrinsic state-gas component. Normalize the
                        // persisted research fields to this schedule's
                        // PR-11616 initial split before downstream
                        // runtime-state/spillover derivation.
                        let total_intrinsic_delta = i128::from(schedule_initial_total_gas) -
                            i128::from(native_initial_total_gas);
                        let state_intrinsic_delta = i128::from(schedule_initial_state_gas) -
                            i128::from(native_initial_state_gas);
                        sched_gas_used =
                            Self::apply_signed_gas_delta(sched_gas_used, total_intrinsic_delta);
                        sched_total_gas_spent = Self::apply_signed_gas_delta(
                            sched_total_gas_spent,
                            total_intrinsic_delta,
                        );
                        sched_state_gas_spent = Self::apply_signed_gas_delta(
                            sched_state_gas_spent,
                            state_intrinsic_delta,
                        );
                    }
                    let sched_floor_gas = result.result.gas().floor_gas();
                    let sched_gas_refunded = result.result.gas().inner_refunded();
                    let op_counts = Self::serialize_trace(inspector.operation_counts());
                    let insp_result = inspector.result();
                    let halt_reason_debug = match &result.result {
                        revm::context_interface::result::ExecutionResult::Halt {
                            reason, ..
                        } => Some(format!("{reason:?}")),
                        _ => None,
                    };
                    let halt_info = halt_reason_debug
                        .as_ref()
                        .map(|reason| format!("Execution halted: {reason}"));
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
                    let log_count = result.result.logs().len() as u64;
                    let logs_bloom = Self::logs_bloom_hex(&result.result);
                    let call_frames = inspector.call_frames().to_vec();
                    let event_logs = inspector.event_logs().to_vec();

                    // Synthesize a root-frame OOG record when the tx halted with
                    // OutOfGas but the inspector didn't capture any per-frame
                    // oog_info. See the equivalent legacy comment for the full
                    // motivation; nothing tier-specific.
                    let inspector_oog_info = insp_result.oog_info.clone().or_else(|| {
                        if replay_halt_oog == Some(true) {
                            Some(OutOfGasInfo {
                                opcode: 0,
                                opcode_name: "root_halt".to_string(),
                                pc: 0,
                                contract: Address::ZERO,
                                call_depth: 1,
                                gas_remaining: 0,
                                pattern: OogPattern::Unknown,
                            })
                        } else {
                            None
                        }
                    });

                    let psr = PerScheduleResult {
                        success: sched_success,
                        gas_used: sched_gas_used,
                        total_gas_spent: sched_total_gas_spent,
                        state_gas_spent: sched_state_gas_spent,
                        initial_state_gas: schedule_initial_state_gas,
                        initial_reservoir: schedule_initial_reservoir,
                        floor_gas: sched_floor_gas,
                        gas_refunded: sched_gas_refunded,
                        operation_counts: op_counts,
                        oog_info: inspector_oog_info
                            .as_ref()
                            .map(|oog| format!("{oog:?}"))
                            .or(halt_info),
                        divergence_location: insp_result
                            .divergence_location
                            .clone()
                            .or_else(|| Self::derive_divergence_location(&call_frames))
                            .as_ref()
                            .map(|loc| format!("{loc:?}")),
                        replay_halt_oog,
                        oog_call_depth: inspector_oog_info.as_ref().map(|oog| oog.call_depth),
                        oog_info_structured: inspector_oog_info.clone(),
                        divergence_location_structured: insp_result.divergence_location.clone(),
                        frame_opcode_counts: inspector.frame_opcode_counts().frames.clone(),
                        call_frames_hash: Self::hash_serialized(&call_frames),
                        event_logs_hash: Self::hash_serialized(&event_logs),
                        call_frames,
                        event_logs,
                        output_hash,
                        output_len,
                        created_address,
                        log_count,
                        logs_bloom,
                    };

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
                let mut chosen =
                    accepted.or(last_attempt).expect("tier loop ran at least once");

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
                            let mut evm =
                                self.components.evm_config().evm_with_env_and_inspector(
                                    &mut normal_db,
                                    evm_env.clone(),
                                    &mut insp,
                                );
                            let ok = evm.transact(tx_env.clone()).is_ok();
                            drop(evm);
                            baseline_step_trace = Some(
                                (ok && !insp.truncated()).then(|| insp.steps().to_vec()),
                            );
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
                                if let Some(loc) =
                                    first_divergence(base_steps, sched_insp.steps())
                                {
                                    chosen.divergence_location = Some(format!("{loc:?}"));
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

                // Look up the re-execution result for this schedule.
                // For Both schedules the execution ran with an adjusted gas_limit
                // so the EVM had the correct execution gas budget. The reported
                // gas_used includes baseline intrinsic, so we add intrinsic_delta
                // to get the true schedule gas_used (with schedule intrinsic).
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
                    schedule_initial_state_gas,
                    schedule_initial_reservoir,
                    schedule_floor_gas,
                    schedule_gas_refunded,
                    formatted_op_counts,
                    oog_info,
                    divergence_location,
                    replay_halt_oog,
                    call_tree_diverged,
                    event_logs_diverged,
                    baseline_call_frames_json,
                    schedule_call_frames_json,
                    baseline_event_logs_json,
                    schedule_event_logs_json,
                    baseline_call_frames_hash_row,
                    schedule_call_frames_hash_row,
                    baseline_event_logs_hash_row,
                    schedule_event_logs_hash_row,
                    schedule_output_hash,
                    schedule_output_len,
                    schedule_created_address,
                    schedule_log_count,
                    schedule_logs_bloom,
                    output_changed,
                    created_address_changed,
                    logs_bloom_changed,
                ) = match exec_result {
                    Some(r) => {
                        // EVM reports gas_used including baseline intrinsic.
                        // Replace baseline intrinsic with schedule intrinsic to
                        // get the true schedule total:
                        //   gas_used - baseline_intrinsic + schedule_intrinsic
                        //   = gas_used + intrinsic_delta
                        //
                        // For "Both" schedules where gas_limit was clamped
                        // (negative intrinsic_delta), the execution portion may
                        // be inaccurate (see clamping comment above), but the
                        // intrinsic substitution is still correct.
                        let gas = if intrinsic_delta != 0 {
                            (r.gas_used as i64 + intrinsic_delta).max(0) as u64
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
                            r.initial_state_gas,
                            r.initial_reservoir,
                            r.floor_gas,
                            r.gas_refunded,
                            r.operation_counts.clone(),
                            r.oog_info.clone(),
                            r.divergence_location.clone(),
                            r.replay_halt_oog,
                            call_tree_diverged,
                            event_logs_diverged,
                            call_tree_diverged
                                .then(|| Self::serialize_trace(&baseline_call_frames))
                                .flatten(),
                            call_tree_diverged
                                .then(|| Self::serialize_trace(&r.call_frames))
                                .flatten(),
                            event_logs_diverged
                                .then(|| Self::serialize_trace(&baseline_event_logs))
                                .flatten(),
                            event_logs_diverged
                                .then(|| Self::serialize_trace(&r.event_logs))
                                .flatten(),
                            baseline_call_frames_hash.clone(),
                            r.call_frames_hash.clone(),
                            baseline_event_logs_hash.clone(),
                            r.event_logs_hash.clone(),
                            r.output_hash.clone(),
                            r.output_len,
                            r.created_address.clone(),
                            r.log_count,
                            r.logs_bloom.clone(),
                            output_changed,
                            created_address_changed,
                            logs_bloom_changed,
                        )
                    }
                    None => {
                        // Intrinsic-only schedule: no execution result, so the
                        // schedule-side state-gas / reservoir / refund counters
                        // stay zero — only the baseline equivalents are meaningful.
                        let gas = (normal_gas_used as i64 + intrinsic_delta).max(0) as u64;
                        let success = normal_success && gas <= gas_limit;
                        (
                            gas,
                            success,
                            normal_success,
                            0,
                            0,
                            0,
                            0,
                            0,
                            0,
                            None,
                            None,
                            None,
                            None,
                            false,
                            false,
                            None,
                            None,
                            None,
                            None,
                            baseline_call_frames_hash.clone(),
                            baseline_call_frames_hash.clone(),
                            baseline_event_logs_hash.clone(),
                            baseline_event_logs_hash.clone(),
                            baseline_output_hash.clone(),
                            baseline_output_len,
                            baseline_created_address.clone(),
                            baseline_log_count,
                            baseline_logs_bloom.clone(),
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
                let total_delta = schedule_gas as i64 - normal_gas_used as i64;
                // Decompose the outcome flip into its two directions so the
                // bucket classifier can give beneficial flips their own
                // category (`ScheduleRescued`) instead of folding them into
                // `ContractBroken`. The legacy bidirectional `status_changed`
                // value is kept on the `DivergenceRow` for backwards
                // compatibility with existing DB rows / consumers.
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

                // Classify the tx into one of the storage buckets.
                // `oog_call_depth` is the 1-based OOG attribution depth from
                // the inspector (or the producer's Err-branch synthesis when
                // revm rejected the tx outright). The classifier uses it only
                // after handling successful higher-gas witnesses, throttled
                // chains, and sweep-exhausted OOGs.
                let oog_call_depth = exec_result.and_then(|r| r.oog_call_depth);
                let bucket = classify_bucket(&BucketInput {
                    baseline_to_schedule_break,
                    baseline_to_schedule_rescue,
                    gas_delta: total_delta,
                    event_logs_changed: event_logs_diverged,
                    call_tree_changed: call_tree_diverged,
                    output_changed,
                    created_address_changed,
                    logs_bloom_changed,
                    outer_limit_only_failure,
                    replay_halt_oog,
                    oog_call_depth,
                    oog_chain_proportional,
                });

                // For drill-in buckets, build the full per-tx record.
                let drill_in = if bucket.is_drill_in() {
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
                        let oog_info_s = exec_result.and_then(|r| r.oog_info_structured.clone());
                        let frames_ref: &[CallFrame] =
                            exec_result.map(|r| r.call_frames.as_slice()).unwrap_or(&[]);
                        let opcode_frames_ref =
                            exec_result.map(|r| r.frame_opcode_counts.as_slice()).unwrap_or(&[]);
                        let sched_logs_ref: &[EventLog] =
                            exec_result.map(|r| r.event_logs.as_slice()).unwrap_or(&[]);

                        let divergence_row = DivergenceRow {
                            schedule_name: schedule_name.to_string(),
                            schedule_config_hash: metadata.config_hash.clone(),
                            block_number,
                            tx_index: tx_idx as u32,
                            tx_hash,
                            timestamp: block_timestamp,
                            bucket,
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
                            would_fit_in_original_limit: Some(schedule_success),
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
                        };

                        // Inspector emits frames in post-order DFS — children
                        // come before their parent. The parent of a frame at
                        // index `i` (with depth `d > 0`) is the first
                        // subsequent frame at depth `d - 1`.
                        let parent_call_indices = derive_parent_call_indices(frames_ref);

                        // Pull codehashes for every distinct frame target
                        // out of the historical state we already loaded
                        // for analysis (`normal_db`). One revm `basic()`
                        // lookup per address; results cached so repeat
                        // hits in the same drill-in tx are free. Failures
                        // — self-destructed contracts, missing accounts —
                        // leave the column NULL rather than aborting the
                        // drill-in record.
                        let mut codehash_cache: HashMap<Address, Option<B256>> = HashMap::new();
                        let call_frames_rows: Vec<CallFrameRow> = frames_ref
                            .iter()
                            .enumerate()
                            .map(|(i, f)| {
                                let codehash = f.to.and_then(|addr| {
                                    *codehash_cache.entry(addr).or_insert_with(|| {
                                        match normal_db.basic(addr) {
                                            Ok(Some(info)) if info.code_hash != KECCAK_EMPTY => {
                                                Some(info.code_hash)
                                            }
                                            _ => None,
                                        }
                                    })
                                });
                                CallFrameRow {
                                    call_index: f.call_index as u32,
                                    parent_call_index: parent_call_indices[i],
                                    depth: f.depth as u32,
                                    from_address: f.from,
                                    to_address: f.to.unwrap_or_default(),
                                    // For non-DELEGATECALL frames this equals
                                    // `to_address`; once the inspector splits
                                    // storage-context vs code-holder for
                                    // DELEGATECALL, this column will carry the
                                    // code holder while `to_address` carries
                                    // the storage target. Today both come from
                                    // the same source.
                                    code_address: f.to,
                                    codehash,
                                    call_type: format_call_type(&f.call_type),
                                    selector: extract_selector_bytes(&f.input),
                                    value_wei: f.value_wei.map(|v| v.to_string()),
                                    gas_provided: f.gas_provided,
                                    gas_used: f.gas_used,
                                    gas_margin: Some(
                                        (f.gas_provided as i64).saturating_sub(f.gas_used as i64),
                                    ),
                                    success: f.success,
                                    parent_gas_at_call: f.parent_gas_at_call,
                                    gas_requested_on_stack: f.gas_requested_on_stack,
                                    eip150_cap_binding: eip150_cap_binding(
                                        f.gas_requested_on_stack,
                                        f.parent_gas_at_call,
                                    ),
                                    state_gas_running: None,
                                    // For successful CREATE/CREATE2 frames the inspector stashes
                                    // the deployed runtime code in `output`; its length is the
                                    // deployed-bytecode size EIP-8037 charges CPSB per byte for.
                                    // All other frames leave this NULL.
                                    deployed_bytecode_len: match f.call_type {
                                        ResCallType::Create | ResCallType::Create2
                                            if f.success =>
                                        {
                                            f.output.as_ref().map(|b| b.len() as u32)
                                        }
                                        _ => None,
                                    },
                                }
                            })
                            .collect();

                        let opcode_count_rows = OpcodeCountRow::from_frames(opcode_frames_ref);

                        Some(DrillInRecord {
                            divergence: divergence_row,
                            call_frames: call_frames_rows,
                            opcode_counts: opcode_count_rows,
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
                // the aggregator wants it on every tx so per-bucket
                // `opcode_totals_7904` covers every cohort, not just drill-ins.
                // Recompute the slice here from the same source.
                let opcode_frames_for_agg =
                    exec_result.map(|r| r.frame_opcode_counts.as_slice()).unwrap_or(&[]);
                aggregators
                    .get_mut(schedule_name)
                    .expect("aggregator exists for every schedule")
                    .observe_tx(
                        reth_research::block_aggregator::TxObservation {
                            bucket,
                            gas_delta: total_delta,
                            state_gas_spent: schedule_state_gas_spent,
                            state_gas_spillover,
                            min_multiplier_to_succeed,
                            is_creation: is_create,
                            has_authorization: authorization_count > 0,
                            has_runtime_state,
                            drill_in_record: drill_in,
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
    gas_limit_multipliers: Vec<u64>,
    backfill: bool,
    backfill_min_block: u64,
    backfill_concurrency: usize,
    metadata_backfill_interval_secs: u64,
    contract_labels_interval_secs: u64,
    function_signatures_interval_secs: u64,
    label_config_path: Option<std::path::PathBuf>,
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
        gas_limit_multipliers,
        backfill,
        backfill_min_block,
        backfill_concurrency,
        metadata_backfill_interval_secs,
        contract_labels_interval_secs,
        function_signatures_interval_secs,
        label_config_path,
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
                    "No research schedules configured. Use --research.eip2780, --research.eip8037, --research.csv, or --research.multiplier"
                ));
            }

            // Build the schedule registry from CLI args
            let registry = research_args
                .build_registry()
                .map_err(|e| eyre::eyre!("Failed to build schedule registry: {}", e))?;

            let db_path = research_args.db_path.clone();
            let start_block = research_args.start_block;
            let max_divergences_per_block = research_args.max_divergences_per_block;
            let gas_limit_multipliers = research_args.gas_limit_multipliers.clone();
            let backfill = research_args.backfill;
            let backfill_min_block = research_args.backfill_min_block;
            let backfill_concurrency = research_args.backfill_concurrency;
            let metadata_backfill_interval_secs =
                research_args.metadata_backfill_interval_secs;
            let contract_labels_interval_secs =
                research_args.contract_labels_interval_secs;
            let function_signatures_interval_secs =
                research_args.function_signatures_interval_secs;
            let label_config_path = research_args.label_config_path.clone();

            info!(
                target: "reth::cli",
                schedules = registry.len(),
                db_path = ?db_path,
                start_block,
                max_divergences_per_block,
                gas_limit_multipliers = ?gas_limit_multipliers,
                backfill,
                backfill_min_block,
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
                    async move {
                        Ok(research_exex(
                            ctx,
                            registry,
                            db_path,
                            start_block,
                            max_divergences_per_block,
                            gas_limit_multipliers,
                            backfill,
                            backfill_min_block,
                            backfill_concurrency,
                            metadata_backfill_interval_secs,
                            contract_labels_interval_secs,
                            function_signatures_interval_secs,
                            label_config_path,
                        ))
                    }
                })
                .launch()
                .await?;

            handle.wait_for_node_exit().await
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::Address;
    use reth_research::divergence::{CallFrame, CallType};

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
        }
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
