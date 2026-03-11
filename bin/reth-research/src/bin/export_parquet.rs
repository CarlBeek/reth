use clap::Parser;
use reth_research::export::export_sqlite_to_parquet;
use std::path::PathBuf;

#[derive(Debug, Parser)]
struct ExportArgs {
    /// Source SQLite divergence database
    #[arg(long)]
    db_path: PathBuf,
    /// Output directory for Parquet datasets
    #[arg(long)]
    out_dir: PathBuf,
    /// Max rows per Arrow/Parquet batch
    #[arg(long, default_value_t = 50_000)]
    row_group_size: usize,
    /// Number of blocks per partition bucket
    #[arg(long, default_value_t = 100_000)]
    block_bucket_size: u64,
    /// Export the full database instead of appending from the checkpoint
    #[arg(long, default_value_t = false)]
    full_refresh: bool,
}

fn main() -> eyre::Result<()> {
    let args = ExportArgs::parse();
    let stats = export_sqlite_to_parquet(
        &args.db_path,
        &args.out_dir,
        args.row_group_size,
        args.block_bucket_size,
        !args.full_refresh,
    )?;
    println!(
        "exported coverage_rows={} hot_rows={} artifact_rows={} schedules={} incremental={} bucket_size={} out_dir={}",
        stats.coverage_rows,
        stats.hot_rows,
        stats.artifact_rows,
        stats.schedules.len(),
        stats.incremental,
        stats.block_bucket_size,
        stats.output_dir.display()
    );
    Ok(())
}
