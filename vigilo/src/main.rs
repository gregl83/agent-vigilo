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

            let wasm_config = wasm::Config::default();
            let context = Context::new(
                app.database_url.clone(),
                app.messaging_url.clone(),
                wasm_config,
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
                if let Some(kind) = kind.as_str() {
                    if !kind.is_empty() {
                        error!("command failed: {}: {}", kind, value);
                    }
                }
            }

            ExitCode::from(e.exit_code() as u8)
        }
    }
}
