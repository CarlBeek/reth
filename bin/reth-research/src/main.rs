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
use alloy_primitives::{keccak256, logs_bloom, Address, Bloom, Bytes, U256};
use clap::Parser;
use futures::TryStreamExt;
use reth_ethereum::{
    exex::{ExExContext, ExExEvent, ExExNotification},
    node::EthereumNode,
};
use reth_evm::{block::BlockExecutorFactory, ConfigureEvm, Evm, EvmFactory, TransactionEnvMut};
use reth_node_api::{BlockTy, FullNodeComponents};
use reth_node_core::args::ResearchArgs;
use reth_primitives_traits::BlockBody;
use reth_provider::StateProviderFactory;
use reth_research::{
    database::{DivergenceDatabase, ScheduleBlockCoverage},
    divergence::{CallFrame, DivergenceLocation, DivergenceType, EventLog},
    schedule::{GasSchedule, RecipientInfo, ScheduleKind, ScheduleRegistry, TxContext},
    ScheduleDivergence, ScheduleInspector, TrackingInspector,
};
use reth_revm::{database::StateProviderDatabase, db::State, Database, DatabaseCommit};
use reth_tracing::tracing::{debug, info, warn};
use revm::{context::BlockEnv, context_interface::Cfg, primitives::hardfork::SpecId};
use revm_interpreter::gas::calculate_initial_tx_gas;
use std::{collections::HashMap, sync::Arc, thread::JoinHandle};
use tokio::sync::mpsc;

enum DbCommand {
    Record(ScheduleDivergence),
    RecordCoverage(ScheduleBlockCoverage),
    DeleteRange { from_block: u64, to_block: u64 },
}

#[derive(Clone)]
struct ScheduleMetadata {
    kind: String,
    description: String,
    config_hash: String,
    affected_opcodes: Option<String>,
    affected_precompiles: Option<String>,
}

#[derive(Default)]
struct BlockCoverageAccumulator {
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

fn flush_pending_divergence_batch(
    divergence_db: &DivergenceDatabase,
    pending_records: &mut Vec<ScheduleDivergence>,
    write_count: &mut u64,
    context: &str,
) {
    if pending_records.is_empty() {
        return;
    }

    match divergence_db.record_schedule_divergences_batch(pending_records) {
        Ok(written) => {
            *write_count += written as u64;
            if *write_count % 100 == 0 {
                debug!(
                    target: "exex::research::db_writer",
                    total_writes = *write_count,
                    "Database writer progress"
                );
            }
        }
        Err(error) => {
            warn!(
                target: "exex::research::db_writer",
                batch_len = pending_records.len(),
                %error,
                context
            );
            for divergence in pending_records.iter() {
                match divergence_db.record_schedule_divergence(divergence) {
                    Ok(_id) => {
                        *write_count += 1;
                    }
                    Err(single_error) => {
                        warn!(
                            target: "exex::research::db_writer",
                            block = divergence.block_number,
                            tx_idx = divergence.tx_index,
                            schedule = divergence.schedule_name,
                            error = %single_error,
                            "Failed to record divergence to database"
                        );
                    }
                }
            }
        }
    }

    pending_records.clear();
}

/// Research ExEx that performs multi-schedule execution analysis on committed blocks.
struct ResearchExEx<Node: FullNodeComponents> {
    /// ExEx context
    ctx: ExExContext<Node>,
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
    /// Start block for analysis
    start_block: u64,
    /// Maximum divergence rows to persist per block.
    max_divergences_per_block: Option<usize>,
    /// Inflate schedule replay gas limits by this factor.
    gas_limit_multiplier: u64,
    /// Channel sender for async database writes
    db_tx: Option<mpsc::UnboundedSender<DbCommand>>,
    /// Handle for async database writer task
    db_writer_task: Option<JoinHandle<()>>,
    /// Statistics
    blocks_processed: u64,
    divergences_found: u64,
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

    /// Synthesize a `DivergenceLocation` from the recorded call frames when the
    /// inspector didn't capture one itself.
    ///
    /// Native-revm schedules (e.g. EIP-8037) drive their gas accounting inside
    /// the modified spec rather than via per-opcode deltas, so the inspector's
    /// `record_divergence` (gated on `apply_gas_delta`) never fires. Without a
    /// location, downstream forensics can't bucket failures by call depth or
    /// contract. As a fallback, we point the location at the deepest failed
    /// frame in the schedule run — that's where the schedule's accounting most
    /// likely diverged from baseline. `pc`/`opcode` are unrecoverable post-hoc;
    /// only `contract` and `call_depth` are filled with real data.
    fn derive_divergence_location(call_frames: &[CallFrame]) -> Option<DivergenceLocation> {
        let deepest_failed = call_frames
            .iter()
            .filter(|f| !f.success)
            .max_by_key(|f| f.depth)?;
        Some(DivergenceLocation {
            contract: deepest_failed.to.unwrap_or(deepest_failed.from),
            function_selectors: Vec::new(),
            pc: 0,
            call_depth: deepest_failed.depth,
            opcode: 0,
            opcode_name: String::new(),
        })
    }

    fn format_affected_opcodes(schedule: &dyn GasSchedule) -> Option<String> {
        let mut opcodes = schedule.affected_opcodes();
        opcodes.sort_unstable();
        (!opcodes.is_empty()).then(|| format!("{opcodes:?}"))
    }

    fn format_affected_precompiles(schedule: &dyn GasSchedule) -> Option<String> {
        let mut addresses: Vec<_> = schedule
            .affected_precompiles()
            .into_iter()
            .map(|address| format!("{address:#x}"))
            .collect();
        addresses.sort();
        (!addresses.is_empty()).then(|| format!("{addresses:?}"))
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

    /// Create a new research ExEx.
    fn new(
        ctx: ExExContext<Node>,
        registry: ScheduleRegistry,
        db_path: std::path::PathBuf,
        start_block: u64,
        max_divergences_per_block: Option<usize>,
        gas_limit_multiplier: u64,
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
                let affected_opcodes = Self::format_affected_opcodes(schedule.as_ref());
                let affected_precompiles = Self::format_affected_precompiles(schedule.as_ref());
                let kind = format!("{:?}", schedule.kind());
                let description = schedule.description().to_string();
                let config_hash = Self::hash_bytes(schedule.config_fingerprint().as_bytes());
                (
                    schedule.name().to_string(),
                    ScheduleMetadata {
                        kind,
                        description,
                        config_hash,
                        affected_opcodes,
                        affected_precompiles,
                    },
                )
            })
            .collect();

        // Initialize database and async writer
        let (db_tx, db_writer_task) = if db_path.to_str() != Some(":memory:") {
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

            // Spawn database writer task
            let (tx, mut rx) = mpsc::unbounded_channel::<DbCommand>();
            let writer_task = std::thread::Builder::new()
                .name("reth-research-db-writer".to_string())
                .spawn(move || {
                const DB_WRITE_BATCH_SIZE: usize = 256;
                let mut write_count = 0u64;
                let mut pending_records = Vec::with_capacity(DB_WRITE_BATCH_SIZE);
                while let Some(cmd) = rx.blocking_recv() {
                    match cmd {
                            DbCommand::Record(divergence) => {
                                pending_records.push(divergence);
                                if pending_records.len() >= DB_WRITE_BATCH_SIZE {
                                    flush_pending_divergence_batch(
                                        &divergence_db,
                                        &mut pending_records,
                                        &mut write_count,
                                        "Batch divergence write failed, retrying individually",
                                    );
                                }
                            }
                        DbCommand::RecordCoverage(coverage) => {
                            flush_pending_divergence_batch(
                                &divergence_db,
                                &mut pending_records,
                                &mut write_count,
                                "Failed to flush pending divergence batch before coverage write, retrying individually",
                            );
                                if let Err(error) =
                                    divergence_db.record_schedule_block_coverage(&coverage)
                                {
                                    warn!(
                                        target: "exex::research::db_writer",
                                        block = coverage.block_number,
                                        schedule = coverage.schedule_name,
                                        %error,
                                        "Failed to record schedule block coverage"
                                    );
                                }
                            }
                        DbCommand::DeleteRange { from_block, to_block } => {
                                flush_pending_divergence_batch(
                                    &divergence_db,
                                    &mut pending_records,
                                    &mut write_count,
                                    "Failed to flush pending divergence batch before delete, retrying individually",
                                );
                                match divergence_db
                                    .delete_schedule_divergences_in_block_range(from_block, to_block)
                                {
                                    Ok(deleted) => {
                                        info!(
                                            target: "exex::research::db_writer",
                                            from_block,
                                            to_block,
                                            deleted,
                                            "Deleted schedule divergences for non-canonical block range"
                                        );
                                    }
                                    Err(e) => {
                                        warn!(
                                            target: "exex::research::db_writer",
                                            from_block,
                                            to_block,
                                            error = %e,
                                            "Failed to delete non-canonical schedule divergences"
                                        );
                                    }
                                }
                                match divergence_db
                                    .delete_schedule_block_coverage_in_block_range(from_block, to_block)
                                {
                                    Ok(deleted) => {
                                        info!(
                                            target: "exex::research::db_writer",
                                            from_block,
                                            to_block,
                                            deleted,
                                            "Deleted schedule coverage rows for non-canonical block range"
                                        );
                                    }
                                    Err(e) => {
                                        warn!(
                                            target: "exex::research::db_writer",
                                            from_block,
                                            to_block,
                                            error = %e,
                                            "Failed to delete non-canonical schedule coverage"
                                        );
                                    }
                                }
                            }
                    }
                }
                flush_pending_divergence_batch(
                    &divergence_db,
                    &mut pending_records,
                    &mut write_count,
                    "Failed to flush pending divergence batch on shutdown, retrying individually",
                );
                info!(
                    target: "exex::research::db_writer",
                    total_writes = write_count,
                    "Database writer task exiting"
                );
            })
            .map_err(|err| eyre::eyre!("failed to spawn db writer thread: {err}"))?;

            (Some(tx), Some(writer_task))
        } else {
            (None, None)
        };

        Ok(Self {
            ctx,
            registry,
            all_schedules,
            execution_schedules,
            execution_schedule_indices,
            has_intrinsic_schedules,
            schedule_metadata,
            start_block,
            max_divergences_per_block,
            gas_limit_multiplier: gas_limit_multiplier.max(1),
            db_tx,
            db_writer_task,
            blocks_processed: 0,
            divergences_found: 0,
        })
    }

    /// Run the ExEx.
    async fn run(mut self) -> eyre::Result<()> {
        info!(
            target: "exex::research",
            start_block = self.start_block,
            schedule_count = self.registry.len(),
            "Multi-schedule Research ExEx started"
        );

        while let Some(notification) = self.ctx.notifications.try_next().await? {
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
                            self.registry.len()
                        );

                        match self.analyze_block(block).await {
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
                        self.delete_divergences_in_block_range(from_block, to_block);
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

                        match self.analyze_block(block).await {
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
                        self.delete_divergences_in_block_range(from_block, to_block);
                    }

                    info!(
                        target: "exex::research",
                        reverted_tip = old.tip().number(),
                        "Chain reverted"
                    );
                }
            }
        }

        // Flush pending DB commands on shutdown.
        drop(self.db_tx.take());
        if let Some(task) = self.db_writer_task.take() {
            if let Err(err) = task.join() {
                warn!(target: "exex::research", error = ?err, "Database writer task join failed during shutdown");
            }
        }

        Ok(())
    }

    /// Analyze a single block using multi-schedule execution.
    async fn analyze_block(
        &mut self,
        block: &reth_primitives_traits::RecoveredBlock<BlockTy<Node::Types>>,
    ) -> eyre::Result<bool> {
        let block_number = block.number();
        let block_hash = block.hash();
        let parent_hash = block.parent_hash();
        let block_start = std::time::Instant::now();
        let provider = self.ctx.provider();
        let block_timestamp = block.timestamp();

        let evm_env = match self.ctx.evm_config().evm_env(block.header()) {
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
        let mut block_coverage: HashMap<String, BlockCoverageAccumulator> = self
            .all_schedules
            .iter()
            .map(|schedule| (schedule.name().to_string(), BlockCoverageAccumulator::default()))
            .collect();
        let mut recorded_divergences = 0usize;

        for (tx_idx, tx) in block.transactions_recovered().enumerate() {
            let tx_env = self.ctx.evm_config().tx_env(tx);

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
            let schedule_execution_gas_limit = gas_limit.saturating_mul(self.gas_limit_multiplier);
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
            let mut normal_evm = self.ctx.evm_config().evm_with_env_and_inspector(
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

            // Each schedule executes against the pre-tx state (normal_db before
            // baseline commit) so that re-execution sees exactly the same state
            // the baseline saw. We do NOT commit schedule results — each
            // transaction is evaluated independently, so schedule-induced state
            // drift from tx N never contaminates tx N+1's analysis. The baseline
            // commit is deferred until after the re-execution loop.
            for schedule in self.execution_schedules.iter() {
                let mut schedule_evm_env = evm_env.clone();
                let native_env_configured = schedule.configure_evm_env(&mut schedule_evm_env);

                // Capture the per-tx state-gas reservoir budget under the schedule's
                // EVM env. For non-EIP-8037 schedules, both values stay 0 (no
                // reservoir). For EIP-8037 schedules, we recompute against the
                // *original* gas_limit so the recorded reservoir reflects what a
                // mainnet tx would have, not the inflated replay limit.
                let (schedule_initial_state_gas, schedule_initial_reservoir) =
                    if schedule_evm_env.cfg_env.is_amsterdam_eip8037_enabled() {
                        let init_gas = calculate_initial_tx_gas(
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
                        (init_gas.initial_state_gas, reservoir)
                    } else {
                        (0, 0)
                    };

                if self.gas_limit_multiplier > 1 {
                    schedule_evm_env.cfg_env.disable_block_gas_limit = true;
                    schedule_evm_env.cfg_env.disable_balance_check = true;
                    if !schedule_evm_env.cfg_env.is_amsterdam_eip8037_enabled() {
                        schedule_evm_env.cfg_env.tx_gas_limit_cap = Some(u64::MAX);
                    }
                }

                // For "Both" schedules (intrinsic + execution), adjust gas_limit
                // so execution gets the correct gas budget under the new intrinsic.
                // The EVM always deducts baseline intrinsic, so we offset gas_limit
                // to compensate: if schedule intrinsic is higher, execution gets less
                // gas (and vice versa).
                //
                // When the schedule lowers intrinsic (negative delta), the adjusted
                // gas_limit can exceed the original. We cap at the replay gas limit.
                // If the replay limit is inflated, balance checks are disabled above
                // so increased max cost does not mask the schedule result.
                //
                // Gas total reconstruction (below) is still correct: it replaces
                // baseline intrinsic with schedule intrinsic in the reported total,
                // so the gas delta accounts for the intrinsic difference.
                //
                // If `intrinsic_delta` is large enough to drive `adjusted` below the
                // EVM's baseline intrinsic cost, `transact()` will fail (the EVM
                // rejects the tx before execution begins). This is caught below and
                // recorded as a definitive schedule-induced failure.
                let mut sched_tx_env = tx_env.clone();
                sched_tx_env.set_gas_limit(schedule_execution_gas_limit);
                if schedule.modifies_intrinsic() && !schedule.uses_native_intrinsic_gas() {
                    if let Some(ref ctx) = tx_context {
                        if let Some(schedule_intrinsic) = schedule.intrinsic_gas(ctx) {
                            let intrinsic_delta =
                                i128::from(schedule_intrinsic) - i128::from(baseline_intrinsic_gas);
                            let replay_limit = i128::from(schedule_execution_gas_limit);
                            let raw_adjusted = replay_limit - intrinsic_delta;
                            let adjusted = raw_adjusted.clamp(0, replay_limit) as u64;
                            if raw_adjusted < 0 || raw_adjusted > replay_limit {
                                debug!(
                                    target: "exex::research",
                                    block = block_number,
                                    tx_idx,
                                    schedule = schedule.name(),
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

                let mut inspector = ScheduleInspector::new(schedule.clone());
                let mut evm = self.ctx.evm_config().evm_with_env_and_inspector(
                    &mut normal_db,
                    schedule_evm_env,
                    &mut inspector,
                );
                let result = match evm.transact(sched_tx_env) {
                    Ok(result) => result,
                    Err(e) => {
                        debug!(
                            target: "exex::research",
                            block = block_number,
                            tx_idx,
                            schedule = schedule.name(),
                            native_env_configured,
                            error = ?e,
                            "Schedule execution failed"
                        );
                        drop(evm);
                        // Record as definitive failure — the schedule's gas changes
                        // (e.g., adjusted gas_limit falling below baseline intrinsic)
                        // caused the EVM to reject the transaction entirely.
                        schedule_results.push(PerScheduleResult {
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
                drop(evm);

                let sched_success = result.result.is_success();
                let sched_gas_used = result.result.tx_gas_used();
                let sched_total_gas_spent = result.result.gas().total_gas_spent();
                let sched_state_gas_spent = result.result.gas().state_gas_spent();
                let sched_floor_gas = result.result.gas().floor_gas();
                let sched_gas_refunded = result.result.gas().inner_refunded();
                let op_counts = Self::serialize_trace(inspector.operation_counts());
                let insp_result = inspector.result();
                let halt_info = match &result.result {
                    revm::context_interface::result::ExecutionResult::Halt { reason, .. } => {
                        Some(format!("Execution halted: {reason:?}"))
                    }
                    _ => None,
                };
                let (output_hash, output_len) = Self::output_hash_and_len(&result.result);
                let created_address = result.result.created_address().map(Self::hex_address);
                let log_count = result.result.logs().len() as u64;
                let logs_bloom = Self::logs_bloom_hex(&result.result);
                let call_frames = inspector.call_frames().to_vec();
                let event_logs = inspector.event_logs().to_vec();

                schedule_results.push(PerScheduleResult {
                    success: sched_success,
                    gas_used: sched_gas_used,
                    total_gas_spent: sched_total_gas_spent,
                    state_gas_spent: sched_state_gas_spent,
                    initial_state_gas: schedule_initial_state_gas,
                    initial_reservoir: schedule_initial_reservoir,
                    floor_gas: sched_floor_gas,
                    gas_refunded: sched_gas_refunded,
                    operation_counts: op_counts,
                    oog_info: insp_result
                        .oog_info
                        .as_ref()
                        .map(|oog| format!("{oog:?}"))
                        .or(halt_info),
                    divergence_location: insp_result
                        .divergence_location
                        .clone()
                        .or_else(|| Self::derive_divergence_location(&call_frames))
                        .as_ref()
                        .map(|loc| format!("{loc:?}")),
                    call_frames_hash: Self::hash_serialized(&call_frames),
                    event_logs_hash: Self::hash_serialized(&event_logs),
                    call_frames,
                    event_logs,
                    output_hash,
                    output_len,
                    created_address,
                    log_count,
                    logs_bloom,
                });
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
                        let call_tree_diverged = r.call_frames != baseline_call_frames;
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

                // Use the actual gas difference as the total delta rather than
                // the inspector's cumulative opcode deltas, since cascading
                // effects (different execution paths) make the true difference
                // diverge from the sum of per-opcode adjustments.
                let total_delta = schedule_gas as i64 - normal_gas_used as i64;
                let would_oog = !schedule_success && normal_success;

                // Record divergence if there's a gas delta or status change
                let status_changed = schedule_success != normal_success;
                let gas_changed = total_delta != 0;
                if gas_changed ||
                    would_oog ||
                    status_changed ||
                    call_tree_diverged ||
                    event_logs_diverged ||
                    output_changed ||
                    created_address_changed ||
                    logs_bloom_changed
                {
                    let divergence_type = if would_oog || schedule_success != normal_success {
                        DivergenceType::Status
                    } else if event_logs_diverged {
                        DivergenceType::EventLogs
                    } else if call_tree_diverged {
                        DivergenceType::CallTree
                    } else {
                        DivergenceType::GasPattern
                    };

                    let gas_efficiency_ratio = if normal_gas_used > 0 {
                        Some(schedule_gas as f64 / normal_gas_used as f64)
                    } else {
                        None
                    };
                    // Min gas-limit multiplier required for the schedule to succeed.
                    // Only meaningful when the replay actually completed under the
                    // (possibly inflated) replay limit; otherwise the gas_used we'd
                    // divide by is the OOG cap, not the true minimum.
                    let min_multiplier_to_succeed = if schedule_replay_success && gas_limit > 0 {
                        Some(schedule_gas as f64 / gas_limit as f64)
                    } else {
                        None
                    };
                    let metadata = self
                        .schedule_metadata
                        .get(schedule_name)
                        .cloned()
                        .expect("all schedules should have static metadata");

                    let div = ScheduleDivergence {
                        schedule_name: schedule_name.to_string(),
                        block_number,
                        tx_index: tx_idx as u64,
                        tx_hash,
                        timestamp: block_timestamp,
                        divergence_type,
                        schedule_kind: metadata.kind.clone(),
                        schedule_description: metadata.description.clone(),
                        schedule_config_hash: metadata.config_hash.clone(),
                        block_hash,
                        parent_hash,
                        baseline_success: normal_success,
                        baseline_gas_used: normal_gas_used,
                        baseline_intrinsic_gas,
                        schedule_success,
                        schedule_gas_used: schedule_gas,
                        schedule_intrinsic_gas,
                        gas_delta: total_delta,
                        gas_efficiency_ratio,
                        tx_category: tx_category.map(|s| s.to_string()),
                        affected_opcodes: metadata.affected_opcodes.clone(),
                        affected_precompiles: metadata.affected_precompiles.clone(),
                        oog_info,
                        divergence_location,
                        operation_counts: formatted_op_counts,
                        baseline_call_frames: baseline_call_frames_json,
                        schedule_call_frames: schedule_call_frames_json,
                        baseline_event_logs: baseline_event_logs_json,
                        schedule_event_logs: schedule_event_logs_json,
                        baseline_call_frames_hash: baseline_call_frames_hash_row,
                        schedule_call_frames_hash: schedule_call_frames_hash_row,
                        baseline_event_logs_hash: baseline_event_logs_hash_row,
                        schedule_event_logs_hash: schedule_event_logs_hash_row,
                        status_changed,
                        gas_changed,
                        call_tree_changed: call_tree_diverged,
                        event_logs_changed: event_logs_diverged,
                        output_changed,
                        created_address_changed,
                        logs_bloom_changed,
                        sender: Self::hex_address(sender),
                        recipient: recipient.map(Self::hex_address),
                        value_wei: value.to_string(),
                        input_len: input.len() as u64,
                        input_zero_bytes,
                        input_nonzero_bytes,
                        tx_gas_limit: gas_limit,
                        access_list_accounts,
                        access_list_storage_slots,
                        authorization_count,
                        is_create,
                        baseline_output_len,
                        schedule_output_len,
                        baseline_output_hash: baseline_output_hash.clone(),
                        schedule_output_hash,
                        baseline_created_address: baseline_created_address.clone(),
                        schedule_created_address,
                        baseline_log_count,
                        schedule_log_count,
                        baseline_logs_bloom: baseline_logs_bloom.clone(),
                        schedule_logs_bloom,
                        would_fit_in_original_limit: schedule_success,
                        min_multiplier_to_succeed,
                        baseline_total_gas_spent,
                        baseline_gas_refunded,
                        schedule_total_gas_spent,
                        schedule_state_gas_spent,
                        schedule_initial_state_gas,
                        schedule_initial_reservoir,
                        schedule_floor_gas,
                        schedule_gas_refunded,
                    };

                    let should_record = self
                        .max_divergences_per_block
                        .map(|max| recorded_divergences < max)
                        .unwrap_or(true);
                    if should_record {
                        self.record_divergence(div);
                        self.divergences_found += 1;
                        recorded_divergences += 1;
                    } else if recorded_divergences ==
                        self.max_divergences_per_block.unwrap_or_default()
                    {
                        warn!(
                            target: "exex::research",
                            block = block_number,
                            max_divergences_per_block = self.max_divergences_per_block,
                            "Reached divergence persistence limit for block; additional divergences will be counted in coverage only"
                        );
                        recorded_divergences += 1;
                    } else {
                        recorded_divergences += 1;
                    }
                }

                let coverage = block_coverage
                    .get_mut(schedule_name)
                    .expect("every schedule should have a coverage accumulator");
                coverage.tx_count += 1;
                coverage.total_baseline_gas_used += normal_gas_used;
                coverage.total_schedule_gas_used += schedule_gas;
                coverage.total_gas_delta += total_delta;
                if gas_changed ||
                    would_oog ||
                    status_changed ||
                    call_tree_diverged ||
                    event_logs_diverged ||
                    output_changed ||
                    created_address_changed ||
                    logs_bloom_changed
                {
                    coverage.divergence_count += 1;
                }
                if status_changed || would_oog {
                    coverage.status_divergence_count += 1;
                }
                if gas_changed {
                    coverage.gas_divergence_count += 1;
                }
                if call_tree_diverged {
                    coverage.call_tree_divergence_count += 1;
                }
                if event_logs_diverged {
                    coverage.event_log_divergence_count += 1;
                }
                if output_changed {
                    coverage.output_divergence_count += 1;
                }
                if created_address_changed {
                    coverage.created_address_divergence_count += 1;
                }
                if logs_bloom_changed {
                    coverage.logs_bloom_divergence_count += 1;
                }
            }
        }

        for schedule in &self.all_schedules {
            if let Some(coverage) = block_coverage.remove(schedule.name()) {
                let metadata = self
                    .schedule_metadata
                    .get(schedule.name())
                    .expect("all schedules should have static metadata");
                self.record_block_coverage(ScheduleBlockCoverage {
                    schedule_name: schedule.name().to_string(),
                    schedule_kind: metadata.kind.clone(),
                    schedule_config_hash: metadata.config_hash.clone(),
                    block_number,
                    block_hash,
                    parent_hash,
                    timestamp: block_timestamp,
                    tx_count: coverage.tx_count,
                    divergence_count: coverage.divergence_count,
                    status_divergence_count: coverage.status_divergence_count,
                    gas_divergence_count: coverage.gas_divergence_count,
                    call_tree_divergence_count: coverage.call_tree_divergence_count,
                    event_log_divergence_count: coverage.event_log_divergence_count,
                    output_divergence_count: coverage.output_divergence_count,
                    created_address_divergence_count: coverage.created_address_divergence_count,
                    logs_bloom_divergence_count: coverage.logs_bloom_divergence_count,
                    total_baseline_gas_used: coverage.total_baseline_gas_used,
                    total_schedule_gas_used: coverage.total_schedule_gas_used,
                    total_gas_delta: coverage.total_gas_delta,
                });
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

    /// Record a divergence to database.
    fn record_divergence(&self, divergence: ScheduleDivergence) {
        if let Some(ref tx) = self.db_tx {
            let block_number = divergence.block_number;
            let tx_index = divergence.tx_index;
            let gas_delta = divergence.gas_delta;
            match tx.send(DbCommand::Record(divergence)) {
                Ok(()) => {
                    debug!(
                        target: "exex::research",
                        block = block_number,
                        tx_idx = tx_index,
                        gas_delta,
                        "Divergence queued for database"
                    );
                }
                Err(e) => {
                    let error = e.to_string();
                    let DbCommand::Record(divergence) = e.0 else {
                        return;
                    };
                    warn!(
                        target: "exex::research",
                        block = divergence.block_number,
                        tx_idx = divergence.tx_index,
                        schedule = divergence.schedule_name,
                        %error,
                        "Failed to send divergence to database writer"
                    );
                }
            }
        } else {
            info!(
                target: "exex::research",
                block = divergence.block_number,
                tx_idx = divergence.tx_index,
                schedule = divergence.schedule_name,
                gas_delta = divergence.gas_delta,
                "Divergence detected (no database configured)"
            );
        }
    }

    fn delete_divergences_in_block_range(&self, from_block: u64, to_block: u64) {
        if let Some(ref tx) = self.db_tx {
            if let Err(e) = tx.send(DbCommand::DeleteRange { from_block, to_block }) {
                warn!(
                    target: "exex::research",
                    from_block,
                    to_block,
                    error = %e,
                    "Failed to queue non-canonical divergence cleanup"
                );
            } else {
                info!(
                    target: "exex::research",
                    from_block,
                    to_block,
                    "Queued non-canonical divergence cleanup"
                );
            }
        }
    }

    fn record_block_coverage(&self, coverage: ScheduleBlockCoverage) {
        if let Some(ref tx) = self.db_tx {
            if let Err(e) = tx.send(DbCommand::RecordCoverage(coverage)) {
                warn!(
                    target: "exex::research",
                    error = %e,
                    "Failed to queue schedule coverage for database writer"
                );
            }
        }
    }
}

async fn research_exex<Node: FullNodeComponents>(
    ctx: ExExContext<Node>,
    registry: ScheduleRegistry,
    db_path: std::path::PathBuf,
    start_block: u64,
    max_divergences_per_block: Option<usize>,
    gas_limit_multiplier: u64,
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
        gas_limit_multiplier,
    )?
    .run()
    .await
}

fn main() -> eyre::Result<()> {
    reth_ethereum::cli::Cli::<reth_ethereum::cli::chainspec::EthereumChainSpecParser, ResearchArgs>::parse()
        .run(|builder, research_args: ResearchArgs| async move {
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
            let gas_limit_multiplier = research_args.gas_limit_multiplier;

            info!(
                target: "reth::cli",
                schedules = registry.len(),
                db_path = ?db_path,
                start_block,
                max_divergences_per_block,
                gas_limit_multiplier,
                "Starting multi-schedule research mode"
            );

            let handle = builder
                .node(EthereumNode::default())
                .install_exex("research", move |ctx| {
                    let registry = registry.clone();
                    let db_path = db_path.clone();
                    async move {
                        Ok(research_exex(
                            ctx,
                            registry,
                            db_path,
                            start_block,
                            max_divergences_per_block,
                            gas_limit_multiplier,
                        ))
                    }
                })
                .launch()
                .await?;

            handle.wait_for_node_exit().await
        })
}
