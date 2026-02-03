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

    /// Path to CSV file with gas pricing data (EIP-7904 format)
    #[arg(
        long = "research.gas-pricing-csv",
        default_value = "./7904_prelim_numbers.csv",
        help_heading = "Research"
    )]
    pub gas_pricing_csv: PathBuf,

    /// Block number to start research analysis
    #[arg(long = "research.start-block", default_value_t = 0, help_heading = "Research")]
    pub start_block: u64,

    /// Path to divergence database file
    #[arg(long = "research.db-path", default_value = "./divergence.db", help_heading = "Research")]
    pub db_path: PathBuf,
}

impl Default for ResearchArgs {
    fn default() -> Self {
        Self {
            enabled: false,
            gas_pricing_csv: PathBuf::from("./7904_prelim_numbers.csv"),
            start_block: 0,
            db_path: PathBuf::from("./divergence.db"),
        }
    }
}

#[cfg(feature = "research")]
impl ResearchArgs {
    /// Converts ResearchArgs into a ResearchConfig
    pub fn to_research_config(
        &self,
    ) -> Result<reth_research::config::ResearchConfig, reth_research::gas_pricing::GasPricingError>
    {
        let gas_pricing =
            reth_research::gas_pricing::GasPricingTable::from_csv_path(&self.gas_pricing_csv)?;

        Ok(reth_research::config::ResearchConfig {
            gas_pricing,
            start_block: self.start_block,
            divergence_db_path: self.db_path.clone(),
            ..Default::default()
        })
    }

    /// Opens the divergence database
    pub fn open_database(
        &self,
    ) -> Result<reth_research::database::DivergenceDatabase, reth_research::database::DatabaseError>
    {
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
                gas_pricing_csv: PathBuf::from("./7904_prelim_numbers.csv"),
                start_block: 0,
                db_path: PathBuf::from("./divergence.db"),
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
            "--research.gas-pricing-csv",
            "/path/to/pricing.csv",
            "--research.start-block",
            "18000000",
        ])
        .args;
        assert_eq!(
            args,
            ResearchArgs {
                enabled: true,
                gas_pricing_csv: PathBuf::from("/path/to/pricing.csv"),
                start_block: 18000000,
                db_path: PathBuf::from("./divergence.db"),
            }
        );
    }
}
