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
//! use reth_research::{ResearchExecutor, ResearchConfig};
//!
//! let config = ResearchConfig {
//!     gas_multiplier: 128,
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
pub mod inspector;
pub mod metrics;
pub mod tracking_inspector;

pub use config::{ResearchConfig, TraceDetail};
pub use database::DivergenceDatabase;
pub use divergence::{Divergence, DivergenceType, OperationCounts};
pub use executor::ResearchExecutor;
pub use inspector::GasResearchInspector;
pub use tracking_inspector::TrackingInspector;

/// Re-export error types
pub use database::DatabaseError;
pub use executor::ResearchError;
