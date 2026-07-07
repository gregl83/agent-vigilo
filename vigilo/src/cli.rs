//! CLI application definition.
//!
//! This module defines global options, shared output selection, and the
//! `Executable` dispatch contract used by subcommands. Command implementations
//! should live under `cli::commands`; this root should only own arguments that
//! apply process-wide and values needed to construct [`crate::context::Context`].

//! CLI application definition.
//!
//! This module defines global options, shared output selection, and the
//! `Executable` dispatch contract used by subcommands. Command implementations
//! should live under `cli::commands`; this root should only own arguments that
//! apply process-wide and values needed to construct [`crate::context::Context`].

use async_trait::async_trait;
use clap::{
    ArgAction,
    Parser,
    crate_description,
    crate_version,
};

mod args;
mod commands;
use commands::Command;

use super::context::{
    Context,
    output::OutputFormat,
};

#[async_trait]
pub(super) trait Executable {
    async fn exec(self, context: Context) -> anyhow::Result<()>;
}

#[derive(Debug, Parser)]
#[command(
    name = "agent-vigilo",
    version = crate_version!(),
    about = crate_description!(),
    long_about = None,
    after_help = "Agent tip: use `-q -f toon` for compact structured output in AI agent and LLM tool-call workflows. Use `-f json` when exact JSON parsing is required."
)]
pub(crate) struct App {
    /// Database URL (connection string)
    #[arg(long, env = "DATABASE_URL")]
    pub database_url: String,

    /// Maximum Postgres connections for this process
    #[arg(long, env = "DATABASE_MAX_CONNECTIONS", default_value_t = 5, value_parser = clap::value_parser!(u32).range(1..=256))]
    pub database_max_connections: u32,

    /// Messaging URL (connection string)
    #[arg(long, env = "MESSAGING_URL")]
    pub messaging_url: String,

    /// Maximum linear memory bytes per Wasm evaluator invocation
    #[arg(long, env = "VIGILO_WASM_MAX_MEMORY_BYTES", default_value_t = 67_108_864, value_parser = clap::value_parser!(u64).range(65_536..=1_073_741_824))]
    pub wasm_max_memory_bytes: u64,

    /// Maximum table elements per Wasm evaluator invocation
    #[arg(long, env = "VIGILO_WASM_MAX_TABLE_ELEMENTS", default_value_t = 10_000, value_parser = clap::value_parser!(u64).range(1..=10_000_000))]
    pub wasm_max_table_elements: u64,

    /// Maximum component instances per Wasm evaluator invocation
    #[arg(long, env = "VIGILO_WASM_MAX_INSTANCES", default_value_t = 1, value_parser = clap::value_parser!(u64).range(1..=1024))]
    pub wasm_max_instances: u64,

    /// Maximum linear memories per Wasm evaluator invocation
    #[arg(long, env = "VIGILO_WASM_MAX_MEMORIES", default_value_t = 1, value_parser = clap::value_parser!(u64).range(1..=64))]
    pub wasm_max_memories: u64,

    /// Maximum tables per Wasm evaluator invocation
    #[arg(long, env = "VIGILO_WASM_MAX_TABLES", default_value_t = 2, value_parser = clap::value_parser!(u64).range(1..=256))]
    pub wasm_max_tables: u64,

    /// Fuel budget per Wasm evaluator invocation
    #[arg(long, env = "VIGILO_WASM_FUEL_PER_EVALUATION", default_value_t = 50_000_000, value_parser = clap::value_parser!(u64).range(1..=10_000_000_000))]
    pub wasm_fuel_per_evaluation: u64,

    /// Wall-clock timeout in milliseconds per Wasm evaluator invocation
    #[arg(long, env = "VIGILO_WASM_TIMEOUT_MS", default_value_t = 5_000, value_parser = clap::value_parser!(u64).range(1..=600_000))]
    pub wasm_timeout_ms: u64,

    /// Epoch ticker interval in milliseconds used for Wasm timeout traps
    #[arg(long, env = "VIGILO_WASM_EPOCH_TICK_INTERVAL_MS", default_value_t = 10, value_parser = clap::value_parser!(u64).range(1..=1_000))]
    pub wasm_epoch_tick_interval_ms: u64,

    /// Maximum active Wasm evaluator executions per process
    #[arg(long, env = "VIGILO_WASM_MAX_CONCURRENT_EVALUATIONS", default_value_t = 8, value_parser = clap::value_parser!(u64).range(1..=1024))]
    pub wasm_max_concurrent_evaluations: u64,

    /// Maximum bytes logged per evaluator host log message
    #[arg(long, env = "VIGILO_WASM_MAX_LOG_MESSAGE_BYTES", default_value_t = 4_096, value_parser = clap::value_parser!(u64).range(1..=1_048_576))]
    pub wasm_max_log_message_bytes: u64,

    /// Maximum evaluator host log messages per invocation
    #[arg(long, env = "VIGILO_WASM_MAX_LOG_MESSAGES", default_value_t = 128, value_parser = clap::value_parser!(u32).range(0..=100_000))]
    pub wasm_max_log_messages: u32,

    /// Suppress all diagnostic output and progress messages
    #[arg(global = true, short, long, default_value_t = false)]
    pub quiet: bool,

    /// Increase log verbosity (-v for DEBUG, -vv for TRACE)
    #[arg(global = true, short, long, action = ArgAction::Count)]
    pub verbose: u8,

    /// Output encoding for stdout payloads; agents should prefer `-q -f toon` for compact inspection
    #[arg(
        global = true,
        short = 'f',
        long = "output-format",
        alias = "format",
        env = "VIGILO_OUTPUT_FORMAT",
        value_name = "FORMAT",
        default_value_t = OutputFormat::Json,
        value_enum
    )]
    pub output_format: OutputFormat,

    #[command(subcommand)]
    pub command: Command,
}

#[async_trait]
impl Executable for App {
    async fn exec(self, context: Context) -> anyhow::Result<()> {
        self.command.exec(context).await
    }
}
