//! Gas schedule abstraction for multi-schedule research experiments.
//!
//! This module provides a trait-based abstraction for defining different gas schedules
//! that can be tested simultaneously against baseline Ethereum execution.
//!
//! # Architecture
//!
//! - [`GasSchedule`]: Core trait defining how gas costs are modified
//! - [`BaselineSchedule`]: No modifications (reference implementation)
//! - [`AmsterdamSchedule`]: the Amsterdam repricing stack (EIP-2780 + 7976 + 7981 + 8037 + 8038)
//!   via revm's native `SpecId::AMSTERDAM`
//! - [`CsvPricingSchedule`]: Per-opcode/precompile pricing from CSV files
//! - [`MultiplierSchedule`]: Uniform gas cost multiplier
//! - [`ScheduleRegistry`]: Registry for loading and managing multiple schedules
//!
//! # Example
//!
//! ```rust,ignore
//! use reth_research::schedule::{AmsterdamSchedule, CsvPricingSchedule, GasSchedule, ScheduleRegistry};
//!
//! let mut registry = ScheduleRegistry::new();
//! registry.register(AmsterdamSchedule::new())?;
//! registry.register(CsvPricingSchedule::from_path("7904-v1".to_string(), Path::new("./pricing.csv"))?)?;
//!
//! for schedule in registry.all() {
//!     println!("Loaded schedule: {}", schedule.name());
//! }
//! ```

mod amsterdam;
mod baseline;
mod common;
mod context;
mod csv_pricing;
mod multiplier;
mod registry;
mod traits;

pub use amsterdam::AmsterdamSchedule;
pub use baseline::BaselineSchedule;
pub use common::{gas_params_for, initial_and_floor_gas_for};
pub use context::{OpcodeContext, RecipientInfo, TxContext};
pub use csv_pricing::{CsvPricingError, CsvPricingSchedule, GasPricingTable, OperationPricing};
pub use multiplier::MultiplierSchedule;
pub use registry::{ScheduleError, ScheduleRegistry};
pub use traits::{GasSchedule, GasTaxBreakdown, ScheduleKind};
