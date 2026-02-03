//! Configuration types for research mode.

use crate::gas_pricing::{GasPricingError, GasPricingTable};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Configuration for research mode execution.
#[derive(Debug, Clone)]
pub struct ResearchConfig {
    /// Gas pricing table loaded from CSV
    pub gas_pricing: GasPricingTable,

    /// Block number to start research mode (default: 0)
    pub start_block: u64,

    /// Path to the divergence database
    pub divergence_db_path: PathBuf,

    /// Path to the gas loop detection database
    pub loop_detection_db_path: Option<PathBuf>,

    /// Level of detail for divergence traces
    pub trace_detail: TraceDetail,

    /// Maximum number of divergences to record per block (default: unlimited)
    pub max_divergences_per_block: Option<usize>,

    /// Enable gas-dependent loop detection
    pub detect_gas_loops: bool,

    /// Maximum number of transactions to analyze in parallel per block
    /// Set to 1 for sequential processing, or higher to utilize multiple cores
    /// Recommended: number of physical cores for CPU-bound workloads
    pub max_parallel_txs: usize,
}

impl Default for ResearchConfig {
    fn default() -> Self {
        Self {
            gas_pricing: GasPricingTable::new(),
            start_block: 0,
            divergence_db_path: PathBuf::from("divergence.db"),
            loop_detection_db_path: None,
            trace_detail: TraceDetail::Standard,
            max_divergences_per_block: None,
            detect_gas_loops: true,
            max_parallel_txs: num_cpus::get(),
        }
    }
}

impl ResearchConfig {
    /// Create a new config with gas pricing loaded from a CSV file.
    pub fn with_csv_pricing(csv_path: &std::path::Path) -> Result<Self, GasPricingError> {
        let gas_pricing = GasPricingTable::from_csv_path(csv_path)?;
        Ok(Self { gas_pricing, ..Default::default() })
    }

    /// Validate the configuration.
    pub fn validate(&self) -> Result<(), ConfigError> {
        // CSV-based pricing is always valid (empty table means no repricing)
        Ok(())
    }

    /// Check if any opcodes are repriced.
    pub fn has_opcode_repricing(&self) -> bool {
        self.gas_pricing.opcode_count() > 0
    }

    /// Check if any precompiles are repriced.
    pub fn has_precompile_repricing(&self) -> bool {
        self.gas_pricing.precompile_count() > 0
    }

    /// Get summary of loaded pricing.
    pub fn pricing_summary(&self) -> String {
        format!(
            "{} opcodes, {} precompiles repriced",
            self.gas_pricing.opcode_count(),
            self.gas_pricing.precompile_count()
        )
    }
}

/// Level of detail for divergence traces.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum TraceDetail {
    /// Minimal: Only record divergence type and gas metrics
    Minimal,

    /// Standard: Include operation counts and divergence location
    Standard,

    /// Detailed: Include full call trees and event logs
    Detailed,
}

impl TraceDetail {
    /// Check if call trees should be included.
    pub const fn include_call_trees(self) -> bool {
        matches!(self, TraceDetail::Detailed)
    }

    /// Check if event logs should be included.
    pub const fn include_event_logs(self) -> bool {
        matches!(self, TraceDetail::Detailed)
    }
}

impl std::str::FromStr for TraceDetail {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "minimal" => Ok(TraceDetail::Minimal),
            "standard" => Ok(TraceDetail::Standard),
            "detailed" => Ok(TraceDetail::Detailed),
            _ => Err(format!("Invalid trace detail level: {}", s)),
        }
    }
}

/// Configuration errors.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    /// Invalid file path
    #[error("Invalid path: {0}")]
    InvalidPath(String),

    /// Gas pricing error
    #[error("Gas pricing error: {0}")]
    GasPricing(#[from] GasPricingError),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_config() {
        let config = ResearchConfig::default();
        assert_eq!(config.gas_pricing.opcode_count(), 0);
        assert_eq!(config.gas_pricing.precompile_count(), 0);
        assert!(config.validate().is_ok());
    }

    #[test]
    fn test_config_with_pricing() {
        let csv_data = r#"Opcode,Parameter,Current Gas,New Gas
DIV,constant,5,15
ECPAIRING,constant,45000,45000
"#;
        let table = GasPricingTable::from_csv(csv_data.as_bytes()).unwrap();
        let config = ResearchConfig { gas_pricing: table, ..Default::default() };

        assert!(config.has_opcode_repricing());
        assert!(config.has_precompile_repricing());
        assert!(config.validate().is_ok());
    }

    #[test]
    fn test_trace_detail_parsing() {
        assert_eq!("minimal".parse::<TraceDetail>().unwrap(), TraceDetail::Minimal);
        assert_eq!("standard".parse::<TraceDetail>().unwrap(), TraceDetail::Standard);
        assert_eq!("detailed".parse::<TraceDetail>().unwrap(), TraceDetail::Detailed);
        assert!("invalid".parse::<TraceDetail>().is_err());
    }

    #[test]
    fn test_pricing_summary() {
        let config = ResearchConfig::default();
        assert_eq!(config.pricing_summary(), "0 opcodes, 0 precompiles repriced");
    }
}
