//! Research Mode Execution Extension
//!
//! This ExEx performs dual execution analysis to detect divergences caused by modified gas costs.
//! Each committed block's transactions are executed twice:
//! 1. Normal execution (standard gas costs)
//! 2. Experimental execution (multiplied gas costs)
//!
//! Divergences in execution results (status, gas, state, logs) are recorded to a database.
//!
//! Run with:
//!
//! ```sh
//! cargo run --release -p reth-research node --dev --dev.block-time 5s \
//!   --research.gas-multiplier 128 \
//!   --research.db-path ./divergences.db
//! ```

use alloy_consensus::{transaction::TxHashRef, BlockHeader};
use futures::TryStreamExt;
use reth_ethereum::{
    exex::{ExExContext, ExExEvent, ExExNotification},
    node::EthereumNode,
};
use reth_evm::{ConfigureEvm, Evm};
use reth_node_api::{BlockTy, FullNodeComponents};
use reth_primitives_traits::BlockBody;
use reth_provider::StateProviderFactory;
use reth_research::{
    config::{ResearchConfig, TraceDetail},
    database::DivergenceDatabase,
    divergence::{CallTrees, Divergence, DivergenceType, EventLog, EventLogs, GasAnalysis},
    inspector::GasResearchInspector,
    metrics,
    tracking_inspector::TrackingInspector,
};
use reth_revm::{database::StateProviderDatabase, db::CacheDB};
use reth_tracing::tracing::{debug, info, warn};


/// Research ExEx that performs dual execution analysis on committed blocks.
struct ResearchExEx<Node: FullNodeComponents> {
    /// ExEx context
    ctx: ExExContext<Node>,
    /// Research configuration
    config: ResearchConfig,
    /// Divergence database
    divergence_db: Option<DivergenceDatabase>,
    /// Statistics
    blocks_processed: u64,
    divergences_found: u64,
}

impl<Node: FullNodeComponents> ResearchExEx<Node> {
    /// Create a new research ExEx.
    fn new(
        ctx: ExExContext<Node>,
        config: ResearchConfig,
    ) -> eyre::Result<Self> {
        config.validate()?;

        // Initialize database
        let divergence_db = if config.divergence_db_path.to_str() != Some(":memory:") {
            Some(DivergenceDatabase::open(&config.divergence_db_path)?)
        } else {
            None
        };

        if let Some(ref _db) = divergence_db {
            info!(
                target: "exex::research",
                path = ?config.divergence_db_path,
                "Research ExEx initialized with divergence database"
            );
        }

        // Register metrics
        metrics::register_metrics();

        Ok(Self { ctx, config, divergence_db, blocks_processed: 0, divergences_found: 0 })
    }

    /// Run the ExEx.
    async fn run(mut self) -> eyre::Result<()> {
        info!(
            target: "exex::research",
            start_block = self.config.start_block,
            gas_multiplier = self.config.gas_multiplier,
            "Research ExEx started"
        );

        while let Some(notification) = self.ctx.notifications.try_next().await? {
            match &notification {
                ExExNotification::ChainCommitted { new } => {
                    // Process each committed block
                    for (_block_number, block) in new.blocks() {
                        let block_number = block.number();

                        // Check if research mode is enabled for this block
                        if block_number < self.config.start_block {
                            continue;
                        }

                        debug!(
                            target: "exex::research",
                            block = block_number,
                            tx_count = block.body().transactions().len(),
                            "Analyzing block in research mode"
                        );

                        // Analyze the block
                        if let Err(e) = self.analyze_block(block).await {
                            warn!(
                                target: "exex::research",
                                block = block_number,
                                error = %e,
                                "Failed to analyze block"
                            );
                            // Continue processing other blocks
                        }

                        self.blocks_processed += 1;
                    }

                    // Send event to acknowledge processing
                    self.ctx.events.send(ExExEvent::FinishedHeight(new.tip().num_hash()))?;
                }
                ExExNotification::ChainReorged { old: _, new } => {
                    // On reorg, process the new chain
                    info!(
                        target: "exex::research",
                        "Chain reorg detected, processing new chain"
                    );

                    for (_block_number, block) in new.blocks() {
                        let block_number = block.number();
                        if block_number < self.config.start_block {
                            continue;
                        }

                        if let Err(e) = self.analyze_block(block).await {
                            warn!(
                                target: "exex::research",
                                block = block_number,
                                error = %e,
                                "Failed to analyze block after reorg"
                            );
                        }
                    }

                    self.ctx.events.send(ExExEvent::FinishedHeight(new.tip().num_hash()))?;
                }
                ExExNotification::ChainReverted { old } => {
                    info!(
                        target: "exex::research",
                        reverted_tip = old.tip().number(),
                        "Chain reverted"
                    );
                    // TODO: Remove divergences for reverted blocks from database
                }
            }
        }

        Ok(())
    }

    /// Analyze a single block using dual execution.
    async fn analyze_block(
        &mut self,
        block: &reth_primitives_traits::RecoveredBlock<BlockTy<Node::Types>>,
    ) -> eyre::Result<()>
    where
        Node::Evm: ConfigureEvm,
    {
        let block_number = block.number();
        let block_start = std::time::Instant::now();

        // Get provider for accessing historical state
        let provider = self.ctx.provider();

        // Build EVM environment for the block
        let evm_env = match self.ctx.evm_config().evm_env(block.header()) {
            Ok(env) => env,
            Err(e) => {
                warn!(
                    target: "exex::research",
                    block = block_number,
                    error = ?e,
                    "Failed to build EVM environment"
                );
                return Ok(());
            }
        };

        // Process each transaction with dual execution (use recovered transactions)
        for (tx_idx, tx) in block.transactions_recovered().enumerate() {
            let tx_env = self.ctx.evm_config().tx_env(tx);

            // Get state for normal execution
            let normal_state = if block_number > 0 {
                provider.history_by_block_number(block_number - 1)?
            } else {
                provider.latest()?
            };

            // --- EXECUTION 1: Normal (with tracking inspector) ---
            let normal_db = StateProviderDatabase(normal_state);
            let mut normal_cache = CacheDB::new(normal_db);
            let mut normal_inspector = TrackingInspector::new();
            let mut normal_evm = self.ctx.evm_config().evm_with_env_and_inspector(
                &mut normal_cache,
                evm_env.clone(),
                &mut normal_inspector,
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

            // Drop normal EVM to release the inspector
            drop(normal_evm);

            // Get fresh state for experimental execution
            let experimental_state = if block_number > 0 {
                provider.history_by_block_number(block_number - 1)?
            } else {
                provider.latest()?
            };

            // --- EXECUTION 2: Experimental (with gas multiplier inspector) ---
            let experimental_db = StateProviderDatabase(experimental_state);
            let mut experimental_cache = CacheDB::new(experimental_db);
            let mut experimental_inspector =
                GasResearchInspector::new(self.config.clone(), block.header().gas_limit());

            let mut experimental_evm = self.ctx.evm_config().evm_with_env_and_inspector(
                &mut experimental_cache,
                evm_env.clone(),
                &mut experimental_inspector,
            );

            let experimental_result = match experimental_evm.transact(tx_env.clone()) {
                Ok(result) => result,
                Err(e) => {
                    // Experimental execution failed - this is a critical divergence!
                    // Normal execution succeeded but experimental failed.

                    // Drop EVM to release the inspector
                    drop(experimental_evm);

                    warn!(
                        target: "exex::research",
                        block = block_number,
                        tx_idx,
                        error = ?e,
                        "CRITICAL DIVERGENCE: Experimental execution failed while normal succeeded"
                    );

                    // Record as a critical divergence - experimental failure
                    let call_trees = if matches!(self.config.trace_detail, TraceDetail::Detailed) {
                        Some(CallTrees {
                            normal: normal_inspector.call_frames().to_vec(),
                            experimental: experimental_inspector.call_frames().to_vec(),
                        })
                    } else {
                        None
                    };

                    let event_logs = if matches!(self.config.trace_detail, TraceDetail::Detailed) {
                        Some(EventLogs {
                            normal: normal_inspector
                                .event_logs()
                                .iter()
                                .map(|e| EventLog {
                                    log_index: e.log_index,
                                    address: e.address,
                                    topics: e.topics.clone(),
                                    data: e.data.clone(),
                                })
                                .collect(),
                            experimental: vec![],
                        })
                    } else {
                        None
                    };

                    let divergence = Divergence {
                        block_number,
                        tx_index: tx_idx as u64,
                        tx_hash: *tx.tx_hash(),
                        timestamp: block.timestamp(),
                        divergence_types: vec![DivergenceType::Status],
                        gas_analysis: GasAnalysis {
                            normal_gas_used: normal_result.result.gas_used(),
                            experimental_gas_used: 0, // Failed before completion
                            gas_efficiency_ratio: 0.0,
                        },
                        normal_ops: normal_inspector.operation_counts().clone(),
                        experimental_ops: experimental_inspector.operation_counts().clone(),
                        divergence_location: experimental_inspector.divergence_location().cloned(),
                        oog_info: experimental_inspector.oog_info().cloned(),
                        call_trees,
                        event_logs,
                    };

                    self.record_divergence(&divergence);
                    self.divergences_found += 1;

                    // Check max divergences limit
                    if let Some(max) = self.config.max_divergences_per_block {
                        if self.divergences_found >= max as u64 {
                            debug!(
                                target: "exex::research",
                                block = block_number,
                                "Reached max divergences per block limit: {}",
                                max
                            );
                            break;
                        }
                    }

                    continue;
                }
            };

            // Drop the EVM to release the mutable borrow on the inspector
            drop(experimental_evm);

            // --- COMPARE RESULTS ---
            let mut divergence_types = Vec::new();

            // 1. Compare execution status
            let normal_success = normal_result.result.is_success();
            let experimental_success = experimental_result.result.is_success();

            if normal_success != experimental_success {
                divergence_types.push(DivergenceType::Status);
                info!(
                    target: "exex::research",
                    block = block_number,
                    tx_idx,
                    normal_success,
                    experimental_success,
                    "DIVERGENCE: Status differs"
                );
            }

            // 2. Compare gas usage
            let normal_gas = normal_result.result.gas_used();
            let experimental_gas = experimental_result.result.gas_used();
            let gas_ratio = GasAnalysis::calculate_ratio(
                normal_gas,
                experimental_gas,
                self.config.gas_multiplier,
            );
            let gas_analysis = GasAnalysis {
                normal_gas_used: normal_gas,
                experimental_gas_used: experimental_gas,
                gas_efficiency_ratio: gas_ratio,
            };

            if gas_analysis.is_structural_divergence() {
                divergence_types.push(DivergenceType::GasPattern);
                info!(
                    target: "exex::research",
                    block = block_number,
                    tx_idx,
                    normal_gas,
                    experimental_gas,
                    gas_ratio,
                    "DIVERGENCE: Gas pattern differs structurally"
                );
            }

            // 3. Compare state changes
            let normal_state_len = normal_result.state.len();
            let experimental_state_len = experimental_result.state.len();

            if normal_state_len != experimental_state_len {
                divergence_types.push(DivergenceType::StateRoot);
            } else {
                // Compare individual account states
                for (address, normal_account) in &normal_result.state {
                    if let Some(experimental_account) = experimental_result.state.get(address) {
                        if normal_account.storage != experimental_account.storage ||
                            normal_account.info != experimental_account.info
                        {
                            divergence_types.push(DivergenceType::StateRoot);
                            break;
                        }
                    } else {
                        divergence_types.push(DivergenceType::StateRoot);
                        break;
                    }
                }
            }

            // 4. Compare logs
            let normal_logs = normal_result.result.logs();
            let experimental_logs = experimental_result.result.logs();

            if normal_logs.len() != experimental_logs.len() {
                divergence_types.push(DivergenceType::EventLogs);
            } else {
                for (normal_log, experimental_log) in
                    normal_logs.iter().zip(experimental_logs.iter())
                {
                    if normal_log.address != experimental_log.address ||
                        normal_log.data.topics() != experimental_log.data.topics() ||
                        normal_log.data.data != experimental_log.data.data
                    {
                        divergence_types.push(DivergenceType::EventLogs);
                        break;
                    }
                }
            }

            // If divergences detected or OOG occurred, record it
            if !divergence_types.is_empty() || experimental_inspector.oog_occurred() {
                // Extract call trees and event logs if detailed tracing is enabled
                let call_trees = if matches!(self.config.trace_detail, TraceDetail::Detailed) {
                    Some(CallTrees {
                        normal: normal_inspector.call_frames().to_vec(),
                        experimental: experimental_inspector.call_frames().to_vec(),
                    })
                } else {
                    None
                };

                let event_logs = if matches!(self.config.trace_detail, TraceDetail::Detailed) {
                    Some(EventLogs {
                        normal: normal_inspector
                            .event_logs()
                            .iter()
                            .map(|e| EventLog {
                                log_index: e.log_index,
                                address: e.address,
                                topics: e.topics.clone(),
                                data: e.data.clone(),
                            })
                            .collect(),
                        experimental: vec![], // GasResearchInspector doesn't track logs yet
                    })
                } else {
                    None
                };

                let divergence = Divergence {
                    block_number,
                    tx_index: tx_idx as u64,
                    tx_hash: *tx.tx_hash(),
                    timestamp: block.timestamp(),
                    divergence_types,
                    gas_analysis,
                    normal_ops: normal_inspector.operation_counts().clone(),
                    experimental_ops: experimental_inspector.operation_counts().clone(),
                    divergence_location: experimental_inspector.divergence_location().cloned(),
                    oog_info: experimental_inspector.oog_info().cloned(),
                    call_trees,
                    event_logs,
                };

                self.record_divergence(&divergence);
                self.divergences_found += 1;

                // Check max divergences limit
                if let Some(max) = self.config.max_divergences_per_block {
                    if self.divergences_found >= max as u64 {
                        debug!(
                            target: "exex::research",
                            block = block_number,
                            "Reached max divergences per block limit: {}",
                            max
                        );
                        break;
                    }
                }
            }
        }

        let block_duration = block_start.elapsed().as_secs_f64();
        let tx_count = block.body().transactions().len();
        metrics::record_block_processed(block_number, tx_count, block_duration);

        info!(
            target: "exex::research",
            block = block_number,
            tx_count,
            duration_ms = block_duration * 1000.0,
            "Block analyzed in research mode"
        );

        Ok(())
    }

    /// Record a divergence to database and metrics.
    fn record_divergence(&self, divergence: &Divergence) {
        // Record metrics
        metrics::record_divergence(
            &divergence.divergence_types,
            divergence.gas_analysis.gas_efficiency_ratio,
        );
        if let Some(ref oog) = divergence.oog_info {
            metrics::record_oog(oog.pattern);
        }

        // Record to database if available
        if let Some(ref db) = self.divergence_db {
            match db.record_divergence(divergence) {
                Ok(id) => {
                    info!(
                        target: "exex::research",
                        block = divergence.block_number,
                        tx_idx = divergence.tx_index,
                        tx_hash = ?divergence.tx_hash,
                        types = ?divergence.divergence_types,
                        divergence_id = id,
                        "Divergence recorded to database"
                    );
                }
                Err(e) => {
                    warn!(
                        target: "exex::research",
                        block = divergence.block_number,
                        tx_idx = divergence.tx_index,
                        error = %e,
                        "Failed to record divergence to database"
                    );
                }
            }
        } else {
            info!(
                target: "exex::research",
                block = divergence.block_number,
                tx_idx = divergence.tx_index,
                tx_hash = ?divergence.tx_hash,
                types = ?divergence.divergence_types,
                "Divergence detected (no database configured)"
            );
        }
    }
}

async fn research_exex<Node: FullNodeComponents>(
    ctx: ExExContext<Node>,
    config: ResearchConfig,
) -> eyre::Result<()> {
    ResearchExEx::new(ctx, config)?.run().await
}

fn main() -> eyre::Result<()> {
    reth_ethereum::cli::Cli::parse_args().run(|builder, _ext| {
        // Extract research config from node config
        let node_config = builder.config();
        let research_args = &node_config.research;

        // Build research config from the built-in research args
        let config = ResearchConfig {
            gas_multiplier: research_args.gas_multiplier,
            divergence_db_path: research_args.db_path.clone(),
            start_block: research_args.start_block,
            max_divergences_per_block: None,
            trace_detail: TraceDetail::Standard,
            refund_multiplier: research_args.refund_multiplier,
            stipend_multiplier: research_args.stipend_multiplier,
            loop_detection_db_path: None,
            gas_limit_multiplier: None,
            detect_gas_loops: false,
        };

        Box::pin(async move {
            let handle = builder
                .node(EthereumNode::default())
                .install_exex("research", |ctx| async move {
                    Ok(research_exex(ctx, config.clone()))
                })
                .launch()
                .await?;

            handle.wait_for_node_exit().await
        })
    })
}
