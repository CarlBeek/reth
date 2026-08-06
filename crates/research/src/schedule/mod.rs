//! Gas schedule abstraction for multi-schedule research experiments.
//!
//! This module provides a trait-based abstraction for defining different gas schedules
//! that can be tested simultaneously against baseline Ethereum execution.
//!
//! # Architecture
//!
//! - [`GasSchedule`]: Core trait defining how gas costs are modified
//! - [`BaselineSchedule`]: No modifications (reference implementation)
//! - [`Eip2780Schedule`]: Reduced intrinsic gas based on transaction category
//! - [`Eip8037Schedule`]: Native EIP-8037 state-gas metering
//! - [`Eip8038Schedule`]: EIP-8038 state access/write repricing (native-spec)
//! - [`CsvPricingSchedule`]: Per-opcode/precompile pricing from CSV files
//! - [`MultiplierSchedule`]: Uniform gas cost multiplier
//! - [`ScheduleRegistry`]: Registry for loading and managing multiple schedules
//!
//! # Example
//!
//! ```rust,ignore
//! use reth_research::schedule::{GasSchedule, ScheduleRegistry, Eip2780Schedule, Eip8037Schedule, CsvPricingSchedule};
//!
//! let mut registry = ScheduleRegistry::new();
//! registry.register(Eip2780Schedule::new())?;
//! registry.register(Eip8037Schedule::new())?;
//! registry.register(CsvPricingSchedule::from_path("7904-v1".to_string(), Path::new("./pricing.csv"))?)?;
//!
//! for schedule in registry.all() {
//!     println!("Loaded schedule: {}", schedule.name());
//! }
//! ```

mod baseline;
mod common;
mod context;
mod csv_pricing;
mod eip2780;
mod eip8037;
mod eip8038;
mod multiplier;
mod registry;
mod traits;

pub use baseline::BaselineSchedule;
pub use common::{gas_params_for, initial_and_floor_gas_for};
pub use context::{OpcodeContext, RecipientInfo, TxContext};
pub use csv_pricing::{CsvPricingError, CsvPricingSchedule, GasPricingTable, OperationPricing};
pub use eip2780::{Eip2780Category, Eip2780Constants, Eip2780Schedule};
pub use eip8037::{Eip8037Constants, Eip8037Schedule};
pub use eip8038::{
    apply_gas_overrides as apply_eip8038_gas_overrides, Eip8038Constants, Eip8038Schedule,
};
pub use multiplier::MultiplierSchedule;
pub use registry::{ScheduleError, ScheduleRegistry};
pub use traits::{GasSchedule, GasTaxBreakdown, ScheduleKind};
