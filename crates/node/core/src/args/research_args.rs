//! clap [Args](clap::Args) for multi-schedule gas price research mode

use clap::Args;
use std::path::PathBuf;

/// Parameters for multi-schedule gas price research mode.
///
/// This supports running multiple gas schedule experiments simultaneously,
/// comparing each against baseline Ethereum execution.
#[derive(Debug, Clone, Args, PartialEq, Eq)]
#[command(next_help_heading = "Research")]
pub struct ResearchArgs {
    /// Enable EIP-2780 intrinsic gas experiment.
    ///
    /// This reduces intrinsic gas costs based on transaction category
    /// (e.g., simple transfers cost less than contract calls).
    #[arg(long = "research.eip2780", help_heading = "Research")]
    pub eip2780: bool,

    /// Enable EIP-8037 state creation gas experiment.
    ///
    /// This uses revm's native EIP-8037 state-gas and reservoir accounting.
    #[arg(long = "research.eip8037", help_heading = "Research")]
    pub eip8037: bool,

    /// Enable EIP-8038 state access/write gas experiment.
    ///
    /// This reprices state access/write/create costs on the block's native
    /// spec, independent of EIP-8037's state-gas reservoir.
    #[arg(long = "research.eip8038", help_heading = "Research")]
    pub eip8038: bool,

    /// Add a CSV-based gas pricing schedule.
    ///
    /// Format: name=path (e.g., --research.csv 7904-v1=./pricing.csv)
    /// Can be specified multiple times for A/B testing.
    #[arg(long = "research.csv", value_name = "NAME=PATH", help_heading = "Research")]
    pub csv_schedules: Vec<String>,

    /// Add a uniform gas multiplier schedule.
    ///
    /// Format: name=multiplier (e.g., --research.multiplier 128x=128)
    /// Can be specified multiple times.
    #[arg(long = "research.multiplier", value_name = "NAME=MULT", help_heading = "Research")]
    pub multiplier_schedules: Vec<String>,

    /// Path to the SQLite divergence database file. Opened in WAL mode so
    /// the consumer dashboard (DuckDB sqlite_scanner) can read concurrently.
    #[arg(
        long = "research.db-path",
        default_value = "./divergences.sqlite",
        help_heading = "Research"
    )]
    pub db_path: PathBuf,

    /// Block number to start research analysis.
    #[arg(long = "research.start-block", default_value_t = 0, help_heading = "Research")]
    pub start_block: u64,

    /// Maximum drill-in (divergence) rows persisted per block — one budget
    /// shared across ALL schedules, spent first-come in tx-major order.
    /// Dropped drill-ins still count in coverage (`expected_drill_in_count` /
    /// `drill_ins_truncated`) and never affect the class aggregates. Omit for
    /// unlimited.
    #[arg(
        long = "research.max-divergences-per-block",
        value_name = "COUNT",
        help_heading = "Research"
    )]
    pub max_divergences_per_block: Option<usize>,

    /// Tiered sweep of gas-limit multipliers to try during schedule replay.
    ///
    /// The replay runs at `tx_gas_limit × multiplier` for each tier in order;
    /// the first tier whose replay succeeds is accepted, and
    /// `min_multiplier_to_succeed = schedule_gas_used / tx_gas_limit` is
    /// recorded (a continuous value, not the tier itself). If no tier
    /// succeeds, `min_multiplier_to_succeed = NULL` and `replay_halt_oog`
    /// captures whether the highest tier OOG'd (`1` — could still benefit
    /// from more gas beyond the ceiling) vs halted for a non-gas reason
    /// (`0` — no amount of gas would resolve it).
    ///
    /// The success comparison still uses the transaction's original gas
    /// limit, so this only reveals how much more gas would be needed without
    /// masking the fact that the original transaction would fail under the
    /// schedule. Default `1,2,4,8` covers the common range; pass `1` to
    /// disable the sweep entirely (legacy behavior).
    #[arg(
        long = "research.gas-limit-multipliers",
        value_name = "MULTS",
        value_delimiter = ',',
        default_value = "1,2,4,8",
        help_heading = "Research"
    )]
    pub gas_limit_multipliers: Vec<u64>,

    /// Enable historical backfill during idle periods.
    ///
    /// When the node has caught up to the chain head and no notifications are
    /// pending, the ExEx walks the chain backward from the current head and
    /// re-analyzes blocks whose coverage rows are missing for the current
    /// schedule configuration. Live `ChainCommitted` notifications take
    /// priority — backfill resumes on the next idle window.
    #[arg(long = "research.backfill", help_heading = "Research")]
    pub backfill: bool,

    /// Inclusive lower bound for backfill. Backfill stops once the cursor
    /// crosses this block number. Defaults to genesis (0).
    #[arg(
        long = "research.backfill-min-block",
        value_name = "BLOCK",
        default_value_t = 0,
        help_heading = "Research"
    )]
    pub backfill_min_block: u64,

    /// Inclusive upper bound for backfill. The cursor starts at
    /// `min(head - 1, backfill_max_block)` and walks down to
    /// `backfill_min_block`. `None` (the default) starts at `head - 1`
    /// (today's behavior). Set both bounds to analyze an explicit
    /// `[min, max]` window — needed for windowed re-analysis, where a fresh
    /// per-window database carries no dedup state.
    #[arg(long = "research.backfill-max-block", value_name = "BLOCK", help_heading = "Research")]
    pub backfill_max_block: Option<u64>,

    /// Maximum concurrent backfill workers.
    ///
    /// Each worker analyzes one historical block at a time on a blocking
    /// thread, so this scales backfill throughput linearly with cores until
    /// disk reads or the DB writer saturate. `0` (the default) auto-selects
    /// `available_parallelism - 1`, leaving one core for the live arm and the
    /// async runtime.
    #[arg(
        long = "research.backfill-concurrency",
        value_name = "WORKERS",
        default_value_t = 0,
        help_heading = "Research"
    )]
    pub backfill_concurrency: usize,

    /// Run the `contract-metadata-backfill` flow instead of the normal
    /// research analysis.
    ///
    /// When set, the ExEx walks every distinct contract address recorded in
    /// `divergence_call_frames`, fetches the deployed bytecode from reth
    /// state, parses the Solidity CBOR metadata trailer, and UPSERTs a row
    /// into `contract_metadata`. The process exits after the backfill
    /// completes; no live analysis runs. Idempotent and resumable — re-runs
    /// skip codehashes already in the DB.
    #[arg(long = "research.metadata-backfill", help_heading = "Research")]
    pub metadata_backfill: bool,

    /// Interval (seconds) for the periodic contract-metadata backfill that
    /// runs alongside the live research ExEx. Each tick re-runs the
    /// backfill against any call-frame addresses whose codehash isn't yet
    /// in `contract_metadata`, so `contract_metadata` fills incrementally
    /// without the operator running the one-shot mode. Set `0` to disable.
    #[arg(
        long = "research.metadata-backfill-interval-secs",
        value_name = "SECS",
        default_value_t = 60,
        help_heading = "Research"
    )]
    pub metadata_backfill_interval_secs: u64,

    /// Interval (seconds) for the periodic contract-label backfill that
    /// fetches per-address contract names and protocol tags from
    /// external APIs (Blockscout → Sourcify → Etherscan fallback chain).
    /// Set `0` to disable. Requires `--research.label-config-path` for
    /// the Etherscan rung; Blockscout + Sourcify work key-less.
    #[arg(
        long = "research.contract-labels-interval-secs",
        value_name = "SECS",
        default_value_t = 0,
        help_heading = "Research"
    )]
    pub contract_labels_interval_secs: u64,

    /// Interval (seconds) for the periodic function-signature backfill
    /// that fetches 4-byte selector → signature mappings from OpenChain.
    /// Set `0` to disable.
    #[arg(
        long = "research.function-signatures-interval-secs",
        value_name = "SECS",
        default_value_t = 0,
        help_heading = "Research"
    )]
    pub function_signatures_interval_secs: u64,

    /// Path to a TOML config file holding optional credentials and URL
    /// overrides for the external-label backfill (`etherscan_api_key`,
    /// `blockscout_base_url`, `sourcify_base_url`, `etherscan_base_url`,
    /// `openchain_base_url`, `chain_id`). When absent, the backfill runs
    /// with default mainnet endpoints and no Etherscan rung.
    #[arg(long = "research.label-config-path", value_name = "PATH", help_heading = "Research")]
    pub label_config_path: Option<PathBuf>,

    /// Path to the strict `ClickHouse` export `TOML` config. When set, each
    /// analyzed block output is durably enqueued in a `SQLite` outbox and
    /// shipped to `ClickHouse` by an embedded worker. Export is disabled when
    /// absent.
    ///
    /// The endpoint must be `https://`; the password is resolved at startup from
    /// the environment variable named by `password_env` in the file (never the
    /// command line). Requires a real on-disk `--research.db-path` (not
    /// `:memory:`).
    #[arg(long = "research.export-config-path", value_name = "PATH", help_heading = "Research")]
    pub export_config_path: Option<PathBuf>,
}

impl Default for ResearchArgs {
    fn default() -> Self {
        Self {
            eip2780: false,
            eip8037: false,
            eip8038: false,
            csv_schedules: Vec::new(),
            multiplier_schedules: Vec::new(),
            // Keep in sync with the `#[arg(default_value = ...)]` on `db_path`.
            db_path: PathBuf::from("./divergences.sqlite"),
            start_block: 0,
            max_divergences_per_block: None,
            gas_limit_multipliers: vec![1, 2, 4, 8],
            backfill: false,
            backfill_min_block: 0,
            backfill_max_block: None,
            backfill_concurrency: 0,
            metadata_backfill: false,
            metadata_backfill_interval_secs: 60,
            contract_labels_interval_secs: 0,
            function_signatures_interval_secs: 0,
            label_config_path: None,
            export_config_path: None,
        }
    }
}

impl ResearchArgs {
    /// Check if any research schedules are configured.
    pub const fn has_schedules(&self) -> bool {
        self.eip2780 ||
            self.eip8037 ||
            self.eip8038 ||
            !self.csv_schedules.is_empty() ||
            !self.multiplier_schedules.is_empty()
    }

    /// Get the number of configured schedules.
    pub const fn schedule_count(&self) -> usize {
        let mut count = 0;
        if self.eip2780 {
            count += 1;
        }
        if self.eip8037 {
            count += 1;
        }
        if self.eip8038 {
            count += 1;
        }
        count += self.csv_schedules.len();
        count += self.multiplier_schedules.len();
        count
    }
}

#[cfg(feature = "research")]
impl ResearchArgs {
    /// Build a schedule registry from these CLI arguments.
    pub fn build_registry(
        &self,
    ) -> Result<reth_research::schedule::ScheduleRegistry, reth_research::CliError> {
        use reth_research::{NamedCsvSchedule, NamedMultiplierSchedule};

        let mut args = reth_research::ResearchArgs::new()
            .with_db_path(self.db_path.clone())
            .with_start_block(self.start_block);

        if let Some(max) = self.max_divergences_per_block {
            args = args.with_max_divergences_per_block(max);
        }

        if self.eip2780 {
            args = args.with_eip2780();
        }

        if self.eip8037 {
            args = args.with_eip8037();
        }

        if self.eip8038 {
            args = args.with_eip8038();
        }

        args = args.with_gas_limit_multipliers(self.gas_limit_multipliers.clone());

        for csv_spec in &self.csv_schedules {
            let schedule = NamedCsvSchedule::parse(csv_spec)?;
            args = args.with_csv_schedule(schedule);
        }

        for mult_spec in &self.multiplier_schedules {
            let schedule = NamedMultiplierSchedule::parse(mult_spec)?;
            args = args.with_multiplier_schedule(schedule);
        }

        args.build_registry()
    }

    /// Opens the divergence database (DuckDB).
    pub fn open_database(
        &self,
    ) -> Result<reth_research::database::DivergenceDatabase, reth_research::DatabaseError> {
        reth_research::database::DivergenceDatabase::open(&self.db_path)
    }

    /// Convert to the research crate's [`reth_research::ResearchArgs`] type.
    pub fn to_research_args(&self) -> Result<reth_research::ResearchArgs, reth_research::CliError> {
        use reth_research::{NamedCsvSchedule, NamedMultiplierSchedule};

        let mut args = reth_research::ResearchArgs::new()
            .with_db_path(self.db_path.clone())
            .with_start_block(self.start_block);

        if let Some(max) = self.max_divergences_per_block {
            args = args.with_max_divergences_per_block(max);
        }

        if self.eip2780 {
            args = args.with_eip2780();
        }

        if self.eip8037 {
            args = args.with_eip8037();
        }

        if self.eip8038 {
            args = args.with_eip8038();
        }

        args = args.with_gas_limit_multipliers(self.gas_limit_multipliers.clone());

        for csv_spec in &self.csv_schedules {
            let schedule = NamedCsvSchedule::parse(csv_spec)?;
            args = args.with_csv_schedule(schedule);
        }

        for mult_spec in &self.multiplier_schedules {
            let schedule = NamedMultiplierSchedule::parse(mult_spec)?;
            args = args.with_multiplier_schedule(schedule);
        }

        Ok(args)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    /// A helper type to parse Args more easily
    #[derive(Parser)]
    struct CommandParser<T: Args> {
        #[command(flatten)]
        args: T,
    }

    #[test]
    fn test_parse_research_args_default() {
        let args = CommandParser::<ResearchArgs>::parse_from(["reth"]).args;
        assert_eq!(args, ResearchArgs::default());
        assert!(!args.has_schedules());
    }

    #[test]
    fn test_parse_research_args_eip2780() {
        let args = CommandParser::<ResearchArgs>::parse_from(["reth", "--research.eip2780"]).args;
        assert!(args.eip2780);
        assert!(args.has_schedules());
        assert_eq!(args.schedule_count(), 1);
    }

    #[test]
    fn test_parse_research_args_eip8037() {
        let args = CommandParser::<ResearchArgs>::parse_from(["reth", "--research.eip8037"]).args;
        assert!(args.eip8037);
        assert!(args.has_schedules());
        assert_eq!(args.schedule_count(), 1);
    }

    #[test]
    fn test_parse_research_args_eip8038() {
        let args = CommandParser::<ResearchArgs>::parse_from(["reth", "--research.eip8038"]).args;
        assert!(args.eip8038);
        assert!(args.has_schedules());
        assert_eq!(args.schedule_count(), 1);
    }

    #[test]
    fn test_parse_research_args_eip8037_and_eip8038() {
        let args = CommandParser::<ResearchArgs>::parse_from([
            "reth",
            "--research.eip8037",
            "--research.eip8038",
        ])
        .args;
        assert!(args.eip8037);
        assert!(args.eip8038);
        assert_eq!(args.schedule_count(), 2);
    }

    #[test]
    fn test_parse_research_args_csv() {
        let args = CommandParser::<ResearchArgs>::parse_from([
            "reth",
            "--research.csv",
            "7904-v1=./pricing.csv",
        ])
        .args;
        assert_eq!(args.csv_schedules, vec!["7904-v1=./pricing.csv"]);
        assert!(args.has_schedules());
        assert_eq!(args.schedule_count(), 1);
    }

    #[test]
    fn test_parse_research_args_multiple_csv() {
        let args = CommandParser::<ResearchArgs>::parse_from([
            "reth",
            "--research.csv",
            "7904-v1=./v1.csv",
            "--research.csv",
            "7904-v2=./v2.csv",
        ])
        .args;
        assert_eq!(args.csv_schedules.len(), 2);
        assert_eq!(args.schedule_count(), 2);
    }

    #[test]
    fn test_parse_research_args_multiplier() {
        let args = CommandParser::<ResearchArgs>::parse_from([
            "reth",
            "--research.multiplier",
            "128x=128",
        ])
        .args;
        assert_eq!(args.multiplier_schedules, vec!["128x=128"]);
        assert!(args.has_schedules());
    }

    #[test]
    fn test_parse_research_args_combined() {
        let args = CommandParser::<ResearchArgs>::parse_from([
            "reth",
            "--research.eip2780",
            "--research.csv",
            "7904-prelim=./7904.csv",
            "--research.multiplier",
            "128x=128",
            "--research.db-path",
            "./results.db",
            "--research.start-block",
            "18000000",
            "--research.max-divergences-per-block",
            "25",
            "--research.gas-limit-multipliers",
            "1,2,4,8",
        ])
        .args;
        assert!(args.eip2780);
        assert_eq!(args.csv_schedules, vec!["7904-prelim=./7904.csv"]);
        assert_eq!(args.multiplier_schedules, vec!["128x=128"]);
        assert_eq!(args.db_path, PathBuf::from("./results.db"));
        assert_eq!(args.start_block, 18000000);
        assert_eq!(args.max_divergences_per_block, Some(25));
        assert_eq!(args.gas_limit_multipliers, vec![1, 2, 4, 8]);
        assert_eq!(args.schedule_count(), 3);
    }

    #[test]
    fn test_parse_research_args_export_disabled_by_default() {
        let args = CommandParser::<ResearchArgs>::parse_from(["reth"]).args;
        assert_eq!(args.export_config_path, None);
    }

    #[test]
    fn test_parse_research_args_export_config_path() {
        let args = CommandParser::<ResearchArgs>::parse_from([
            "reth",
            "--research.export-config-path",
            "/etc/reth-research/clickhouse.toml",
        ])
        .args;
        assert_eq!(
            args.export_config_path,
            Some(PathBuf::from("/etc/reth-research/clickhouse.toml"))
        );
    }

    #[test]
    fn test_parse_research_args_backfill_max_block() {
        let args = CommandParser::<ResearchArgs>::parse_from(["reth"]).args;
        assert_eq!(args.backfill_max_block, None);
        let args = CommandParser::<ResearchArgs>::parse_from([
            "reth",
            "--research.backfill-max-block",
            "25000000",
        ])
        .args;
        assert_eq!(args.backfill_max_block, Some(25_000_000));
    }

    #[test]
    fn test_parse_research_args_gas_limit_multipliers_default() {
        let args = CommandParser::<ResearchArgs>::parse_from(["reth"]).args;
        assert_eq!(args.gas_limit_multipliers, vec![1, 2, 4, 8]);
    }

    #[test]
    fn test_parse_research_args_gas_limit_multipliers_single() {
        let args = CommandParser::<ResearchArgs>::parse_from([
            "reth",
            "--research.gas-limit-multipliers",
            "1",
        ])
        .args;
        // Single value = legacy "no sweep" behavior.
        assert_eq!(args.gas_limit_multipliers, vec![1]);
    }
}
