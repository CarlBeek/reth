//! clap [Args](clap::Args) for multi-schedule gas price research mode

use clap::Args;
use std::path::PathBuf;

/// Parameters for multi-schedule gas price research mode.
///
/// This supports running multiple gas schedule experiments simultaneously,
/// comparing each against baseline Ethereum execution.
#[derive(Debug, Clone, Args, PartialEq)]
#[command(next_help_heading = "Research")]
pub struct ResearchArgs {
    /// Enable EIP-2780 intrinsic gas experiment.
    ///
    /// This reduces intrinsic gas costs based on transaction category
    /// (e.g., simple transfers cost less than contract calls).
    #[arg(long = "research.eip2780", help_heading = "Research")]
    pub eip2780: bool,

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

    /// Path to divergence database file.
    #[arg(long = "research.db-path", default_value = "./divergence.db", help_heading = "Research")]
    pub db_path: PathBuf,

    /// Block number to start research analysis.
    #[arg(long = "research.start-block", default_value_t = 0, help_heading = "Research")]
    pub start_block: u64,
}

impl Default for ResearchArgs {
    fn default() -> Self {
        Self {
            eip2780: false,
            csv_schedules: Vec::new(),
            multiplier_schedules: Vec::new(),
            db_path: PathBuf::from("./divergence.db"),
            start_block: 0,
        }
    }
}

impl ResearchArgs {
    /// Check if any research schedules are configured.
    pub fn has_schedules(&self) -> bool {
        self.eip2780 || !self.csv_schedules.is_empty() || !self.multiplier_schedules.is_empty()
    }

    /// Get the number of configured schedules.
    pub fn schedule_count(&self) -> usize {
        let mut count = 0;
        if self.eip2780 {
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

        if self.eip2780 {
            args = args.with_eip2780();
        }

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

    /// Opens the divergence database.
    pub fn open_database(
        &self,
    ) -> Result<reth_research::database::DivergenceDatabase, reth_research::database::DatabaseError>
    {
        reth_research::database::DivergenceDatabase::open(&self.db_path)
    }

    /// Convert to the research crate's ResearchArgs type.
    pub fn to_research_args(&self) -> Result<reth_research::ResearchArgs, reth_research::CliError> {
        use reth_research::{NamedCsvSchedule, NamedMultiplierSchedule};

        let mut args = reth_research::ResearchArgs::new()
            .with_db_path(self.db_path.clone())
            .with_start_block(self.start_block);

        if self.eip2780 {
            args = args.with_eip2780();
        }

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
        ])
        .args;
        assert!(args.eip2780);
        assert_eq!(args.csv_schedules, vec!["7904-prelim=./7904.csv"]);
        assert_eq!(args.multiplier_schedules, vec!["128x=128"]);
        assert_eq!(args.db_path, PathBuf::from("./results.db"));
        assert_eq!(args.start_block, 18000000);
        assert_eq!(args.schedule_count(), 3);
    }
}
