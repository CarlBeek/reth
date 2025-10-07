//! Research executor that performs dual execution and divergence detection.

use crate::{
    config::ResearchConfig,
    database::DivergenceDatabase,
    divergence::{Divergence, DivergenceType, GasAnalysis},
    inspector::GasResearchInspector,
    metrics,
};
use alloy_consensus::TxReceipt;
use alloy_primitives::B256;
use reth_evm::execute::Executor;
use reth_execution_types::BlockExecutionResult;
use reth_primitives_traits::{AlloyBlockHeader, BlockBody, NodePrimitives, RecoveredBlock, SignedTransaction};
use thiserror::Error;
use tracing::{debug, info, warn};

/// Errors that can occur in the research executor.
#[derive(Debug, Error)]
pub enum ResearchError<E = std::convert::Infallible> {
    #[error("Execution error: {0}")]
    Execution(E),

    #[error("Database error: {0}")]
    Database(#[from] crate::database::DatabaseError),

    #[error("Configuration error: {0}")]
    Config(#[from] crate::config::ConfigError),

    #[error("Research mode not enabled for this block: {0}")]
    NotEnabled(u64),
}

/// Executor wrapper that performs execution analysis and divergence detection.
///
/// This executor wraps an existing executor and uses an inspector to simulate
/// high gas costs during normal execution, then detects divergences.
pub struct ResearchExecutor<E> {
    /// The underlying executor
    inner: E,

    /// Research configuration
    config: ResearchConfig,

    /// Divergence database
    divergence_db: Option<DivergenceDatabase>,

    /// Statistics
    blocks_processed: u64,
    divergences_found: u64,
}

impl<E> ResearchExecutor<E> {
    /// Create a new research executor.
    pub fn new(
        inner: E,
        config: ResearchConfig,
        divergence_db: Option<DivergenceDatabase>,
    ) -> Result<Self, ResearchError> {
        config.validate()?;

        if let Some(ref db) = divergence_db {
            info!(
                target: "reth::research",
                path = ?config.divergence_db_path,
                "Research mode enabled with divergence database"
            );
        }

        // Register metrics
        metrics::register_metrics();

        Ok(Self {
            inner,
            config,
            divergence_db,
            blocks_processed: 0,
            divergences_found: 0,
        })
    }

    /// Get statistics.
    pub fn stats(&self) -> ResearchStats {
        ResearchStats {
            blocks_processed: self.blocks_processed,
            divergences_found: self.divergences_found,
        }
    }

    /// Check if research mode is enabled for the given block.
    fn is_enabled_for_block(&self, block_number: u64) -> bool {
        block_number >= self.config.start_block
    }

    /// Analyze execution results and detect divergences using the inspector data.
    ///
    /// This method simulates what would have happened with modified gas costs
    /// by examining the inspector's findings during normal execution.
    fn analyze_execution<N: NodePrimitives>(
        &mut self,
        block: &RecoveredBlock<N::Block>,
        result: &BlockExecutionResult<N::Receipt>,
        inspector: &GasResearchInspector,
    ) -> Vec<Divergence>
    where
        N::Receipt: TxReceipt,
        N::SignedTx: SignedTransaction,
    {
        let start = std::time::Instant::now();
        let mut divergences = Vec::new();

        let tx_count = block.body().transactions().len();

        for tx_idx in 0..tx_count {
            let mut divergence_types = Vec::new();

            // Check if OOG occurred in simulation
            if inspector.oog_occurred() {
                divergence_types.push(DivergenceType::Status);

                // If experimental ran out of gas but normal didn't, that's a status divergence
                if let Some(receipt) = result.receipts.get(tx_idx) {
                    if receipt.status() {
                        // Normal succeeded but experimental would have failed
                        divergence_types.push(DivergenceType::ExecutionTrace);
                    }
                }
            }

            // Check if we have gas loop patterns (potential divergence source)
            if inspector.has_gas_loop_pattern() {
                divergence_types.push(DivergenceType::GasPattern);
            }

            // Get gas metrics
            let normal_gas = result.receipts
                .get(tx_idx)
                .map(|r| r.cumulative_gas_used())
                .unwrap_or(0);

            let simulated_gas = inspector.simulated_gas_used();

            let gas_ratio = GasAnalysis::calculate_ratio(
                normal_gas,
                simulated_gas,
                self.config.gas_multiplier,
            );

            let gas_analysis = GasAnalysis {
                normal_gas_used: normal_gas,
                experimental_gas_used: simulated_gas,
                gas_efficiency_ratio: gas_ratio,
            };

            // If gas pattern is structurally different, that indicates divergence
            if gas_analysis.is_structural_divergence() {
                divergence_types.push(DivergenceType::GasPattern);
            }

            // Get operation counts from inspector
            let ops = inspector.operation_counts().clone();

            // If any divergences detected, record it
            if !divergence_types.is_empty() || inspector.oog_occurred() {
                // For a RecoveredBlock, transactions should already be recovered
                // We'll just compute the hash from the transaction itself
                let tx_hash = block.body().transactions()
                    .get(tx_idx)
                    .map(|tx| *tx.tx_hash())
                    .unwrap_or(B256::ZERO);

                let divergence = Divergence {
                    block_number: block.number(),
                    tx_index: tx_idx as u64,
                    tx_hash,
                    timestamp: block.timestamp(),
                    divergence_types: divergence_types.clone(),
                    gas_analysis,
                    normal_ops: ops.clone(),
                    experimental_ops: ops.clone(), // In simulation, ops are same but gas differs
                    divergence_location: inspector.divergence_location().cloned(),
                    oog_info: inspector.oog_info().cloned(),
                    call_trees: None, // TODO: Extract from inspector
                    event_logs: None, // TODO: Extract from receipts
                };

                // Record to database if available
                if let Some(ref db) = self.divergence_db {
                    match db.record_divergence(&divergence) {
                        Ok(id) => {
                            debug!(
                                target: "reth::research",
                                divergence_id = id,
                                tx_hash = ?divergence.tx_hash,
                                types = ?divergence.divergence_types,
                                "Recorded divergence"
                            );
                        }
                        Err(e) => {
                            warn!(
                                target: "reth::research",
                                error = %e,
                                "Failed to record divergence"
                            );
                        }
                    }
                }

                // Record metrics
                metrics::record_divergence(&divergence_types, gas_ratio);
                if inspector.oog_occurred() {
                    if let Some(ref oog) = inspector.oog_info() {
                        metrics::record_oog(oog.pattern);
                    }
                }

                self.divergences_found += 1;
                divergences.push(divergence);
            }

            // Check max divergences limit
            if let Some(max) = self.config.max_divergences_per_block {
                if divergences.len() >= max {
                    debug!(
                        target: "reth::research",
                        block = block.number(),
                        "Reached max divergences per block limit: {}",
                        max
                    );
                    break;
                }
            }
        }

        let detection_time = start.elapsed().as_secs_f64();
        metrics::record_divergence_detection_time(detection_time);

        divergences
    }
}

impl<E, DB> Executor<DB> for ResearchExecutor<E>
where
    E: Executor<DB>,
    DB: reth_evm::Database,
{
    type Primitives = E::Primitives;
    type Error = ResearchError<E::Error>;

    fn execute_one(
        &mut self,
        block: &RecoveredBlock<<Self::Primitives as NodePrimitives>::Block>,
    ) -> Result<BlockExecutionResult<<Self::Primitives as NodePrimitives>::Receipt>, Self::Error> {
        let block_number = block.number();

        // If research mode is not enabled for this block, execute normally
        if !self.is_enabled_for_block(block_number) {
            return self.inner.execute_one(block).map_err(ResearchError::Execution);
        }

        debug!(
            target: "reth::research",
            block = block_number,
            tx_count = block.body().transactions().len(),
            "Executing block in research mode"
        );

        let start = std::time::Instant::now();

        // TODO: For now, we execute normally without inspector
        // In a complete implementation, we would:
        // 1. Create a GasResearchInspector
        // 2. Execute with inspector attached using execute_one_with_state_hook
        // 3. Analyze the inspector data
        //
        // However, this requires access to the executor's internal state/EVM
        // which we don't have through the Executor trait.

        let result = self.inner.execute_one(block).map_err(ResearchError::Execution)?;

        // TODO: Create inspector and analyze
        // For now, just log that we processed the block
        info!(
            target: "reth::research",
            block = block_number,
            tx_count = block.body().transactions().len(),
            gas_used = result.gas_used,
            "Block executed in research mode (analysis not yet implemented)"
        );

        self.blocks_processed += 1;

        let duration = start.elapsed().as_secs_f64();
        metrics::record_block_processed(block_number, block.body().transactions().len(), duration);

        Ok(result)
    }

    fn execute_one_with_state_hook<F>(
        &mut self,
        block: &RecoveredBlock<<Self::Primitives as NodePrimitives>::Block>,
        state_hook: F,
    ) -> Result<BlockExecutionResult<<Self::Primitives as NodePrimitives>::Receipt>, Self::Error>
    where
        F: reth_evm::OnStateHook + 'static,
    {
        // For now, just execute normally
        // In a complete implementation, we would wrap the state_hook with our inspector
        self.inner
            .execute_one_with_state_hook(block, state_hook)
            .map_err(ResearchError::Execution)
    }

    fn into_state(self) -> revm::database::State<DB> {
        self.inner.into_state()
    }

    fn size_hint(&self) -> usize {
        self.inner.size_hint()
    }
}

/// Statistics from research execution.
#[derive(Debug, Clone, Copy)]
pub struct ResearchStats {
    pub blocks_processed: u64,
    pub divergences_found: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    // TODO: Add tests with mock executor
}
