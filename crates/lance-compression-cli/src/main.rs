mod output;

use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, ValueEnum};
use lance_compression_estimation::{
    AnalyzeOptions, analyze_dataset_with_options, probe::probe_local_dataset,
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

    /// Include decode-cost multipliers when ranking compression candidates.
    #[arg(long)]
    consider_decoding_penalty: bool,

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
    let probe = probe_local_dataset(&args.dataset, &args.branch, args.version).await?;
    let report = analyze_dataset_with_options(
        probe,
        AnalyzeOptions {
            consider_decoding_penalty: args.consider_decoding_penalty,
        },
    );
    match args.output {
        Output::Human => output::print_human(&report),
        Output::Json => println!("{}", serde_json::to_string_pretty(&report)?),
    }
    Ok(())
}
