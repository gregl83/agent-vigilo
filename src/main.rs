use std::io::stderr;

use clap::Parser;

mod cli;
use cli::{
    App,
    Executable,
};
use tracing::{
    error,
    Level,
};
use tracing_subscriber::{
    EnvFilter,
    fmt,
    prelude::*,
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

    tracing_subscriber::registry()
        .with(
            fmt::layer()
                .with_writer(stderr)
                .with_target(false)
        )
        .with(
            EnvFilter::from_default_env()
                .add_directive(level.into())
        )
        .init();
}

#[tokio::main]
async fn main() {
    let cli = App::parse();

    init_logger(cli.quiet, cli.verbose);

    match cli.exec().await {
        Ok(_) => (),
        Err(e) => {
            error!("{}", e);
        },
    }
}
