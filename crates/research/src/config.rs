//! Configuration types for research mode.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Configuration for research mode execution.
#[derive(Debug, Clone)]
pub struct ResearchConfig {
    /// Multiply all gas costs by this factor (default: 128)
    pub gas_multiplier: u64,

    /// Block number to start research mode (default: 0)
    pub start_block: u64,

    /// Multiply gas refunds by this factor (default: 1.0)
    pub refund_multiplier: f64,

    /// Multiply the 2300 gas stipend by this factor (default: 1.0)
    /// Set to 1.0 to keep it unchanged (this is the interesting case)
    pub stipend_multiplier: f64,

    /// Path to the divergence database
    pub divergence_db_path: PathBuf,

    /// Path to the gas loop detection database
    pub loop_detection_db_path: Option<PathBuf>,

    /// Level of detail for divergence traces
    pub trace_detail: TraceDetail,

    /// Inflate transaction gas limits by this factor (default: same as gas_multiplier)
    /// This prevents trivial OOG failures
    pub gas_limit_multiplier: Option<u64>,

    /// Maximum number of divergences to record per block (default: unlimited)
    pub max_divergences_per_block: Option<usize>,

    /// Enable gas-dependent loop detection
    pub detect_gas_loops: bool,
}

impl Default for ResearchConfig {
    fn default() -> Self {
        Self {
            gas_multiplier: 128,
            start_block: 0,
            refund_multiplier: 1.0,
            stipend_multiplier: 1.0,
            divergence_db_path: PathBuf::from("divergence.db"),
            loop_detection_db_path: None,
            trace_detail: TraceDetail::Standard,
            gas_limit_multiplier: None,
            max_divergences_per_block: None,
            detect_gas_loops: true,
        }
    }
}

impl ResearchConfig {
    /// Get the effective gas limit multiplier.
    /// Defaults to the gas multiplier if not explicitly set.
    pub fn effective_gas_limit_multiplier(&self) -> u64 {
        self.gas_limit_multiplier.unwrap_or(self.gas_multiplier)
    }

    /// Validate the configuration.
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.gas_multiplier == 0 {
            return Err(ConfigError::InvalidMultiplier("gas_multiplier must be > 0"));
        }

        if self.refund_multiplier < 0.0 {
            return Err(ConfigError::InvalidMultiplier("refund_multiplier must be >= 0"));
        }

        if self.stipend_multiplier < 0.0 {
            return Err(ConfigError::InvalidMultiplier("stipend_multiplier must be >= 0"));
        }

        Ok(())
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
    #[error("Invalid multiplier: {0}")]
    InvalidMultiplier(&'static str),

    #[error("Invalid path: {0}")]
    InvalidPath(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_config() {
        let config = ResearchConfig::default();
        assert_eq!(config.gas_multiplier, 128);
        assert_eq!(config.effective_gas_limit_multiplier(), 128);
        assert!(config.validate().is_ok());
    }

    #[test]
    fn test_custom_gas_limit_multiplier() {
        let config = ResearchConfig {
            gas_multiplier: 128,
            gas_limit_multiplier: Some(256),
            ..Default::default()
        };
        assert_eq!(config.effective_gas_limit_multiplier(), 256);
    }

    #[test]
    fn test_invalid_multiplier() {
        let config = ResearchConfig { gas_multiplier: 0, ..Default::default() };
        assert!(config.validate().is_err());
    }

    #[test]
    fn test_trace_detail_parsing() {
        assert_eq!("minimal".parse::<TraceDetail>().unwrap(), TraceDetail::Minimal);
        assert_eq!("standard".parse::<TraceDetail>().unwrap(), TraceDetail::Standard);
        assert_eq!("detailed".parse::<TraceDetail>().unwrap(), TraceDetail::Detailed);
        assert!("invalid".parse::<TraceDetail>().is_err());
    }
}
