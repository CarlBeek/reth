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
use reth_evm::{ConfigureEvm, Evm};
use reth_node_api::{BlockTy, FullNodeComponents};
use reth_node_core::args::ResearchArgs;
use reth_primitives_traits::BlockBody;
use reth_provider::StateProviderFactory;
use reth_research::{
    database::DivergenceDatabase,
    divergence::DivergenceType,
    schedule::{RecipientInfo, ScheduleKind, ScheduleRegistry, TxContext},
    MultiScheduleInspector, ScheduleDivergence,
};
use reth_revm::{database::StateProviderDatabase, db::State};
use reth_revm::{Database, DatabaseCommit};
use reth_tracing::tracing::{debug, info, warn};
use std::{collections::HashMap, sync::Arc};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

enum DbCommand {
    Record(ScheduleDivergence),
    DeleteRange { from_block: u64, to_block: u64 },
}

/// Research ExEx that performs multi-schedule execution analysis on committed blocks.
struct ResearchExEx<Node: FullNodeComponents> {
    /// ExEx context
    ctx: ExExContext<Node>,
    /// Schedule registry containing all configured experiments
    registry: Arc<ScheduleRegistry>,
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
    fn baseline_intrinsic_gas(tx_context: &TxContext) -> u64 {
        let base = if tx_context.is_create { 53_000 } else { 21_000 };
        base + tx_context.calldata_gas()
    }

    /// Create a new research ExEx.
    fn new(
        ctx: ExExContext<Node>,
        registry: ScheduleRegistry,
        db_path: std::path::PathBuf,
        start_block: u64,
    ) -> eyre::Result<Self> {
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
                let mut write_count = 0u64;
                while let Some(cmd) = rx.recv().await {
                    match cmd {
                        DbCommand::Record(div) => {
                            match divergence_db.record_schedule_divergence(&div) {
                                Ok(_id) => {
                                    write_count += 1;
                                    if write_count % 100 == 0 {
                                        debug!(
                                            target: "exex::research::db_writer",
                                            total_writes = write_count,
                                            "Database writer progress"
                                        );
                                    }
                                }
                                Err(e) => {
                                    warn!(
                                        target: "exex::research::db_writer",
                                        block = div.block_number,
                                        tx_idx = div.tx_index,
                                        schedule = div.schedule_name,
                                        error = %e,
                                        "Failed to record divergence to database"
                                    );
                                }
                            }
                        }
                        DbCommand::DeleteRange { from_block, to_block } => {
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
            registry: Arc::new(registry),
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
        let all_schedules = self.registry.all();
        let execution_schedules = self.registry.execution_schedules();
        let mut normal_db =
            State::builder().with_database(StateProviderDatabase::new(baseline_state)).build();
        let mut inspected_db = if execution_schedules.is_empty() {
            None
        } else {
            let inspected_state = if block_number > 0 {
                match provider.history_by_block_number(block_number - 1) {
                    Ok(state) => state,
                    Err(err) => {
                        debug!(
                            target: "exex::research",
                            block = block_number,
                            %err,
                            "Historical state not yet available for inspected pass, skipping block \
                             (expected during initial pipeline sync)"
                        );
                        return Ok(false);
                    }
                }
            } else {
                provider.latest()?
            };
            Some(
                State::builder().with_database(StateProviderDatabase::new(inspected_state)).build(),
            )
        };
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

        for (tx_idx, tx) in block.transactions_recovered().enumerate() {
            let tx_env = self.ctx.evm_config().tx_env(tx);
            let sender = tx.signer();

            // Extract transaction fields using Transaction trait
            // Use kind() to get TxKind which tells us if it's create or call
            let tx_kind = tx.kind();
            let is_create = tx_kind.is_create();
            let recipient: Option<Address> = tx_kind.to().copied();
            let value: U256 = tx.value();
            let input: Bytes = tx.input().clone();
            let gas_limit: u64 = tx.gas_limit();
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

            let tx_context =
                TxContext { sender, recipient, value, input, gas_limit, is_create, recipient_info };

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

            // --- EXECUTION: Inspected (execution-modifying schedules) ---
            let (inspector_results, formatted_operation_counts) =
                if let Some(ref mut inspected_db) = inspected_db {
                    let mut inspector = MultiScheduleInspector::new(execution_schedules.clone());
                    let mut inspected_evm = self.ctx.evm_config().evm_with_env_and_inspector(
                        &mut *inspected_db,
                        evm_env.clone(),
                        &mut inspector,
                    );
                    let inspected_result = match inspected_evm.transact(tx_env.clone()) {
                        Ok(result) => result,
                        Err(e) => {
                            debug!(
                                target: "exex::research",
                                block = block_number,
                                tx_idx,
                                error = ?e,
                                "Inspected execution failed"
                            );
                            continue;
                        }
                    };
                    drop(inspected_evm);
                    inspected_db.commit(inspected_result.state);
                    (inspector.results(), Some(format!("{:?}", inspector.operation_counts())))
                } else {
                    (Vec::new(), None)
                };

            let normal_gas_used = normal_result.result.gas_used();
            let normal_success = normal_result.result.is_success();
            let tx_hash = *tx.tx_hash();
            let baseline_intrinsic_gas = Self::baseline_intrinsic_gas(&tx_context);
            let execution_metadata_by_name: HashMap<_, _> = inspector_results
                .iter()
                .map(|result| {
                    (
                        result.schedule_name.as_str(),
                        (
                            result.additional_gas,
                            result.would_oog,
                            result.oog_info.as_ref().map(|oog| format!("{oog:?}")),
                            result
                                .divergence_location
                                .as_ref()
                                .map(|location| format!("{location:?}")),
                        ),
                    )
                })
                .collect();
            // --- ANALYZE EACH SCHEDULE ---
            for schedule in &all_schedules {
                let schedule_name = schedule.name();
                let schedule_kind = schedule.kind();

                // Calculate gas deltas based on schedule kind.
                let (intrinsic_delta, schedule_intrinsic_gas, tx_category) = match schedule_kind {
                    ScheduleKind::IntrinsicOnly | ScheduleKind::Both => {
                        // For intrinsic-modifying schedules (like EIP-2780), calculate intrinsic
                        // gas. Baseline includes create/call base and calldata cost.
                        let schedule_intrinsic =
                            schedule.intrinsic_gas(&tx_context).unwrap_or(baseline_intrinsic_gas);
                        let delta = schedule_intrinsic as i64 - baseline_intrinsic_gas as i64;
                        let category = schedule.tx_category(&tx_context);
                        (delta, Some(schedule_intrinsic), category)
                    }
                    ScheduleKind::ExecutionOnly => {
                        // For execution-only schedules, no intrinsic change
                        (0i64, None, None)
                    }
                    ScheduleKind::None => continue,
                };

                let (execution_delta, execution_would_oog, oog_info, divergence_location) =
                    match schedule_kind {
                        ScheduleKind::ExecutionOnly | ScheduleKind::Both => {
                            if let Some((
                                additional_gas,
                                would_oog,
                                oog_info,
                                divergence_location,
                            )) = execution_metadata_by_name.get(schedule_name).cloned()
                            {
                                (additional_gas, would_oog, oog_info, divergence_location)
                            } else {
                                (0i64, false, None, None)
                            }
                        }
                        ScheduleKind::IntrinsicOnly | ScheduleKind::None => {
                            (0i64, false, None, None)
                        }
                    };

                let total_delta = intrinsic_delta + execution_delta;
                let (affected_opcodes, affected_precompiles) =
                    schedule_metadata.get(schedule_name).cloned().unwrap_or((None, None));

                // Determine if this would cause a divergence
                let schedule_gas = (normal_gas_used as i64 + total_delta).max(0) as u64;
                let would_oog = execution_would_oog || schedule_gas > gas_limit;
                let schedule_success = normal_success && !would_oog;

                // Record divergence if there's a gas delta or status change
                if total_delta != 0 || would_oog {
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
                        operation_counts: if matches!(
                            schedule_kind,
                            ScheduleKind::ExecutionOnly | ScheduleKind::Both
                        ) {
                            formatted_operation_counts.clone()
                        } else {
                            None
                        },
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
