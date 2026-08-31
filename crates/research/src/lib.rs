//! Research mode for analyzing gas price impact on transaction execution.
//!
//! This crate implements a multi-schedule execution analysis system that can
//! compare transactions under different gas schedules simultaneously. The goal
//! is to detect behavioral changes in smart contracts when gas costs change.
//!
//! # Overview
//!
//! The research system allows you to run transactions under multiple gas pricing
//! experiments at once, comparing each against baseline Ethereum execution:
//!
//! - **EIP-2780**: Reduced intrinsic gas based on transaction category
//! - **EIP-8037**: Native state creation gas with reservoir accounting
//! - **CSV Pricing**: Per-opcode/precompile gas repricing from CSV files
//! - **Multipliers**: Uniform gas cost multiplication (e.g., 128x)
//!
//! # Architecture
//!
//! ```text
//! CLI Args ──► ScheduleRegistry ──► MultiScheduleAnalyzer
//!                                           │
//!                    ┌──────────────────────┼──────────────────────┐
//!                    │                      │                      │
//!                    ▼                      ▼                      ▼
//!            EIP-2780 Schedule    CSV Pricing Schedule    Multiplier Schedule
//!            (Intrinsic Only)    (Execution Gas)         (Execution Gas)
//!                    │                      │                      │
//!                    └──────────────────────┴──────────────────────┘
//!                                           │
//!                                           ▼
//!                              MultiScheduleComparisonResult
//!                                           │
//!                                           ▼
//!                                  DivergenceDatabase
//! ```
//!
//! # Key Components
//!
//! - [`schedule`]: Gas schedule trait and implementations
//!   - [`schedule::GasSchedule`]: Core trait for gas pricing experiments
//!   - [`schedule::AmsterdamSchedule`]: the Amsterdam repricing stack (EIP-2780 + 7976 + 7981 +
//!     8037 + 8038) via revm's native `SpecId::AMSTERDAM`
//!   - [`schedule::CsvPricingSchedule`]: Per-opcode pricing from CSV files
//!   - [`schedule::MultiplierSchedule`]: Uniform gas multiplier
//!   - [`schedule::ScheduleRegistry`]: Manages multiple schedules
//!
//! - [`cli`]: CLI argument types
//!   - [`ResearchArgs`]: Builder for research configuration
//!   - [`NamedCsvSchedule`], [`NamedMultiplierSchedule`]: Parsed CLI args
//!
//! - [`analyzer`]: High-level analysis
//!   - [`MultiScheduleAnalyzer`]: Coordinates comparison across schedules
//!   - [`ExecutionSummaryBuilder`]: Builds execution summaries
//!
//! - [`multi_schedule_inspector`]: Execution tracking
//!   - [`ScheduleInspector`]: Applies gas cost modifications during execution
//!
//! - [`comparison`]: Result types
//!   - [`MultiScheduleComparisonResult`]: Full comparison for a transaction
//!   - [`ScheduleComparisonResult`]: Per-schedule results
//!   - [`IntrinsicComparison`], [`ExecutionComparison`]: Detailed comparisons
//!
//! - [`database`][]: Persistence
//!   - [`DivergenceDatabase`]: `SQLite` storage for divergences
//!
//! # Example: Configure via CLI
//!
//! ```rust,ignore
//! use reth_research::{ResearchArgs, MultiScheduleAnalyzer, DivergenceDatabase};
//! use std::path::PathBuf;
//!
//! // Configure schedules
//! let args = ResearchArgs::new()
//!     .with_eip2780()  // Enable EIP-2780 intrinsic gas experiment
//!     .with_eip8037()  // Enable EIP-8037 state-gas experiment
//!     .with_csv("7904-v1", "./schedules/7904_v1.csv")?  // CSV pricing
//!     .with_multiplier("128x", 128)?  // Uniform 128x multiplier
//!     .with_db_path(PathBuf::from("research.db"));
//!
//! // Build registry and analyzer
//! let registry = args.build_registry()?;
//! let db = DivergenceDatabase::open(&args.db_path)?;
//! let mut analyzer = MultiScheduleAnalyzer::new(registry)?
//!     .with_database(db);
//!
//! // For each transaction:
//! let inspector = analyzer.create_inspector();
//! // ... execute with inspector attached ...
//! ```
//!
//! # Example: Amsterdam Intrinsic Gas Analysis
//!
//! ```rust
//! use alloy_primitives::{Address, Bytes, U256};
//! use reth_research::schedule::{AmsterdamSchedule, GasSchedule, RecipientInfo, TxContext};
//!
//! let schedule = AmsterdamSchedule::new();
//!
//! // Simple ETH transfer to an existing EOA
//! let ctx = TxContext {
//!     baseline_intrinsic_gas: 21000,
//!     sender: Address::repeat_byte(0x01),
//!     recipient: Some(Address::repeat_byte(0x02)),
//!     value: U256::from(1000),
//!     input: Bytes::new(),
//!     gas_limit: 100_000,
//!     is_create: false,
//!     recipient_info: Some(RecipientInfo {
//!         exists: true,
//!         has_code: false, // EOA, not contract
//!         balance: U256::from(100),
//!         nonce: 1,
//!     }),
//!     ..Default::default()
//! };
//!
//! // EIP-2780 decomposes the intrinsic: TX_BASE + COLD_ACCOUNT_ACCESS + TX_VALUE_COST
//! let intrinsic = schedule.intrinsic_gas(&ctx).unwrap();
//! assert_eq!(intrinsic, 12_000 + 3_000 + 6_000); // vs 21000 baseline
//!
//! let category = schedule.tx_category(&ctx).unwrap();
//! assert_eq!(category, "value_call");
//! ```
//!
//! # Example: CSV-based Per-Opcode Pricing
//!
//! ```rust
//! use reth_research::schedule::{CsvPricingSchedule, GasSchedule, OpcodeContext};
//!
//! let csv_data = r#"Opcode,Parameter,Current Gas,New Gas
//! DIV,constant,5,15
//! KECCAK256,constant,30,45
//! KECCAK256,msg_size,6,6
//! "#;
//!
//! let schedule =
//!     CsvPricingSchedule::from_csv("my-experiment".to_string(), csv_data.as_bytes()).unwrap();
//!
//! let ctx = OpcodeContext::default();
//!
//! // DIV opcode: +10 gas delta (15 - 5)
//! assert_eq!(schedule.opcode_gas_delta(0x04, &ctx), 10);
//!
//! // Unaffected opcodes return 0
//! assert_eq!(schedule.opcode_gas_delta(0x01, &ctx), 0); // ADD
//! ```
//!
//! # CLI Usage Examples
//!
//! ```bash
//! # Amsterdam repricing only
//! reth-research --research.amsterdam --research.db-path ./results.db
//!
//! # Multiple CSV schedules for A/B testing
//! reth-research \
//!   --research.csv 7904-v1=./schedules/7904_v1.csv \
//!   --research.csv 7904-v2=./schedules/7904_v2.csv \
//!   --research.csv aggressive=./schedules/aggressive.csv
//!
//! # Combined experiment
//! reth-research \
//!   --research.amsterdam \
//!   --research.csv 7904-prelim=./7904_prelim.csv \
//!   --research.multiplier 128x=128 \
//!   --research.gas-limit-multiplier 8 \
//!   --research.db-path ./full-analysis.db
//! ```
//!
//! # Database Queries
//!
//! Results are stored in `SQLite` for analysis:
//!
//! ```sql
//! -- Divergences by schedule
//! SELECT schedule_name, COUNT(*) as divergences
//! FROM schedule_divergences
//! GROUP BY schedule_name;
//!
//! -- EIP-2780 category breakdown
//! SELECT tx_category, COUNT(*), AVG(gas_delta)
//! FROM schedule_divergences
//! WHERE schedule_name = 'eip-2780'
//! GROUP BY tx_category;
//!
//! -- Status-changing divergences (would cause tx failure)
//! SELECT * FROM schedule_divergences
//! WHERE divergence_type = 'status';
//! ```

#![cfg_attr(not(test), warn(unused_crate_dependencies))]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]

pub mod block_aggregator;
pub mod cli;
pub mod contract_metadata;
pub mod database;
pub mod divergence;
pub mod export;
pub mod external_labels;
pub mod multi_schedule_inspector;
pub mod oog_chain;
pub mod opcode;
pub mod schedule;
pub mod step_trace;
pub mod tracking_inspector;

pub use cli::{CliError, NamedCsvSchedule, NamedMultiplierSchedule, ResearchArgs};
pub use divergence::{AggregateClass, DivergenceFacts, EventLog, OperationCounts};
pub use multi_schedule_inspector::{ScheduleInspector, ScheduleResult};
pub use tracking_inspector::{EventLogEntry, TrackingInspector};

/// Re-export error types
pub use database::DatabaseError;
pub use schedule::ScheduleError;
