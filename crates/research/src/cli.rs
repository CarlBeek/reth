//! CLI argument types for research mode.
//!
//! This module provides CLI argument parsing types that can be used
//! to configure multi-schedule research experiments.

use crate::schedule::{
    CsvPricingError, CsvPricingSchedule, Eip2780Schedule, Eip8037Schedule, Eip8038Schedule,
    GlamsterdamSchedule, MultiplierSchedule, ScheduleRegistry,
};
use std::path::PathBuf;
use thiserror::Error;

/// Errors that can occur when parsing CLI arguments.
#[derive(Debug, Error)]
pub enum CliError {
    /// Invalid CSV schedule format
    #[error("Invalid CSV schedule format: expected 'name=path', got '{0}'")]
    InvalidCsvFormat(String),

    /// Invalid multiplier schedule format
    #[error("Invalid multiplier format: expected 'name=multiplier', got '{0}'")]
    InvalidMultiplierFormat(String),

    /// Invalid multiplier value
    #[error("Invalid multiplier value '{0}': {1}")]
    InvalidMultiplierValue(String, String),

    /// CSV file not found
    #[error("CSV file not found: {0:?}")]
    CsvFileNotFound(PathBuf),

    /// Failed to load CSV schedule
    #[error("Failed to load CSV schedule '{name}': {source}")]
    CsvLoadError {
        /// Name of the schedule that failed to load
        name: String,
        /// The underlying CSV parsing error
        source: CsvPricingError,
    },

    /// No schedules configured
    #[error("No schedules configured. Enable at least one schedule with --research.eip2780, --research.eip8037, --research.eip8038, --research.glamsterdam, --research.csv, or --research.multiplier")]
    NoSchedules,

    /// Schedule registry error
    #[error("Schedule error: {0}")]
    ScheduleError(#[from] crate::schedule::ScheduleError),
}

/// A named CSV schedule parsed from CLI.
///
/// Format: `name=path`
/// Example: `--research.csv 7904-v1=./pricing.csv`
#[derive(Debug, Clone)]
pub struct NamedCsvSchedule {
    /// Unique name for this schedule
    pub name: String,
    /// Path to the CSV file
    pub path: PathBuf,
}

impl NamedCsvSchedule {
    /// Parse from a string in "name=path" format.
    pub fn parse(s: &str) -> Result<Self, CliError> {
        let (name, path) =
            s.split_once('=').ok_or_else(|| CliError::InvalidCsvFormat(s.to_string()))?;

        let name = name.trim().to_string();
        let path = PathBuf::from(path.trim());

        if name.is_empty() {
            return Err(CliError::InvalidCsvFormat(s.to_string()));
        }

        Ok(Self { name, path })
    }

    /// Load this schedule from the CSV file.
    pub fn load(&self) -> Result<CsvPricingSchedule, CliError> {
        if !self.path.exists() {
            return Err(CliError::CsvFileNotFound(self.path.clone()));
        }

        CsvPricingSchedule::from_path(self.name.clone(), &self.path)
            .map_err(|e| CliError::CsvLoadError { name: self.name.clone(), source: e })
    }
}

/// A named multiplier schedule parsed from CLI.
///
/// Format: `name=multiplier`
/// Example: `--research.multiplier 128x=128`
#[derive(Debug, Clone)]
pub struct NamedMultiplierSchedule {
    /// Unique name for this schedule
    pub name: String,
    /// Gas multiplier value
    pub multiplier: u64,
}

impl NamedMultiplierSchedule {
    /// Parse from a string in "name=multiplier" format.
    pub fn parse(s: &str) -> Result<Self, CliError> {
        let (name, mult_str) =
            s.split_once('=').ok_or_else(|| CliError::InvalidMultiplierFormat(s.to_string()))?;

        let name = name.trim().to_string();
        let multiplier: u64 = mult_str.trim().parse().map_err(|e: std::num::ParseIntError| {
            CliError::InvalidMultiplierValue(mult_str.to_string(), e.to_string())
        })?;

        if name.is_empty() {
            return Err(CliError::InvalidMultiplierFormat(s.to_string()));
        }

        if multiplier == 0 {
            return Err(CliError::InvalidMultiplierValue(
                mult_str.to_string(),
                "multiplier must be > 0".to_string(),
            ));
        }

        Ok(Self { name, multiplier })
    }

    /// Create the multiplier schedule.
    pub fn to_schedule(&self) -> MultiplierSchedule {
        MultiplierSchedule::new(self.name.clone(), self.multiplier)
    }
}

/// Research mode CLI arguments.
///
/// These arguments configure multi-schedule research experiments.
/// Can be used with clap or similar CLI parsing libraries.
#[derive(Debug, Clone, Default)]
pub struct ResearchArgs {
    /// Enable EIP-2780 intrinsic gas experiment
    pub eip2780_enabled: bool,

    /// Enable EIP-8037 state creation gas experiment
    pub eip8037_enabled: bool,

    /// Enable EIP-8038 state access/write gas experiment
    pub eip8038_enabled: bool,

    /// Enable the composite Glamsterdam repricing experiment.
    ///
    /// Independent of the single-EIP flags above: enabling this alongside them
    /// gives the composite lane its own dataset for comparison, rather than
    /// replacing theirs.
    pub glamsterdam_enabled: bool,

    /// CSV pricing schedules (name=path pairs)
    pub csv_schedules: Vec<NamedCsvSchedule>,

    /// Multiplier schedules (name=multiplier pairs)
    pub multiplier_schedules: Vec<NamedMultiplierSchedule>,

    /// Path to divergence database
    pub db_path: PathBuf,

    /// Block number to start analysis
    pub start_block: u64,

    /// Maximum parallel transactions per block
    pub parallel_txs: Option<usize>,

    /// Trace detail level
    pub trace_detail: String,

    /// Detect gas-dependent loops
    pub detect_gas_loops: bool,

    /// Maximum divergences to record per block
    pub max_divergences_per_block: Option<usize>,

    /// Tiered gas-limit-multiplier sweep applied during schedule replay.
    /// `None` falls back to the single-tier `[1]` default at the call
    /// site; `Some(vec![1, 2, 4, 8])` runs the replay at each tier in
    /// order, accepting the first that succeeds.
    pub gas_limit_multipliers: Option<Vec<u64>>,
}

impl ResearchArgs {
    /// Create new research args with default values.
    pub fn new() -> Self {
        Self {
            db_path: PathBuf::from("research.db"),
            trace_detail: "standard".to_string(),
            detect_gas_loops: true,
            ..Default::default()
        }
    }

    /// Enable EIP-2780 schedule.
    pub const fn with_eip2780(mut self) -> Self {
        self.eip2780_enabled = true;
        self
    }

    /// Enable EIP-8037 schedule.
    pub const fn with_eip8037(mut self) -> Self {
        self.eip8037_enabled = true;
        self
    }

    /// Enable EIP-8038 schedule.
    pub const fn with_eip8038(mut self) -> Self {
        self.eip8038_enabled = true;
        self
    }

    /// Enable the composite Glamsterdam schedule.
    pub const fn with_glamsterdam(mut self) -> Self {
        self.glamsterdam_enabled = true;
        self
    }

    /// Add a CSV schedule.
    pub fn with_csv(mut self, name: &str, path: &str) -> Result<Self, CliError> {
        self.csv_schedules.push(NamedCsvSchedule::parse(&format!("{}={}", name, path))?);
        Ok(self)
    }

    /// Add a CSV schedule from a pre-parsed struct.
    pub fn with_csv_schedule(mut self, schedule: NamedCsvSchedule) -> Self {
        self.csv_schedules.push(schedule);
        self
    }

    /// Add a multiplier schedule.
    pub fn with_multiplier(mut self, name: &str, multiplier: u64) -> Result<Self, CliError> {
        self.multiplier_schedules
            .push(NamedMultiplierSchedule::parse(&format!("{}={}", name, multiplier))?);
        Ok(self)
    }

    /// Add a multiplier schedule from a pre-parsed struct.
    pub fn with_multiplier_schedule(mut self, schedule: NamedMultiplierSchedule) -> Self {
        self.multiplier_schedules.push(schedule);
        self
    }

    /// Set the database path.
    pub fn with_db_path(mut self, path: PathBuf) -> Self {
        self.db_path = path;
        self
    }

    /// Set the start block.
    pub const fn with_start_block(mut self, block: u64) -> Self {
        self.start_block = block;
        self
    }

    /// Set parallel transaction count.
    pub const fn with_parallel_txs(mut self, count: usize) -> Self {
        self.parallel_txs = Some(count);
        self
    }

    /// Set trace detail level.
    pub fn with_trace_detail(mut self, detail: &str) -> Self {
        self.trace_detail = detail.to_string();
        self
    }

    /// Set the maximum divergences to persist per block.
    pub const fn with_max_divergences_per_block(mut self, count: usize) -> Self {
        self.max_divergences_per_block = Some(count);
        self
    }

    /// Set the schedule replay gas-limit-multiplier sweep tiers.
    ///
    /// Replay runs at `tx_gas_limit × tier` for each tier in order;
    /// the first that succeeds is accepted. Pass `vec![1]` for legacy
    /// single-shot behavior.
    pub fn with_gas_limit_multipliers(mut self, multipliers: Vec<u64>) -> Self {
        self.gas_limit_multipliers = Some(multipliers);
        self
    }

    /// Check if any schedules are configured.
    pub const fn has_schedules(&self) -> bool {
        self.eip2780_enabled ||
            self.eip8037_enabled ||
            self.eip8038_enabled ||
            self.glamsterdam_enabled ||
            !self.csv_schedules.is_empty() ||
            !self.multiplier_schedules.is_empty()
    }

    /// Get the number of configured schedules.
    pub const fn schedule_count(&self) -> usize {
        let mut count = 0;
        if self.eip2780_enabled {
            count += 1;
        }
        if self.eip8037_enabled {
            count += 1;
        }
        if self.eip8038_enabled {
            count += 1;
        }
        if self.glamsterdam_enabled {
            count += 1;
        }
        count += self.csv_schedules.len();
        count += self.multiplier_schedules.len();
        count
    }

    /// Build a schedule registry from these arguments.
    pub fn build_registry(&self) -> Result<ScheduleRegistry, CliError> {
        if !self.has_schedules() {
            return Err(CliError::NoSchedules);
        }

        let mut registry = ScheduleRegistry::new();

        // Add EIP-2780 if enabled
        if self.eip2780_enabled {
            registry.register(Eip2780Schedule::new())?;
        }

        // Add EIP-8037 if enabled
        if self.eip8037_enabled {
            registry.register(Eip8037Schedule::new())?;
        }

        // Add EIP-8038 if enabled
        if self.eip8038_enabled {
            registry.register(Eip8038Schedule::new())?;
        }

        // Add the composite Glamsterdam lane if enabled. Registered alongside
        // the single-EIP schedules, not instead of them, so the isolated lanes
        // stay available for comparison and debugging.
        if self.glamsterdam_enabled {
            registry.register(GlamsterdamSchedule::new())?;
        }

        // Add CSV schedules
        for csv in &self.csv_schedules {
            let schedule = csv.load()?;
            registry.register(schedule)?;
        }

        // Add multiplier schedules
        for mult in &self.multiplier_schedules {
            registry.register(mult.to_schedule())?;
        }

        Ok(registry)
    }

    /// Generate a summary of configured schedules.
    pub fn summary(&self) -> String {
        let mut parts = Vec::new();

        if self.eip2780_enabled {
            parts.push("eip-2780".to_string());
        }

        if self.eip8037_enabled {
            parts.push("eip-8037".to_string());
        }

        if self.eip8038_enabled {
            parts.push("eip-8038".to_string());
        }

        if self.glamsterdam_enabled {
            parts.push("glamsterdam-v1".to_string());
        }

        for csv in &self.csv_schedules {
            parts.push(format!("{} (CSV)", csv.name));
        }

        for mult in &self.multiplier_schedules {
            parts.push(format!("{} ({}x)", mult.name, mult.multiplier));
        }

        if parts.is_empty() {
            "No schedules configured".to_string()
        } else {
            format!("{} schedules: {}", parts.len(), parts.join(", "))
        }
    }
}

/// Parse a CSV schedule string (for use with clap `value_parser`).
pub fn parse_csv_schedule(s: &str) -> Result<NamedCsvSchedule, String> {
    NamedCsvSchedule::parse(s).map_err(|e| e.to_string())
}

/// Parse a multiplier schedule string (for use with clap `value_parser`).
pub fn parse_multiplier_schedule(s: &str) -> Result<NamedMultiplierSchedule, String> {
    NamedMultiplierSchedule::parse(s).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_csv_schedule() {
        let schedule = NamedCsvSchedule::parse("7904-v1=./pricing.csv").unwrap();
        assert_eq!(schedule.name, "7904-v1");
        assert_eq!(schedule.path, PathBuf::from("./pricing.csv"));
    }

    #[test]
    fn test_parse_csv_schedule_with_spaces() {
        let schedule = NamedCsvSchedule::parse("  my-schedule  =  ./path/to/file.csv  ").unwrap();
        assert_eq!(schedule.name, "my-schedule");
        assert_eq!(schedule.path, PathBuf::from("./path/to/file.csv"));
    }

    #[test]
    fn test_parse_csv_schedule_invalid() {
        assert!(NamedCsvSchedule::parse("no-equals-sign").is_err());
        assert!(NamedCsvSchedule::parse("=./path.csv").is_err());
    }

    #[test]
    fn test_parse_multiplier_schedule() {
        let schedule = NamedMultiplierSchedule::parse("128x=128").unwrap();
        assert_eq!(schedule.name, "128x");
        assert_eq!(schedule.multiplier, 128);
    }

    #[test]
    fn test_parse_multiplier_schedule_invalid() {
        assert!(NamedMultiplierSchedule::parse("no-equals").is_err());
        assert!(NamedMultiplierSchedule::parse("name=notanumber").is_err());
        assert!(NamedMultiplierSchedule::parse("name=0").is_err());
    }

    #[test]
    fn test_research_args_builder() {
        let args = ResearchArgs::new()
            .with_eip2780()
            .with_eip8037()
            .with_multiplier("128x", 128)
            .unwrap()
            .with_start_block(1000000)
            .with_gas_limit_multipliers(vec![1, 2, 4, 8])
            .with_db_path(PathBuf::from("./test.db"));

        assert!(args.eip2780_enabled);
        assert!(args.eip8037_enabled);
        assert_eq!(args.multiplier_schedules.len(), 1);
        assert_eq!(args.start_block, 1000000);
        assert_eq!(args.gas_limit_multipliers, Some(vec![1, 2, 4, 8]));
        assert_eq!(args.db_path, PathBuf::from("./test.db"));
    }

    #[test]
    fn test_research_args_schedule_count() {
        let args = ResearchArgs::new()
            .with_eip2780()
            .with_eip8037()
            .with_multiplier("128x", 128)
            .unwrap()
            .with_multiplier("256x", 256)
            .unwrap();

        assert_eq!(args.schedule_count(), 4);
    }

    #[test]
    fn test_research_args_summary() {
        let args = ResearchArgs::new().with_eip2780().with_multiplier("128x", 128).unwrap();

        let summary = args.summary();
        assert!(summary.contains("2 schedules"));
        assert!(summary.contains("eip-2780"));
        assert!(summary.contains("128x"));
    }

    #[test]
    fn test_research_args_no_schedules() {
        let args = ResearchArgs::new();
        assert!(!args.has_schedules());

        let result = args.build_registry();
        assert!(result.is_err());
    }

    #[test]
    fn test_research_args_build_registry_with_eip2780() {
        let args = ResearchArgs::new().with_eip2780();
        let registry = args.build_registry().unwrap();

        assert_eq!(registry.len(), 1);
        assert!(registry.get("eip-2780").is_some());
    }

    #[test]
    fn test_research_args_build_registry_with_eip8037() {
        let args = ResearchArgs::new().with_eip8037();
        let registry = args.build_registry().unwrap();

        assert_eq!(registry.len(), 1);
        assert!(registry.get("eip-8037").is_some());
    }

    #[test]
    fn test_research_args_build_registry_with_glamsterdam() {
        let args = ResearchArgs::new().with_glamsterdam();
        let registry = args.build_registry().unwrap();

        assert_eq!(registry.len(), 1);
        assert!(registry.get("glamsterdam-v1").is_some());
        assert!(args.summary().contains("glamsterdam-v1"));
    }

    /// Registering the composite lane next to the single-EIP lanes must give
    /// four distinct schedules — each with its own replay pass and dataset.
    #[test]
    fn test_glamsterdam_registers_alongside_single_eip_lanes() {
        let args =
            ResearchArgs::new().with_eip2780().with_eip8037().with_eip8038().with_glamsterdam();
        let registry = args.build_registry().unwrap();

        assert_eq!(registry.len(), 4);
        for name in ["eip-2780", "eip-8037", "eip-8038", "glamsterdam-v1"] {
            assert!(registry.get(name).is_some(), "missing {name}");
        }
        assert_eq!(args.schedule_count(), 4);
    }

    #[test]
    fn test_research_args_build_registry_with_eip8038() {
        let args = ResearchArgs::new().with_eip8038();
        let registry = args.build_registry().unwrap();

        assert_eq!(registry.len(), 1);
        assert!(registry.get("eip-8038").is_some());
    }

    #[test]
    fn test_research_args_eip8037_and_eip8038_independent() {
        // Both schedules register side by side; 8038 must not collide with 8037.
        let args = ResearchArgs::new().with_eip8037().with_eip8038();
        let registry = args.build_registry().unwrap();

        assert_eq!(registry.len(), 2);
        assert!(registry.get("eip-8037").is_some());
        assert!(registry.get("eip-8038").is_some());
    }

    #[test]
    fn test_research_args_build_registry_with_multiplier() {
        let args = ResearchArgs::new().with_multiplier("128x", 128).unwrap();
        let registry = args.build_registry().unwrap();

        assert_eq!(registry.len(), 1);
        assert!(registry.get("128x").is_some());
    }

    #[test]
    fn test_named_multiplier_to_schedule() {
        use crate::schedule::GasSchedule;

        let named = NamedMultiplierSchedule { name: "test".to_string(), multiplier: 128 };
        let schedule = named.to_schedule();

        assert_eq!(schedule.name(), "test");
        assert_eq!(schedule.multiplier(), 128);
    }
}
