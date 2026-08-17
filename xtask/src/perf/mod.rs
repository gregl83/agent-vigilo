//! External performance-harness command surface.
//!
//! The harness builds immutable release snapshots, resolves versioned workload
//! profiles, executes single-binary or counterbalanced comparison campaigns,
//! and writes machine-readable artifacts. It treats unsupported or incomplete
//! campaigns as invalid results instead of silently reducing their scope.

mod artifact;
mod build;
mod check;
mod command;
mod config;
mod model;
mod process;
mod report;
mod schedule;
mod stats;

use std::{
    path::PathBuf,
    thread,
    time::Duration,
};

use anyhow::Result;
use clap::{
    Args,
    Subcommand,
};

/// Exit status for a completed campaign with no failing gate.
pub const EXIT_PASS: u8 = 0;
/// Exit status for a correctness failure or confirmed performance regression.
pub const EXIT_REGRESSION: u8 = 1;
/// Exit status for an unsupported, invalid, inconclusive, or incomplete result.
pub const EXIT_INVALID: u8 = 2;

/// Arguments for the `cargo perf` command family.
#[derive(Debug, Args)]
pub struct PerfArgs {
    #[command(subcommand)]
    command: PerfCommand,
}

#[derive(Debug, Subcommand)]
enum PerfCommand {
    /// Validate harness contracts and isolation boundaries.
    Check(check::CheckArgs),
    /// Build and snapshot a release binary with provenance.
    Build(build::BuildArgs),
    /// Measure one release binary without a regression verdict.
    Run(command::RunArgs),
    /// Compare baseline and candidate release binaries.
    Compare(command::CompareArgs),
    /// Re-render a completed run's terminal and Markdown summary.
    Report(report::ReportArgs),
}

/// Arguments for the hidden subprocess fixture used by lifecycle tests.
#[derive(Debug, Args)]
pub struct FixtureArgs {
    #[arg(long, default_value_t = 0)]
    delay_ms: u64,
    #[arg(long, default_value_t = 0)]
    output_bytes: usize,
    #[arg(long, default_value_t = 0)]
    exit_code: i32,
}

/// Dispatches a parsed performance-harness command and returns its process code.
pub fn run(args: PerfArgs) -> Result<u8> {
    match args.command {
        PerfCommand::Check(args) => check::execute(args),
        PerfCommand::Build(args) => build::execute(args),
        PerfCommand::Run(args) => command::run_single(args),
        PerfCommand::Compare(args) => command::compare(args),
        PerfCommand::Report(args) => report::execute(args),
    }
}

/// Runs the hidden deterministic subprocess fixture used by harness self-tests.
pub fn run_fixture(args: FixtureArgs) -> Result<u8> {
    thread::sleep(Duration::from_millis(args.delay_ms));
    if args.output_bytes > 0 {
        println!("{}", "x".repeat(args.output_bytes));
    }
    Ok(args.exit_code.clamp(0, u8::MAX as i32) as u8)
}

fn default_profile_dir() -> PathBuf {
    PathBuf::from("performance/profiles")
}
