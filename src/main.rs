use clap::Parser;

mod cli;

use cli::{
    App,
    Executable,
};

#[tokio::main]
async fn main() {
    // todo - bootstrap logger/output

    let cli = App::parse();
    let result = cli.exec().await;

    // todo - do something with result
}
