//! Multi-Schedule Research Mode Execution Extension
//!
//! This ExEx performs multi-schedule execution analysis to detect divergences caused by
//! modified gas costs across multiple gas pricing experiments simultaneously.
//!
//! Supported schedules:
//! - EIP-2780: Reduced intrinsic gas based on transaction category
//! - CSV Pricing: Per-opcode/precompile gas repricing from CSV files
//! - Multiplier: Uniform gas cost multiplication
//!
//! Run with:
//!
//! ```sh
//! cargo run --release -p reth-research-bin node --dev --dev.block-time 5s \
//!   --research.eip2780 \
//!   --research.csv 7904-prelim=./schedules/7904_prelim.csv \
//!   --research.db-path ./divergences.db
//! ```

use alloy_consensus::{constants::KECCAK_EMPTY, transaction::TxHashRef, BlockHeader, Transaction};
use alloy_primitives::{Address, Bytes, U256};
use clap::Parser;
use futures::TryStreamExt;
use reth_ethereum::{
    exex::{ExExContext, ExExEvent, ExExNotification},
    node::EthereumNode,
};
use reth_evm::{ConfigureEvm, Evm, TransactionEnv};
use reth_node_api::{BlockTy, FullNodeComponents};
use reth_node_core::args::ResearchArgs;
use reth_primitives_traits::BlockBody;
use reth_provider::StateProviderFactory;
use reth_research::{
    database::DivergenceDatabase,
    divergence::DivergenceType,
    schedule::{GasSchedule, RecipientInfo, ScheduleKind, ScheduleRegistry, TxContext},
    ScheduleDivergence, ScheduleInspector,
};
use reth_revm::{database::StateProviderDatabase, db::State, Database, DatabaseCommit};
use reth_tracing::tracing::{debug, info, warn};
use std::{collections::HashMap, sync::Arc};
use tokio::{
    sync::mpsc,
    task::JoinHandle,
    time::{self, Duration, MissedTickBehavior},
};

enum DbCommand {
    Record(ScheduleDivergence),
    DeleteRange { from_block: u64, to_block: u64 },
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
    schedule_metadata: HashMap<String, (Option<String>, Option<String>)>,
    /// Start block for analysis
    start_block: u64,
    /// Channel sender for async database writes
    db_tx: Option<mpsc::UnboundedSender<DbCommand>>,
    /// Handle for async database writer task
    db_writer_task: Option<JoinHandle<()>>,
    /// Statistics
    blocks_processed: u64,
    divergences_found: u64,
}

impl<Node: FullNodeComponents> ResearchExEx<Node> {
    fn baseline_intrinsic_gas_from_parts(is_create: bool, input: &[u8]) -> u64 {
        let base = if is_create { 53_000 } else { 21_000 };
        let zero_bytes = input.iter().filter(|&&b| b == 0).count() as u64;
        let nonzero_bytes = input.len() as u64 - zero_bytes;
        base + (zero_bytes * 4 + nonzero_bytes * 16)
    }

    /// Create a new research ExEx.
    fn new(
        ctx: ExExContext<Node>,
        registry: ScheduleRegistry,
        db_path: std::path::PathBuf,
        start_block: u64,
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
        let schedule_metadata: HashMap<String, (Option<String>, Option<String>)> = all_schedules
            .iter()
            .map(|schedule| {
                let metadata = if schedule.modifies_execution() {
                    (
                        Some(format!("{:?}", schedule.affected_opcodes())),
                        Some(format!("{:?}", schedule.affected_precompiles())),
                    )
                } else {
                    (None, None)
                };
                (schedule.name().to_string(), metadata)
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
            let writer_task = tokio::spawn(async move {
                const DB_WRITE_BATCH_SIZE: usize = 256;
                const DB_WRITE_FLUSH_INTERVAL_MS: u64 = 50;
                let mut write_count = 0u64;
                let mut pending_records = Vec::with_capacity(DB_WRITE_BATCH_SIZE);
                let mut flush_tick =
                    time::interval(Duration::from_millis(DB_WRITE_FLUSH_INTERVAL_MS));
                flush_tick.set_missed_tick_behavior(MissedTickBehavior::Delay);

                let mut rx_closed = false;
                while !rx_closed {
                    let cmd = tokio::select! {
                        cmd = rx.recv(), if !rx_closed => cmd,
                        _ = flush_tick.tick() => {
                            flush_pending_divergence_batch(
                                &divergence_db,
                                &mut pending_records,
                                &mut write_count,
                                "Timed batch divergence flush failed, retrying individually",
                            );
                            continue;
                        }
                    };

                    let Some(cmd) = cmd else {
                        rx_closed = true;
                        continue;
                    };

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
            });

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
            if let Err(err) = task.await {
                warn!(
                    target: "exex::research",
                    error = ?err,
                    "Database writer task join failed during shutdown"
                );
            }
        }

        Ok(())
    }

    /// Analyze a single block using multi-schedule execution.
    async fn analyze_block(
        &mut self,
        block: &reth_primitives_traits::RecoveredBlock<BlockTy<Node::Types>>,
    ) -> eyre::Result<bool>
    where
        Node::Evm: ConfigureEvm,
    {
        let block_number = block.number();
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
        // Create one DB per execution-modifying schedule so each gets its own
        // state that tracks cumulative changes across transactions in the block.
        //
        // State drift is intentional: if tx N OOGs under a schedule (changing
        // state), tx N+1 starts from that diverged state, modeling cascading
        // effects accurately. The reported deltas for later transactions thus
        // include second-order state differences, not just opcode cost changes.
        //
        // Each schedule needs its own `StateProvider` because the provider is
        // consumed when building the `State`. We call `history_by_block_number`
        // N times — one per schedule — which may hit disk each time.
        let mut schedule_dbs = {
            let n = self.execution_schedules.len();
            let mut dbs = Vec::with_capacity(n);
            for _ in 0..n {
                let state = if block_number > 0 {
                    match provider.history_by_block_number(block_number - 1) {
                        Ok(state) => state,
                        Err(err) => {
                            debug!(
                                target: "exex::research",
                                block = block_number,
                                %err,
                                "Historical state not yet available for schedule execution, \
                                 skipping block (expected during initial pipeline sync)"
                            );
                            return Ok(false);
                        }
                    }
                } else {
                    provider.latest()?
                };
                dbs.push(State::builder().with_database(StateProviderDatabase::new(state)).build());
            }
            dbs
        };

        for (tx_idx, tx) in block.transactions_recovered().enumerate() {
            let tx_env = self.ctx.evm_config().tx_env(tx);

            // Extract transaction fields using Transaction trait
            // Use kind() to get TxKind which tells us if it's create or call
            let tx_kind = tx.kind();
            let is_create = tx_kind.is_create();
            let recipient: Option<Address> = tx_kind.to().copied();
            let gas_limit: u64 = tx.gas_limit();
            let baseline_intrinsic_gas =
                Self::baseline_intrinsic_gas_from_parts(is_create, tx.input());
            let tx_context = if self.has_intrinsic_schedules {
                let sender = tx.signer();
                let value: U256 = tx.value();
                let input: Bytes = tx.input().clone();
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
                    sender,
                    recipient,
                    value,
                    input,
                    gas_limit,
                    is_create,
                    recipient_info,
                })
            } else {
                None
            };

            // --- EXECUTION: Baseline ---
            let mut normal_evm =
                self.ctx.evm_config().evm_with_env(&mut normal_db, evm_env.clone());
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
            normal_db.commit(normal_result.state);

            // --- EXECUTION: Per-schedule re-execution with gas modifications ---
            // Each execution-modifying schedule gets its own full execution pass
            // so gas changes propagate naturally through subcalls.
            struct PerScheduleResult {
                success: bool,
                gas_used: u64,
                operation_counts: String,
                oog_info: Option<String>,
                divergence_location: Option<String>,
            }

            // Indexed parallel to self.execution_schedules — accessed by schedule
            // name lookup during the analysis phase below.
            let mut schedule_results: Vec<PerScheduleResult> =
                Vec::with_capacity(self.execution_schedules.len());

            for (sched_idx, schedule) in self.execution_schedules.iter().enumerate() {
                // For "Both" schedules (intrinsic + execution), adjust gas_limit
                // so execution gets the correct gas budget under the new intrinsic.
                // The EVM always deducts baseline intrinsic, so we offset gas_limit
                // to compensate: if schedule intrinsic is higher, execution gets less
                // gas (and vice versa).
                //
                // When the schedule lowers intrinsic (negative delta), the adjusted
                // gas_limit would exceed the original. We cap at gas_limit to avoid
                // InsufficientFunds failures (the sender only had balance for the
                // original gas_limit * gas_price). This means the extra execution
                // budget from cheaper intrinsic is not modeled.
                //
                // **Impact**: a "Both" schedule that lowers intrinsic may produce
                // false OOGs in the execution pass. Since the execution portion
                // doesn't get the extra budget freed by cheaper intrinsic, opcodes
                // that would have succeeded with the full budget may OOG. The
                // reported result is conservative (may report failures that wouldn't
                // actually occur). This trade-off is acceptable because
                // InsufficientFunds from exceeding gas_limit would produce a more
                // confusing false failure. Gas total reconstruction (below) is still
                // correct: it replaces baseline intrinsic with schedule intrinsic in
                // the reported total, so the gas delta accounts for the intrinsic
                // difference even though the execution portion was capped.
                //
                // If `intrinsic_delta` is large enough to drive `adjusted` below the
                // EVM's baseline intrinsic cost, `transact()` will fail (the EVM
                // rejects the tx before execution begins). This is caught below and
                // recorded as a definitive schedule-induced failure.
                let mut sched_tx_env = tx_env.clone();
                if schedule.modifies_intrinsic() {
                    if let Some(ref ctx) = tx_context {
                        if let Some(schedule_intrinsic) = schedule.intrinsic_gas(ctx) {
                            let intrinsic_delta =
                                schedule_intrinsic as i64 - baseline_intrinsic_gas as i64;
                            let raw_adjusted = gas_limit as i64 - intrinsic_delta;
                            let adjusted = raw_adjusted.clamp(0, gas_limit as i64) as u64;
                            if raw_adjusted < 0 || raw_adjusted > gas_limit as i64 {
                                debug!(
                                    target: "exex::research",
                                    block = block_number,
                                    tx_idx,
                                    schedule = schedule.name(),
                                    %intrinsic_delta,
                                    %gas_limit,
                                    %adjusted,
                                    "Gas limit clamped for 'Both' schedule — execution \
                                     budget may be conservative"
                                );
                            }
                            sched_tx_env.set_gas_limit(adjusted);
                        }
                    }
                }

                let db = &mut schedule_dbs[sched_idx];
                let mut inspector = ScheduleInspector::new(schedule.clone());
                let mut evm = self.ctx.evm_config().evm_with_env_and_inspector(
                    &mut *db,
                    evm_env.clone(),
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
                            operation_counts: String::new(),
                            oog_info: Some(format!("EVM transact failed: {e:?}")),
                            divergence_location: None,
                        });
                        continue;
                    }
                };
                drop(evm);

                let sched_success = result.result.is_success();
                let sched_gas_used = result.result.gas_used();
                let op_counts = format!("{:?}", inspector.operation_counts());
                let insp_result = inspector.result();

                db.commit(result.state);

                schedule_results.push(PerScheduleResult {
                    success: sched_success,
                    gas_used: sched_gas_used,
                    operation_counts: op_counts,
                    oog_info: insp_result.oog_info.as_ref().map(|oog| format!("{oog:?}")),
                    divergence_location: insp_result
                        .divergence_location
                        .as_ref()
                        .map(|loc| format!("{loc:?}")),
                });
            }

            let normal_gas_used = normal_result.result.gas_used();
            let normal_success = normal_result.result.is_success();
            let tx_hash = *tx.tx_hash();

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
                    formatted_op_counts,
                    oog_info,
                    divergence_location,
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
                        (
                            gas,
                            success,
                            Some(r.operation_counts.clone()),
                            r.oog_info.clone(),
                            r.divergence_location.clone(),
                        )
                    }
                    None => {
                        // Intrinsic-only schedule: estimate from baseline
                        let gas = (normal_gas_used as i64 + intrinsic_delta).max(0) as u64;
                        let success = normal_success && gas <= gas_limit;
                        (gas, success, None, None, None)
                    }
                };

                // Use the actual gas difference as the total delta rather than
                // the inspector's cumulative opcode deltas, since cascading
                // effects (different execution paths) make the true difference
                // diverge from the sum of per-opcode adjustments.
                let total_delta = schedule_gas as i64 - normal_gas_used as i64;
                let would_oog = !schedule_success && normal_success;

                // Record divergence if there's a gas delta or status change
                if total_delta != 0 || would_oog || schedule_success != normal_success {
                    let divergence_type = if would_oog || schedule_success != normal_success {
                        DivergenceType::Status
                    } else {
                        DivergenceType::GasPattern
                    };

                    let gas_efficiency_ratio = if normal_gas_used > 0 {
                        Some(schedule_gas as f64 / normal_gas_used as f64)
                    } else {
                        None
                    };
                    let (affected_opcodes, affected_precompiles) =
                        self.schedule_metadata.get(schedule_name).cloned().unwrap_or((None, None));

                    let div = ScheduleDivergence {
                        schedule_name: schedule_name.to_string(),
                        block_number,
                        tx_index: tx_idx as u64,
                        tx_hash,
                        timestamp: block_timestamp,
                        divergence_type,
                        baseline_success: normal_success,
                        baseline_gas_used: normal_gas_used,
                        baseline_intrinsic_gas,
                        schedule_success,
                        schedule_gas_used: schedule_gas,
                        schedule_intrinsic_gas,
                        gas_delta: total_delta,
                        gas_efficiency_ratio,
                        tx_category: tx_category.map(|s| s.to_string()),
                        affected_opcodes,
                        affected_precompiles,
                        oog_info,
                        divergence_location,
                        operation_counts: formatted_op_counts,
                    };

                    self.record_divergence(div);
                    self.divergences_found += 1;
                }
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
}

async fn research_exex<Node: FullNodeComponents>(
    ctx: ExExContext<Node>,
    registry: ScheduleRegistry,
    db_path: std::path::PathBuf,
    start_block: u64,
) -> eyre::Result<()> {
    ResearchExEx::new(ctx, registry, db_path, start_block)?.run().await
}

fn main() -> eyre::Result<()> {
    reth_ethereum::cli::Cli::<reth_ethereum::cli::chainspec::EthereumChainSpecParser, ResearchArgs>::parse()
        .run(|builder, research_args: ResearchArgs| async move {
        // Check if any schedules are configured
            if !research_args.has_schedules() {
                return Err(eyre::eyre!(
                    "No research schedules configured. Use --research.eip2780, --research.csv, or --research.multiplier"
                ));
            }

            // Build the schedule registry from CLI args
            let registry = research_args
                .build_registry()
                .map_err(|e| eyre::eyre!("Failed to build schedule registry: {}", e))?;

            let db_path = research_args.db_path.clone();
            let start_block = research_args.start_block;

            info!(
                target: "reth::cli",
                schedules = registry.len(),
                db_path = ?db_path,
                start_block,
                "Starting multi-schedule research mode"
            );

            let handle = builder
                .node(EthereumNode::default())
                .install_exex("research", move |ctx| {
                    let registry = registry.clone();
                    let db_path = db_path.clone();
                    async move { Ok(research_exex(ctx, registry, db_path, start_block)) }
                })
                .launch()
                .await?;

            handle.wait_for_node_exit().await
        })
}
