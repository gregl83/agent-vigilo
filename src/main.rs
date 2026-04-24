use std::{
    io::stderr,
    process::ExitCode,
};

use clap::Parser;

mod cli;
use cli::{
    App,
    Executable,
};

mod adapters;
mod context;
use context::Context;
use tracing::{
    error,
    Level,
};
use tracing_subscriber::{
    EnvFilter,
    fmt,
    prelude::*,
    Registry,
};


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
        .with(
            fmt::layer()
                .with_writer(stderr)
                .with_target(false)
        )
        .with(filter);

    let _ = subscriber.try_init();
}

#[tokio::main]
async fn main() -> ExitCode {
    let app_parse_result = App::try_parse();

    match app_parse_result {
        Ok(app) => {
            init_logger(app.quiet, app.verbose);

            let context = Context::new(
                app.database_uri.clone(),
            );

            if let Err(e) = app.exec(context).await {
                error!(error = %e, "command execution failed");
                return ExitCode::FAILURE;
            }

            ExitCode::SUCCESS
        }
        Err(e) => {
            init_logger(false, 0);

            if !e.use_stderr() {
                _ = e.print();
                return ExitCode::SUCCESS;
            }
            let multi_line_error_msg = e.render().to_string();
            let error_msg = multi_line_error_msg.lines()
                .next()
                .unwrap_or("unexpected error message");
            error!(error = error_msg);

            ExitCode::from(e.exit_code() as u8)
        }
    }
}
