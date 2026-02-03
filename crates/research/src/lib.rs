//! Research mode for analyzing gas price impact on transaction execution.
//!
//! This crate implements a dual-execution system that runs each transaction twice:
//! once with normal gas costs and once with modified (higher) gas costs. The goal
//! is to detect behavioral changes in smart contracts when gas prices increase
//! significantly.
//!
//! # Architecture
//!
//! - [`ResearchExecutor`]: Wraps an executor to perform dual execution
//! - [`GasResearchInspector`]: Tracks execution details and simulates high gas costs
//! - [`DivergenceDatabase`]: Stores detected divergences for later analysis
//! - [`ResearchConfig`]: Configuration for research mode
//!
//! # Example Usage
//!
//! ```rust,ignore
//! use reth_research::{ResearchExecutor, ResearchConfig, GasPricingTable};
//! use std::path::Path;
//!
//! // Load gas pricing from CSV
//! let gas_pricing = GasPricingTable::from_csv_path(Path::new("7904_prelim_numbers.csv"))?;
//!
//! let config = ResearchConfig {
//!     gas_pricing,
//!     divergence_db_path: "divergence.db".into(),
//!     ..Default::default()
//! };
//!
//! let research_executor = ResearchExecutor::new(base_executor, config)?;
//! ```

#![cfg_attr(not(test), warn(unused_crate_dependencies))]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]

pub mod config;
pub mod database;
pub mod divergence;
pub mod executor;
pub mod gas_pricing;
pub mod inspector;
pub mod metrics;
pub mod tracking_inspector;

pub use config::{ResearchConfig, TraceDetail};
pub use database::DivergenceDatabase;
pub use divergence::{CallTrees, Divergence, DivergenceType, EventLog, EventLogs, OperationCounts};
pub use executor::ResearchExecutor;
pub use gas_pricing::{GasPricingError, GasPricingTable, OperationPricing};
pub use inspector::GasResearchInspector;
pub use tracking_inspector::{EventLogEntry, TrackingInspector};

/// Re-export error types
pub use database::DatabaseError;
pub use executor::ResearchError;
