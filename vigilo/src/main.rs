use std::{
    io::stderr,
    process::ExitCode,
};

use clap::Parser;
use tracing::{
    Level,
    error,
};
use tracing_subscriber::{
    EnvFilter,
    Registry,
    fmt,
    prelude::*,
};

mod agent_client;
mod cli;
use cli::{
    App,
    Executable,
};
mod context;
use context::{
    Context,
    wasm,
};
mod contracts;
mod db;
mod evaluators;
mod manifest;
mod models;
mod mq;
mod outbox;
mod runtime;

fn init_logger(quiet: bool, verbose: u8) {
    let level = if quiet {
        Level::ERROR
    } else {
        match verbose {
            0 => Level::INFO,
            1 => Level::DEBUG,
            _ => Level::TRACE,
        }
    };

    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new(""))
        .add_directive(level.into());

    let subscriber = Registry::default()
        .with(fmt::layer().with_writer(stderr).with_target(false))
        .with(filter);

    let _ = subscriber.try_init();
}

#[tokio::main]
async fn main() -> ExitCode {
    let app_parse_result = App::try_parse();

    match app_parse_result {
        Ok(app) => {
            init_logger(app.quiet, app.verbose);

            let wasm_config = wasm::Config {
                max_memory_bytes: app.wasm_max_memory_bytes,
                max_table_elements: app.wasm_max_table_elements,
                max_instances: app.wasm_max_instances,
                max_memories: app.wasm_max_memories,
                max_tables: app.wasm_max_tables,
                fuel_per_evaluation: app.wasm_fuel_per_evaluation,
                timeout_ms: app.wasm_timeout_ms,
                epoch_tick_interval_ms: app.wasm_epoch_tick_interval_ms,
                max_concurrent_evaluations: app.wasm_max_concurrent_evaluations,
                max_log_message_bytes: app.wasm_max_log_message_bytes,
                max_log_messages: app.wasm_max_log_messages,
            };
            let context = Context::new(
                app.database_url.clone(),
                app.database_max_connections,
                app.messaging_url.clone(),
                wasm_config,
                app.output_format,
            );

            match app.exec(context).await {
                Err(e) => {
                    error!(error = %e, "command execution failed");
                    ExitCode::FAILURE
                }
                Ok(()) => ExitCode::SUCCESS,
            }
        }
        Err(e) => {
            init_logger(false, 0);

            if !e.use_stderr() {
                _ = e.print();
                return ExitCode::SUCCESS;
            }

            for (kind, value) in e.context() {
                if let Some(kind) = kind.as_str()
                    && !kind.is_empty()
                {
                    error!("command failed: {}: {}", kind, value);
                }
            }

            ExitCode::from(e.exit_code() as u8)
        }
    }
}
