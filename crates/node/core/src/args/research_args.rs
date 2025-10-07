//! clap [Args](clap::Args) for gas price research mode

use clap::Args;
use std::path::PathBuf;

/// Parameters for gas price research mode
#[derive(Debug, Clone, Args, PartialEq)]
#[command(next_help_heading = "Research")]
pub struct ResearchArgs {
    /// Enable gas price research mode
    #[arg(long = "research.enabled", help_heading = "Research")]
    pub enabled: bool,

    /// Gas cost multiplier for research mode
    #[arg(long = "research.gas-multiplier", default_value_t = 128, help_heading = "Research")]
    pub gas_multiplier: u64,

    /// Block number to start research analysis
    #[arg(long = "research.start-block", default_value_t = 0, help_heading = "Research")]
    pub start_block: u64,

    /// Path to divergence database file
    #[arg(
        long = "research.db-path",
        default_value = "./divergence.db",
        help_heading = "Research"
    )]
    pub db_path: PathBuf,

    /// Refund multiplier for research mode
    #[arg(long = "research.refund-multiplier", default_value_t = 128.0, help_heading = "Research")]
    pub refund_multiplier: f64,

    /// Stipend multiplier for research mode
    #[arg(long = "research.stipend-multiplier", default_value_t = 128.0, help_heading = "Research")]
    pub stipend_multiplier: f64,
}

impl Default for ResearchArgs {
    fn default() -> Self {
        Self {
            enabled: false,
            gas_multiplier: 128,
            start_block: 0,
            db_path: PathBuf::from("./divergence.db"),
            refund_multiplier: 128.0,
            stipend_multiplier: 128.0,
        }
    }
}

#[cfg(feature = "research")]
impl ResearchArgs {
    /// Converts ResearchArgs into a ResearchConfig
    pub fn to_research_config(&self) -> reth_research::config::ResearchConfig {
        reth_research::config::ResearchConfig {
            gas_multiplier: self.gas_multiplier,
            start_block: self.start_block,
            refund_multiplier: self.refund_multiplier,
            stipend_multiplier: self.stipend_multiplier,
            divergence_db_path: self.db_path.clone(),
            ..Default::default()
        }
    }

    /// Opens the divergence database
    pub fn open_database(&self) -> Result<reth_research::database::DivergenceDatabase, reth_research::database::DatabaseError> {
        reth_research::database::DivergenceDatabase::open(&self.db_path)
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
    fn test_parse_research_args() {
        let args = CommandParser::<ResearchArgs>::parse_from(["reth"]).args;
        assert_eq!(
            args,
            ResearchArgs {
                enabled: false,
                gas_multiplier: 128,
                start_block: 0,
                db_path: PathBuf::from("./divergence.db"),
                refund_multiplier: 128.0,
                stipend_multiplier: 128.0,
            }
        );
    }

    #[test]
    fn research_args_default_sanity_test() {
        let default_args = ResearchArgs::default();
        let args = CommandParser::<ResearchArgs>::parse_from(["reth"]).args;
        assert_eq!(args, default_args);
    }

    #[test]
    fn test_parse_research_args_enabled() {
        let args = CommandParser::<ResearchArgs>::parse_from([
            "reth",
            "--research.enabled",
            "--research.gas-multiplier",
            "256",
            "--research.start-block",
            "18000000",
        ])
        .args;
        assert_eq!(
            args,
            ResearchArgs {
                enabled: true,
                gas_multiplier: 256,
                start_block: 18000000,
                db_path: PathBuf::from("./divergence.db"),
                refund_multiplier: 128.0,
                stipend_multiplier: 128.0,
            }
        );
    }
}
