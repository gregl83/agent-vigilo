//! Repository-local development tooling for the Vigilo workspace.
//!
//! The binary currently hosts the external performance harness. It remains a
//! separate workspace package so benchmark orchestration, fixtures, and report
//! dependencies cannot enter the production `vigilo` dependency graph.
#![warn(missing_docs)]

mod perf;

use std::process::ExitCode;

use clap::{
    Parser,
    Subcommand,
};

#[derive(Debug, Parser)]
#[command(
    name = "xtask",
    version,
    about = "Repository-local development tooling"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Build, run, and compare Vigilo performance workloads.
    Perf(perf::PerfArgs),
    #[command(name = "__perf-fixture", hide = true)]
    PerfFixture(perf::FixtureArgs),
    #[command(name = "__perf-service-fixture", hide = true)]
    PerfServiceFixture,
}

fn main() -> ExitCode {
    execute(Cli::parse().command)
}

fn execute(command: Command) -> ExitCode {
    let result = match command {
        Command::Perf(args) => perf::run(args),
        Command::PerfFixture(args) => perf::run_fixture(args),
        Command::PerfServiceFixture => perf::run_service_fixture(),
    };

    match result {
        Ok(code) => ExitCode::from(code),
        Err(error) => {
            eprintln!("error: {error:#}");
            ExitCode::from(perf::EXIT_INVALID)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dispatcher_propagates_fixture_exit_codes() {
        let success = Cli::try_parse_from(["xtask", "__perf-fixture"]).unwrap();
        assert_eq!(execute(success.command), ExitCode::SUCCESS);

        let failure = Cli::try_parse_from([
            "xtask",
            "__perf-fixture",
            "--exit-code",
            "7",
            "--output-bytes",
            "1",
        ])
        .unwrap();
        assert_eq!(execute(failure.command), ExitCode::from(7));
    }

    #[test]
    fn dispatcher_maps_command_errors_to_invalid() {
        let invalid = Cli::try_parse_from([
            "xtask",
            "perf",
            "report",
            "--run-dir",
            "target/perf/runs/does-not-exist",
        ])
        .unwrap();
        assert_eq!(execute(invalid.command), ExitCode::from(perf::EXIT_INVALID));
    }
}
