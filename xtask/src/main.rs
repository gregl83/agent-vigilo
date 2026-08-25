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
    /// Create verified test subjects, execute workloads, and detect Vigilo regressions.
    Perf(Box<perf::PerfArgs>),
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
        Command::Perf(args) => perf::run(*args),
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
    use clap::CommandFactory;

    use super::*;

    fn command_help(path: &[&str]) -> String {
        let mut root = Cli::command();
        let mut command = &mut root;
        for name in path {
            command = command
                .find_subcommand_mut(name)
                .unwrap_or_else(|| panic!("missing command in help contract: {name}"));
        }
        command.render_help().to_string()
    }

    fn assert_visible_help_is_documented(command: &clap::Command) {
        assert!(
            command.get_about().is_some(),
            "{} is missing a use-case summary",
            command.get_name()
        );
        for argument in command.get_arguments().filter(|argument| {
            argument.get_long().is_some() && argument.get_id().as_str() != "help"
        }) {
            assert!(
                argument.get_help().is_some(),
                "{} --{} is missing help",
                command.get_name(),
                argument.get_long().unwrap()
            );
        }
        for subcommand in command
            .get_subcommands()
            .filter(|subcommand| subcommand.get_name() != "help")
        {
            assert_visible_help_is_documented(subcommand);
        }
    }

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

    #[test]
    fn performance_help_explains_operator_use_cases_and_every_option() {
        let root = Cli::command();
        let perf = root.find_subcommand("perf").unwrap();
        assert_visible_help_is_documented(perf);

        assert!(command_help(&["perf", "build"]).contains("test subject"));
        assert!(command_help(&["perf", "run"]).contains("does not compare revisions"));
        assert!(command_help(&["perf", "compare"]).contains("Regression-test"));
    }
}
