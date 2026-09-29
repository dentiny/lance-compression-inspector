mod output;

use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, ValueEnum};
use lance_compression_estimation::{
    AnalyzeOptions, DEFAULT_SAMPLE_ROWS, analyze_dataset_with_options, probe::probe_local_dataset,
};

#[derive(Debug, Parser)]
#[command(
    name = "lance-compression",
    about = "Inspect a Lance dataset snapshot for compression opportunities"
)]
struct Args {
    /// Local Lance dataset directory.
    dataset: PathBuf,

    /// Dataset branch. Defaults to main.
    #[arg(long, default_value = "main")]
    branch: String,

    /// Dataset manifest version. Defaults to latest on the selected branch.
    #[arg(long)]
    version: Option<u64>,

    /// Maximum physical rows sampled per active data file for each encoding plan.
    #[arg(long, default_value_t = DEFAULT_SAMPLE_ROWS)]
    sample_rows: usize,

    /// Include decode-cost multipliers when ranking encoding plans.
    #[arg(long)]
    consider_decoding_penalty: bool,

    /// Show every measured plan, score, and evidence item.
    #[arg(long)]
    verbose: bool,

    /// Report format.
    #[arg(long, value_enum, default_value_t = Output::Human)]
    output: Output,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum Output {
    Human,
    Json,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let probe =
        probe_local_dataset(&args.dataset, &args.branch, args.version, args.sample_rows).await?;
    let report = analyze_dataset_with_options(
        probe,
        AnalyzeOptions {
            consider_decoding_penalty: args.consider_decoding_penalty,
        },
    );
    match args.output {
        Output::Human => output::print_human(&report, args.verbose),
        Output::Json => println!("{}", serde_json::to_string_pretty(&report)?),
    }
    Ok(())
}
