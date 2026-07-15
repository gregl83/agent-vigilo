//! Root command router for the `vigilo` CLI.
//!
//! This module owns the top-level command enum used by clap and delegates
//! execution to feature-specific command modules (`run`, `evaluators`,
//! `coordinator`, `worker`, `shard`, `setup`).

use async_trait::async_trait;
use clap::Subcommand;

use crate::context::Context;

pub(super) mod coordinator;
pub(super) mod evaluators;
pub(super) mod run;
pub(super) mod setup;
pub(super) mod shard;
pub(super) mod worker;

use super::{
    Executable,
    args,
};

#[derive(Debug, Subcommand)]
/// Top-level CLI commands exposed by `vigilo`.
///
/// Each variant wraps a subcommand module that implements [`Executable`].
pub(crate) enum Command {
    /// Run system setup (install or upgrade)
    Setup(setup::Command),

    /// Manage system evaluators
    Evaluators(evaluators::Command),

    /// Run profiles and datasets
    Run(run::Command),

    /// Run coordinator processes
    Coordinator(coordinator::Command),

    /// Run worker processes
    Worker(worker::Command),

    /// Manage shard database placements
    Shard(shard::Command),
}

#[async_trait]
impl Executable for Command {
    /// Dispatches the selected top-level command to its concrete handler.
    async fn exec(self, context: Context) -> anyhow::Result<()> {
        match self {
            Command::Coordinator(cmd) => cmd.exec(context).await,
            Command::Evaluators(cmd) => cmd.exec(context).await,
            Command::Run(cmd) => cmd.exec(context).await,
            Command::Shard(cmd) => cmd.exec(context).await,
            Command::Setup(cmd) => cmd.exec(context).await,
            Command::Worker(cmd) => cmd.exec(context).await,
        }
    }
}
