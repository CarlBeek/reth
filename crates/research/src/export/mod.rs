//! Durable `ClickHouse` export for research block outputs.
//!
//! The export pipeline captures each analyzed [`BlockOutput`](crate::database::BlockOutput)
//! as a versioned, compressed envelope in a transactional `SQLite` outbox, then
//! drains the outbox from a background worker that inserts the data into
//! `ClickHouse` over HTTPS. Remote failures never block replay or local `SQLite`
//! persistence; deterministic identities plus `ReplacingMergeTree` make
//! at-least-once delivery safe.
//!
//! - [`model`]: dataset identity (manifest + hash), the outbox envelope, and the `ClickHouse` row
//!   contract with conversion from a `BlockOutput`.
//! - [`config`]: strict TOML configuration; export is opt-in.
//! - [`clickhouse`]: the HTTPS `JSONEachRow` client and failure classification.
//! - [`worker`]: the background drain loop.

pub mod clickhouse;
pub mod config;
pub mod model;
pub mod worker;

pub use clickhouse::{ClickHouseClient, ClickHouseError, DestinationTable};
pub use config::{ConfigError, ExportConfig};
pub use model::{
    block_output_to_rows, coverage_row_id, divergence_row_id, export_id, normalize_gas_tiers,
    summary_row_id, AnalysisManifestV1, BlockClickHouseRows, CaptureMetadataV1, CoverageRow,
    DivergenceExportRow, EncodedEnvelope, ExportEnvelopeV1, ExportModelError, RunRow,
    ScheduleManifestV1, SummaryRow, ENVELOPE_FORMAT_VERSION, MANIFEST_FORMAT_VERSION,
    REPLAY_SEMANTICS,
};
pub use worker::{run_export_worker, ClickHouseSink, ExportError};
