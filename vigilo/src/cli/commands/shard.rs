//! Database placement, run-shard, and rebalance administration commands.
//!
//! The public CLI exposes these resources at their ownership boundaries:
//! databases are global, run shards belong to runs, and rebalances span runs.
//! Legacy `vigilo shard ...` paths translate into the same handlers.

use async_trait::async_trait;
use clap::{
    Args,
    Subcommand,
};
use serde_json::{
    Value,
    json,
};
use tracing::{
    info,
    warn,
};
use uuid::Uuid;

use super::Executable;
use crate::{
    context::Context,
    db::workflows::shard_admin,
    models::{
        database_placement::DatabasePlacement,
        shard_placement::ShardPlacement,
    },
};

const REBALANCE_LEASE_SECONDS: i32 = 300;

#[derive(Debug, Subcommand)]
/// Shard placement administration operations.
pub(crate) enum SubCommand {
    /// Manage configured database placements
    Databases {
        #[command(subcommand)]
        command: DatabaseSubCommand,
    },

    /// Manage run-shard placement rows
    Placements {
        #[command(subcommand)]
        command: PlacementSubCommand,
    },

    /// Inspect the resolved route for one run shard
    Route {
        /// Run UUID
        run_id: String,

        /// Logical run shard
        #[arg(value_parser = clap::value_parser!(i16).range(0..=127))]
        run_shard: i16,
    },

    /// Move one run shard to another database placement
    Move {
        /// Run UUID
        run_id: String,

        /// Logical run shard
        #[arg(value_parser = clap::value_parser!(i16).range(0..=127))]
        run_shard: i16,

        /// Target database placement alias
        #[arg(long, alias = "to")]
        alias: String,

        /// Validate and report the move plan without writing data
        #[arg(long, default_value_t = false, conflicts_with = "verify_only")]
        dry_run: bool,

        /// Verify source and target shard rows without copying or switching placement
        #[arg(long, default_value_t = false)]
        verify_only: bool,

        /// Compatibility flag; active work is always protected by the shard write fence
        #[arg(long, default_value_t = false)]
        force: bool,
    },

    /// Restore an in-progress shard move to its source placement
    MoveAbort {
        /// Run UUID
        run_id: String,

        /// Logical run shard
        #[arg(value_parser = clap::value_parser!(i16).range(0..=127))]
        run_shard: i16,

        /// Expected source database placement alias
        #[arg(long, alias = "from")]
        source: String,

        /// Expected target database placement alias
        #[arg(long = "to", alias = "target")]
        target: String,
    },

    /// Plan, apply, verify, or cancel bulk shard movement
    Rebalance {
        #[command(subcommand)]
        command: RebalanceSubCommand,
    },
}

#[derive(Debug, Subcommand)]
/// Database placement operations.
pub(crate) enum DatabaseSubCommand {
    /// List database placements
    List,

    /// Register an active shard database placement
    #[command(alias = "add")]
    Register {
        /// Stable placement alias
        alias: String,

        /// Environment variable containing this placement's database URL
        #[arg(long, value_name = "ENV")]
        database_url_env: String,

        /// Add the placement even when database_url_env is not set in this process
        #[arg(long, default_value_t = false)]
        defer_env_validation: bool,
    },

    /// Stop new ownership unless an in-flight move targets this placement
    Drain {
        /// Stable placement alias
        alias: String,
    },

    /// Disable a non-control database placement
    ///
    /// The placement must be draining, own no routes, and be no move target.
    Disable {
        /// Stable placement alias
        alias: String,
    },
}

#[derive(Debug, Subcommand)]
/// Run-shard placement operations.
pub(crate) enum PlacementSubCommand {
    /// List placements for a run
    List {
        /// Run UUID
        run_id: String,
    },

    /// Show placement for one run shard
    Show {
        /// Run UUID
        run_id: String,

        /// Logical run shard
        #[arg(value_parser = clap::value_parser!(i16).range(0..=127))]
        run_shard: i16,
    },

    /// Set placement for an empty or new run shard
    Set {
        /// Run UUID
        run_id: String,

        /// Logical run shard
        #[arg(value_parser = clap::value_parser!(i16).range(0..=127))]
        run_shard: i16,

        /// Target database placement alias
        #[arg(long)]
        alias: String,
    },
}

#[derive(Debug, Subcommand)]
/// Bulk shard rebalance operations.
pub(crate) enum RebalanceSubCommand {
    /// Create a persisted rebalance plan
    Plan {
        /// Optional source placement to drain
        #[arg(long)]
        from: Option<String>,

        /// Target database placement alias
        #[arg(long = "to", alias = "target")]
        target: String,

        /// Maximum items to include in this plan
        #[arg(long, default_value_t = 100)]
        max_items: usize,

        /// Build and print the plan without persisting it
        #[arg(long, default_value_t = false)]
        dry_run: bool,
    },

    /// Apply pending items from a persisted rebalance plan
    Apply {
        /// Rebalance operation UUID
        operation_id: String,

        /// Maximum pending items to apply in this pass
        #[arg(long, default_value_t = 25)]
        max_items: usize,

        /// Seconds before an interrupted item claim can be recovered
        #[arg(long, env = "VIGILO_REBALANCE_LEASE_SECONDS", default_value_t = REBALANCE_LEASE_SECONDS, value_parser = clap::value_parser!(i32).range(1..=86400))]
        lease_seconds: i32,

        /// Compatibility flag; active work is always protected by the shard write fence
        #[arg(long, default_value_t = false)]
        force: bool,
    },

    /// Verify completed items from a persisted rebalance plan
    Verify {
        /// Rebalance operation UUID
        operation_id: String,

        /// Maximum completed items to verify in this pass
        #[arg(long, default_value_t = 25)]
        max_items: usize,
    },

    /// Cancel unapplied items in a persisted rebalance plan
    Cancel {
        /// Rebalance operation UUID
        operation_id: String,
    },
}

#[derive(Debug, Args)]
/// Global database placement administration.
#[command(
    after_help = "Tip: draining stops new ownership but does not move existing shards. Use `vigilo rebalance` to evacuate the database before disabling it."
)]
pub(crate) struct DatabaseCommand {
    #[command(subcommand)]
    pub(crate) command: DatabaseSubCommand,
}

#[derive(Debug, Subcommand)]
/// Operations on shards owned by one run.
pub(crate) enum RunShardSubCommand {
    /// List shard assignments for a run
    List {
        /// Run UUID
        run_id: String,
    },

    /// Show the resolved route for one run shard
    Show {
        /// Run UUID
        run_id: String,

        /// Logical run shard
        #[arg(value_parser = clap::value_parser!(i16).range(0..=127))]
        run_shard: i16,
    },

    /// Assign an empty or new run shard to a database
    Assign {
        /// Run UUID
        run_id: String,

        /// Logical run shard
        #[arg(value_parser = clap::value_parser!(i16).range(0..=127))]
        run_shard: i16,

        /// Target database placement alias
        #[arg(long = "to")]
        target: String,
    },

    /// Move one run shard to another database
    Move {
        /// Run UUID
        run_id: String,

        /// Logical run shard
        #[arg(value_parser = clap::value_parser!(i16).range(0..=127))]
        run_shard: i16,

        /// Target database placement alias
        #[arg(long = "to", alias = "alias")]
        target: String,

        /// Validate and report the move plan without writing data
        #[arg(long, default_value_t = false, conflicts_with = "verify_only")]
        dry_run: bool,

        /// Verify source and target shard rows without copying or switching placement
        #[arg(long, default_value_t = false)]
        verify_only: bool,

        /// Compatibility flag; active work is always protected by the shard write fence
        #[arg(long, default_value_t = false)]
        force: bool,
    },

    /// Restore an in-progress move to its source database
    AbortMove {
        /// Run UUID
        run_id: String,

        /// Logical run shard
        #[arg(value_parser = clap::value_parser!(i16).range(0..=127))]
        run_shard: i16,

        /// Expected source database placement alias
        #[arg(long = "from", alias = "source")]
        source: String,

        /// Expected target database placement alias
        #[arg(long = "to", alias = "target")]
        target: String,
    },
}

#[derive(Debug, Args)]
/// Run-shard routing administration.
#[command(
    after_help = "Tip: use `assign` only for an empty or new shard; use `move` when the shard owns data."
)]
pub(crate) struct RunShardCommand {
    #[command(subcommand)]
    pub(crate) command: RunShardSubCommand,
}

#[derive(Debug, Args)]
/// Cross-run shard rebalance administration.
#[command(
    after_help = "Tip: use plan, apply, and verify to evacuate a draining database before disabling it."
)]
pub(crate) struct RebalanceCommand {
    #[command(subcommand)]
    pub(crate) command: RebalanceSubCommand,
}

#[derive(Debug, Args)]
/// Deprecated `vigilo shard` compatibility commands.
pub(crate) struct Command {
    #[command(subcommand)]
    pub command: SubCommand,
}

#[async_trait]
impl Executable for Command {
    async fn exec(self, context: Context) -> anyhow::Result<()> {
        warn!(
            "the `vigilo shard` command group is deprecated; use `vigilo database`, \
             `vigilo run shard`, or `vigilo rebalance`"
        );
        match self.command {
            SubCommand::Databases { command } => exec_database_command(context, command).await,
            SubCommand::Move {
                run_id,
                run_shard,
                alias,
                dry_run,
                verify_only,
                force,
            } => {
                exec_move_command(
                    context,
                    run_id,
                    run_shard,
                    alias,
                    dry_run,
                    verify_only,
                    force,
                )
                .await
            }
            SubCommand::MoveAbort {
                run_id,
                run_shard,
                source,
                target,
            } => exec_move_abort_command(context, run_id, run_shard, source, target).await,
            SubCommand::Placements { command } => exec_placement_command(context, command).await,
            SubCommand::Rebalance { command } => exec_rebalance_command(context, command).await,
            SubCommand::Route { run_id, run_shard } => {
                exec_route_command(context, run_id, run_shard).await
            }
        }
    }
}

#[async_trait]
impl Executable for DatabaseCommand {
    async fn exec(self, context: Context) -> anyhow::Result<()> {
        exec_database_command(context, self.command).await
    }
}

#[async_trait]
impl Executable for RunShardCommand {
    async fn exec(self, context: Context) -> anyhow::Result<()> {
        match self.command {
            RunShardSubCommand::List { run_id } => exec_shard_list_command(context, run_id).await,
            RunShardSubCommand::Show { run_id, run_shard } => {
                exec_route_command(context, run_id, run_shard).await
            }
            RunShardSubCommand::Assign {
                run_id,
                run_shard,
                target,
            } => exec_shard_assign_command(context, run_id, run_shard, target).await,
            RunShardSubCommand::Move {
                run_id,
                run_shard,
                target,
                dry_run,
                verify_only,
                force,
            } => {
                exec_move_command(
                    context,
                    run_id,
                    run_shard,
                    target,
                    dry_run,
                    verify_only,
                    force,
                )
                .await
            }
            RunShardSubCommand::AbortMove {
                run_id,
                run_shard,
                source,
                target,
            } => exec_move_abort_command(context, run_id, run_shard, source, target).await,
        }
    }
}

#[async_trait]
impl Executable for RebalanceCommand {
    async fn exec(self, context: Context) -> anyhow::Result<()> {
        exec_rebalance_command(context, self.command).await
    }
}

async fn exec_database_command(
    context: Context,
    command: DatabaseSubCommand,
) -> anyhow::Result<()> {
    let database_router = context.dbr().await?;
    let control_db = database_router.control().await?;
    let out = context.out().await?;

    match command {
        DatabaseSubCommand::List => {
            let placements = shard_admin::list_database_placements(control_db).await?;
            out.write_value(&database_list_payload(&placements))?;
        }
        DatabaseSubCommand::Register {
            alias,
            database_url_env,
            defer_env_validation,
        } => {
            let placement = shard_admin::add_shard_database_placement(
                control_db,
                &alias,
                &database_url_env,
                defer_env_validation,
            )
            .await?;
            info!(database_alias = %placement.alias, "registered shard database placement");
            out.write_value(&database_registered_payload(
                &placement,
                !defer_env_validation,
            ))?;
        }
        DatabaseSubCommand::Disable { alias } => {
            let placement =
                shard_admin::disable_database_placement(database_router, &alias).await?;
            info!(database_alias = %placement.alias, "disabled database placement");
            out.write_value(&database_disabled_payload(&placement))?;
        }
        DatabaseSubCommand::Drain { alias } => {
            let placement = shard_admin::drain_database_placement(database_router, &alias).await?;
            info!(database_alias = %placement.alias, "started draining database placement");
            out.write_value(&database_draining_payload(&placement))?;
        }
    }

    Ok(())
}

async fn exec_move_command(
    context: Context,
    run_id: String,
    run_shard: i16,
    alias: String,
    dry_run: bool,
    verify_only: bool,
    force: bool,
) -> anyhow::Result<()> {
    let run_id = parse_run_id(&run_id)?;
    let database_router = context.dbr().await?;
    let out = context.out().await?;
    let outcome = shard_admin::move_shard_placement(
        database_router,
        run_id,
        run_shard,
        &alias,
        shard_admin::ShardMoveOptions {
            dry_run,
            verify_only,
            force,
        },
    )
    .await?;

    info!(
        run_id = %run_id,
        run_shard,
        source_database_alias = %outcome.source_database_alias,
        target_database_alias = %outcome.target_database_alias,
        moved = outcome.moved,
        "completed shard move workflow"
    );
    out.write_value(&move_payload(&outcome))?;
    Ok(())
}

async fn exec_move_abort_command(
    context: Context,
    run_id: String,
    run_shard: i16,
    source: String,
    target: String,
) -> anyhow::Result<()> {
    let run_id = parse_run_id(&run_id)?;
    let database_router = context.dbr().await?;
    let out = context.out().await?;
    let outcome =
        shard_admin::abort_shard_move(database_router, run_id, run_shard, &source, &target).await?;

    info!(
        run_id = %run_id,
        run_shard,
        source_database_alias = %outcome.source_database_alias,
        target_database_alias = %outcome.target_database_alias,
        aborted = outcome.aborted,
        "completed shard move abort workflow"
    );
    out.write_value(&move_abort_payload(&outcome))?;
    Ok(())
}

async fn exec_route_command(
    context: Context,
    run_id: String,
    run_shard: i16,
) -> anyhow::Result<()> {
    let run_id = parse_run_id(&run_id)?;
    let database_router = context.dbr().await?;
    let out = context.out().await?;
    let route = shard_admin::inspect_shard_route(database_router, run_id, run_shard).await?;

    info!(
        run_id = %run_id,
        run_shard,
        database_alias = %route.database_alias,
        placement_status = %route.shard_placement_status,
        routing_decision = route.routing_decision,
        "inspected shard route"
    );
    out.write_value(&route_payload(&route))?;
    Ok(())
}

async fn exec_shard_list_command(context: Context, run_id: String) -> anyhow::Result<()> {
    let run_id = parse_run_id(&run_id)?;
    let database_router = context.dbr().await?;
    let control_db = database_router.control().await?;
    let out = context.out().await?;
    let placements = shard_admin::list_shard_placements(control_db, run_id).await?;
    out.write_value(&placement_list_payload(run_id, &placements))?;
    Ok(())
}

async fn exec_shard_assign_command(
    context: Context,
    run_id: String,
    run_shard: i16,
    target: String,
) -> anyhow::Result<()> {
    let run_id = parse_run_id(&run_id)?;
    let database_router = context.dbr().await?;
    let out = context.out().await?;
    let outcome =
        shard_admin::set_shard_placement(database_router, run_id, run_shard, &target).await?;
    info!(
        run_id = %run_id,
        run_shard,
        database_alias = %outcome.placement.database_alias,
        "assigned run shard"
    );
    out.write_value(&shard_assignment_payload(&outcome))?;
    Ok(())
}

async fn exec_placement_command(
    context: Context,
    command: PlacementSubCommand,
) -> anyhow::Result<()> {
    match command {
        PlacementSubCommand::List { run_id } => exec_shard_list_command(context, run_id).await,
        PlacementSubCommand::Set {
            run_id,
            run_shard,
            alias,
        } => exec_shard_assign_command(context, run_id, run_shard, alias).await,
        PlacementSubCommand::Show { run_id, run_shard } => {
            let run_id = parse_run_id(&run_id)?;
            let database_router = context.dbr().await?;
            let control_db = database_router.control().await?;
            let out = context.out().await?;
            let placement = shard_admin::select_shard_placement(control_db, run_id, run_shard)
                .await?
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "shard placement for run {} shard {} was not found",
                        run_id,
                        run_shard
                    )
                })?;
            out.write_value(&placement_show_payload(&placement))?;
            Ok(())
        }
    }
}

async fn exec_rebalance_command(
    context: Context,
    command: RebalanceSubCommand,
) -> anyhow::Result<()> {
    let database_router = context.dbr().await?;
    let out = context.out().await?;

    match command {
        RebalanceSubCommand::Plan {
            from,
            target,
            max_items,
            dry_run,
        } => {
            let outcome = shard_admin::plan_shard_rebalance(
                database_router,
                shard_admin::ShardRebalancePlanOptions {
                    source_database_alias: from,
                    target_database_alias: target,
                    max_items,
                    dry_run,
                },
            )
            .await?;
            info!(
                operation_id = ?outcome.operation.as_ref().map(|operation| operation.id),
                planned_item_count = outcome.items.len(),
                dry_run = outcome.dry_run,
                "planned shard rebalance operation"
            );
            out.write_value(&rebalance_plan_payload(&outcome))?;
        }
        RebalanceSubCommand::Apply {
            operation_id,
            max_items,
            lease_seconds,
            force,
        } => {
            let operation_id = parse_operation_id(&operation_id)?;
            let outcome = shard_admin::apply_shard_rebalance(
                database_router,
                operation_id,
                shard_admin::ShardRebalanceApplyOptions {
                    max_items,
                    lease_seconds,
                    force,
                },
            )
            .await?;
            info!(
                operation_id = %outcome.operation.id,
                processed_item_count = outcome.processed_items.len(),
                operation_status = %outcome.operation.status,
                "applied shard rebalance operation"
            );
            out.write_value(&rebalance_apply_payload(&outcome))?;
        }
        RebalanceSubCommand::Verify {
            operation_id,
            max_items,
        } => {
            let operation_id = parse_operation_id(&operation_id)?;
            let outcome =
                shard_admin::verify_shard_rebalance(database_router, operation_id, max_items)
                    .await?;
            info!(
                operation_id = %outcome.operation.id,
                verified_item_count = outcome.items.len(),
                "verified shard rebalance operation"
            );
            out.write_value(&rebalance_verify_payload(&outcome))?;
        }
        RebalanceSubCommand::Cancel { operation_id } => {
            let operation_id = parse_operation_id(&operation_id)?;
            let operation =
                shard_admin::cancel_shard_rebalance(database_router, operation_id).await?;
            info!(
                operation_id = %operation.id,
                operation_status = %operation.status,
                "cancelled shard rebalance operation"
            );
            out.write_value(&rebalance_cancel_payload(&operation))?;
        }
    }

    Ok(())
}

fn parse_run_id(raw: &str) -> anyhow::Result<Uuid> {
    Uuid::parse_str(raw).map_err(|err| anyhow::anyhow!("invalid run_id '{}': {}", raw, err))
}

fn parse_operation_id(raw: &str) -> anyhow::Result<Uuid> {
    Uuid::parse_str(raw).map_err(|err| anyhow::anyhow!("invalid operation_id '{}': {}", raw, err))
}

fn database_list_payload(placements: &[DatabasePlacement]) -> Value {
    json!({
        "data": {
            "database_placements": placements,
        },
        "meta": {
            "count": placements.len(),
        }
    })
}

fn database_registered_payload(placement: &DatabasePlacement, env_validated: bool) -> Value {
    json!({
        "data": {
            "database_placement": placement,
        },
        "meta": {
            "created": true,
            "env_validated": env_validated,
        }
    })
}

fn database_disabled_payload(placement: &DatabasePlacement) -> Value {
    json!({
        "data": {
            "database_placement": placement,
        },
        "meta": {
            "disabled": placement.status == "disabled",
        }
    })
}

fn database_draining_payload(placement: &DatabasePlacement) -> Value {
    json!({
        "data": {
            "database_placement": placement,
        },
        "meta": {
            "draining": placement.status == "draining",
        }
    })
}

fn placement_list_payload(run_id: Uuid, placements: &[ShardPlacement]) -> Value {
    json!({
        "data": {
            "run_id": run_id,
            "shard_placements": placements,
        },
        "meta": {
            "count": placements.len(),
        }
    })
}

fn placement_show_payload(placement: &ShardPlacement) -> Value {
    json!({
        "data": {
            "shard_placement": placement,
        }
    })
}

fn shard_assignment_payload(outcome: &shard_admin::ShardPlacementSetOutcome) -> Value {
    json!({
        "data": {
            "shard_placement": outcome.placement,
        },
        "meta": {
            "changed": outcome.changed,
            "previous_database_alias": outcome.previous_database_alias,
        }
    })
}

fn move_payload(outcome: &shard_admin::ShardMoveOutcome) -> Value {
    json!({
        "data": {
            "run_id": outcome.run_id,
            "run_shard": outcome.run_shard,
            "source_database_alias": outcome.source_database_alias,
            "target_database_alias": outcome.target_database_alias,
            "active_work_count": outcome.active_work_count,
            "placement": outcome.placement,
            "tables": outcome.tables,
        },
        "meta": {
            "dry_run": outcome.dry_run,
            "verify_only": outcome.verify_only,
            "forced": outcome.forced,
            "moved": outcome.moved,
            "verified": outcome.tables.iter().all(|table| table.verified),
        }
    })
}

fn move_abort_payload(outcome: &shard_admin::ShardMoveAbortOutcome) -> Value {
    json!({
        "data": {
            "run_id": outcome.run_id,
            "run_shard": outcome.run_shard,
            "source_database_alias": outcome.source_database_alias,
            "target_database_alias": outcome.target_database_alias,
            "placement": outcome.placement,
        },
        "meta": {
            "aborted": outcome.aborted,
        }
    })
}

fn rebalance_plan_payload(outcome: &shard_admin::ShardRebalancePlanOutcome) -> Value {
    json!({
        "data": {
            "operation": outcome.operation,
            "items": outcome.items,
        },
        "meta": {
            "dry_run": outcome.dry_run,
            "planned_item_count": outcome.items.len(),
            "persisted": outcome.operation.is_some(),
        }
    })
}

fn rebalance_apply_payload(outcome: &shard_admin::ShardRebalanceApplyOutcome) -> Value {
    json!({
        "data": {
            "operation": outcome.operation,
            "items": outcome.processed_items,
        },
        "meta": {
            "processed_item_count": outcome.processed_items.len(),
            "completed_item_count": outcome.operation.completed_item_count,
            "failed_item_count": outcome.operation.failed_item_count,
            "cancelled_item_count": outcome.operation.cancelled_item_count,
        }
    })
}

fn rebalance_verify_payload(outcome: &shard_admin::ShardRebalanceVerifyOutcome) -> Value {
    let verified = outcome.items.iter().all(|item| item.verified);
    json!({
        "data": {
            "operation": outcome.operation,
            "items": outcome.items,
        },
        "meta": {
            "verified": verified,
            "verified_item_count": outcome.items.len(),
        }
    })
}

fn rebalance_cancel_payload(operation: &shard_admin::ShardRebalanceOperation) -> Value {
    json!({
        "data": {
            "operation": operation,
        },
        "meta": {
            "cancelled": operation.status == "cancelled",
        }
    })
}

fn route_payload(route: &shard_admin::ShardRouteInspection) -> Value {
    json!({
        "data": {
            "run_id": route.run_id,
            "run_shard": route.run_shard,
            "database_alias": route.database_alias,
            "shard_placement_status": route.shard_placement_status,
            "move_target_database_alias": route.move_target_database_alias,
            "route_version": route.route_version,
            "database_role": route.database_role,
            "database_status": route.database_status,
            "database_url_env": route.database_url_env,
            "database_url_env_resolved": route.database_url_env_resolved,
            "dispatchable": route.dispatchable,
            "readable": route.readable,
            "routing_decision": route.routing_decision,
            "move_operation_id": route.move_operation_id,
            "move_phase": route.move_phase,
            "move_completed_page_count": route.move_completed_page_count,
            "move_copied_row_count": route.move_copied_row_count,
            "move_copied_byte_count": route.move_copied_byte_count,
        }
    })
}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use clap::Parser;

    use super::*;
    use crate::models::{
        database_placement::{
            DATABASE_PLACEMENT_ROLE_SHARD,
            DATABASE_PLACEMENT_STATUS_ACTIVE,
            DATABASE_PLACEMENT_STATUS_DRAINING,
        },
        shard_placement::SHARD_PLACEMENT_STATUS_ACTIVE,
    };

    #[derive(Debug, Parser)]
    struct TestCli {
        #[command(subcommand)]
        command: SubCommand,
    }

    #[derive(Debug, Parser)]
    struct RunShardTestCli {
        #[command(subcommand)]
        command: RunShardSubCommand,
    }

    #[derive(Debug, Parser)]
    struct DatabaseTestCli {
        #[command(subcommand)]
        command: DatabaseSubCommand,
    }

    #[derive(Debug, Parser)]
    struct RebalanceTestCli {
        #[command(subcommand)]
        command: RebalanceSubCommand,
    }

    #[test]
    fn canonical_run_shard_commands_use_directional_flags() {
        let run_id = Uuid::now_v7().to_string();
        let move_cli =
            RunShardTestCli::try_parse_from(["vigilo", "move", &run_id, "4", "--to", "shard_001"])
                .unwrap();
        let abort_cli = RunShardTestCli::try_parse_from([
            "vigilo",
            "abort-move",
            &run_id,
            "4",
            "--from",
            "primary",
            "--to",
            "shard_001",
        ])
        .unwrap();

        assert!(matches!(
            move_cli.command,
            RunShardSubCommand::Move { target, .. } if target == "shard_001"
        ));
        assert!(matches!(
            abort_cli.command,
            RunShardSubCommand::AbortMove {
                source,
                target,
                ..
            } if source == "primary" && target == "shard_001"
        ));
    }

    #[test]
    fn canonical_database_and_rebalance_commands_use_resource_terms() {
        let database_cli = DatabaseTestCli::try_parse_from([
            "vigilo",
            "register",
            "shard_001",
            "--database-url-env",
            "VIGILO_SHARD_001_DATABASE_URL",
        ])
        .unwrap();
        let rebalance_cli = RebalanceTestCli::try_parse_from([
            "vigilo",
            "plan",
            "--from",
            "primary",
            "--to",
            "shard_001",
        ])
        .unwrap();

        assert!(matches!(
            database_cli.command,
            DatabaseSubCommand::Register { alias, .. } if alias == "shard_001"
        ));
        assert!(matches!(
            rebalance_cli.command,
            RebalanceSubCommand::Plan {
                from,
                target,
                ..
            } if from.as_deref() == Some("primary") && target == "shard_001"
        ));
    }

    #[test]
    fn move_command_matches_placement_argument_shape() {
        let run_id = Uuid::now_v7().to_string();
        let cli = TestCli::try_parse_from([
            "vigilo",
            "move",
            &run_id,
            "4",
            "--alias",
            "shard_001",
            "--dry-run",
        ])
        .unwrap();

        let SubCommand::Move {
            run_id: parsed_run_id,
            run_shard,
            alias,
            dry_run,
            ..
        } = cli.command
        else {
            panic!("expected shard move command");
        };

        assert_eq!(parsed_run_id, run_id);
        assert_eq!(run_shard, 4);
        assert_eq!(alias, "shard_001");
        assert!(dry_run);
    }

    #[test]
    fn move_command_accepts_to_as_alias_for_compatibility() {
        let run_id = Uuid::now_v7().to_string();
        let cli =
            TestCli::try_parse_from(["vigilo", "move", &run_id, "4", "--to", "shard_001"]).unwrap();

        let SubCommand::Move { alias, .. } = cli.command else {
            panic!("expected shard move command");
        };

        assert_eq!(alias, "shard_001");
    }

    #[test]
    fn move_abort_command_requires_expected_source_and_target() {
        let run_id = Uuid::now_v7().to_string();
        let cli = TestCli::try_parse_from([
            "vigilo",
            "move-abort",
            &run_id,
            "4",
            "--source",
            "primary",
            "--target",
            "shard_001",
        ])
        .unwrap();

        let SubCommand::MoveAbort {
            run_id: parsed_run_id,
            run_shard,
            source,
            target,
        } = cli.command
        else {
            panic!("expected shard move-abort command");
        };

        assert_eq!(parsed_run_id, run_id);
        assert_eq!(run_shard, 4);
        assert_eq!(source, "primary");
        assert_eq!(target, "shard_001");
    }

    #[test]
    fn route_command_matches_argument_shape() {
        let run_id = Uuid::now_v7().to_string();
        let cli = TestCli::try_parse_from(["vigilo", "route", &run_id, "4"]).unwrap();

        let SubCommand::Route {
            run_id: parsed_run_id,
            run_shard,
        } = cli.command
        else {
            panic!("expected shard route command");
        };

        assert_eq!(parsed_run_id, run_id);
        assert_eq!(run_shard, 4);
    }

    #[test]
    fn rebalance_plan_command_matches_argument_shape() {
        let cli = TestCli::try_parse_from([
            "vigilo",
            "rebalance",
            "plan",
            "--from",
            "primary",
            "--to",
            "shard_001",
            "--max-items",
            "12",
            "--dry-run",
        ])
        .unwrap();

        let SubCommand::Rebalance {
            command:
                RebalanceSubCommand::Plan {
                    from,
                    target,
                    max_items,
                    dry_run,
                },
        } = cli.command
        else {
            panic!("expected shard rebalance plan command");
        };

        assert_eq!(from.as_deref(), Some("primary"));
        assert_eq!(target, "shard_001");
        assert_eq!(max_items, 12);
        assert!(dry_run);
    }

    #[test]
    fn rebalance_apply_command_matches_argument_shape() {
        let operation_id = Uuid::now_v7().to_string();
        let cli = TestCli::try_parse_from([
            "vigilo",
            "rebalance",
            "apply",
            &operation_id,
            "--max-items",
            "3",
            "--lease-seconds",
            "45",
            "--force",
        ])
        .unwrap();

        let SubCommand::Rebalance {
            command:
                RebalanceSubCommand::Apply {
                    operation_id: parsed_operation_id,
                    max_items,
                    lease_seconds,
                    force,
                },
        } = cli.command
        else {
            panic!("expected shard rebalance apply command");
        };

        assert_eq!(parsed_operation_id, operation_id);
        assert_eq!(max_items, 3);
        assert_eq!(lease_seconds, 45);
        assert!(force);
    }

    #[test]
    fn database_drain_command_matches_lifecycle_shape() {
        let cli = TestCli::try_parse_from(["vigilo", "databases", "drain", "shard_001"]).unwrap();

        let SubCommand::Databases {
            command: DatabaseSubCommand::Drain { alias },
        } = cli.command
        else {
            panic!("expected database drain command");
        };

        assert_eq!(alias, "shard_001");
    }

    #[test]
    fn database_list_payload_reports_count() {
        let placement = DatabasePlacement {
            alias: "shard_001".to_string(),
            database_url_env: "VIGILO_SHARD_001_DATABASE_URL".to_string(),
            role: DATABASE_PLACEMENT_ROLE_SHARD.to_string(),
            status: DATABASE_PLACEMENT_STATUS_ACTIVE.to_string(),
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };

        let payload = database_list_payload(&[placement]);

        assert_eq!(payload["meta"]["count"], json!(1));
        assert_eq!(
            payload["data"]["database_placements"][0]["alias"],
            json!("shard_001")
        );
    }

    #[test]
    fn database_draining_payload_reports_lifecycle_state() {
        let placement = DatabasePlacement {
            alias: "shard_001".to_string(),
            database_url_env: "VIGILO_SHARD_001_DATABASE_URL".to_string(),
            role: DATABASE_PLACEMENT_ROLE_SHARD.to_string(),
            status: DATABASE_PLACEMENT_STATUS_DRAINING.to_string(),
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };

        let payload = database_draining_payload(&placement);

        assert_eq!(payload["meta"]["draining"], json!(true));
        assert_eq!(
            payload["data"]["database_placement"]["status"],
            json!("draining")
        );
    }

    #[test]
    fn shard_assignment_payload_reports_previous_alias() {
        let run_id = Uuid::now_v7();
        let placement = ShardPlacement {
            run_id,
            run_shard: 7,
            database_alias: "shard_002".to_string(),
            status: SHARD_PLACEMENT_STATUS_ACTIVE.to_string(),
            move_target_database_alias: None,
            route_version: 1,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };
        let outcome = shard_admin::ShardPlacementSetOutcome {
            placement,
            previous_database_alias: Some("shard_001".to_string()),
            changed: true,
        };

        let payload = shard_assignment_payload(&outcome);

        assert_eq!(payload["meta"]["changed"], json!(true));
        assert_eq!(
            payload["meta"]["previous_database_alias"],
            json!("shard_001")
        );
        assert_eq!(
            payload["data"]["shard_placement"]["database_alias"],
            json!("shard_002")
        );
    }

    #[test]
    fn move_payload_reports_verification_state() {
        let run_id = Uuid::now_v7();
        let placement = ShardPlacement {
            run_id,
            run_shard: 4,
            database_alias: "shard_001".to_string(),
            status: SHARD_PLACEMENT_STATUS_ACTIVE.to_string(),
            move_target_database_alias: None,
            route_version: 2,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };
        let outcome = shard_admin::ShardMoveOutcome {
            run_id,
            run_shard: 4,
            source_database_alias: "primary".to_string(),
            target_database_alias: "shard_001".to_string(),
            dry_run: false,
            verify_only: false,
            forced: false,
            active_work_count: 0,
            moved: true,
            placement,
            tables: vec![shard_admin::ShardMoveTableReport {
                table: "run_chunks",
                source_row_count: None,
                target_row_count: None,
                copied_row_count: 1,
                source_checksum: None,
                target_checksum: None,
                verification_mode: "checkpoint_and_replay",
                verified: true,
            }],
        };

        let payload = move_payload(&outcome);

        assert_eq!(payload["meta"]["moved"], json!(true));
        assert_eq!(payload["meta"]["verified"], json!(true));
        assert_eq!(payload["data"]["tables"][0]["table"], json!("run_chunks"));
        assert_eq!(
            payload["data"]["tables"][0]["verification_mode"],
            json!("checkpoint_and_replay")
        );
        assert!(payload["data"]["tables"][0]["source_row_count"].is_null());
    }

    #[test]
    fn move_abort_payload_reports_restored_source_route() {
        let run_id = Uuid::now_v7();
        let placement = ShardPlacement {
            run_id,
            run_shard: 4,
            database_alias: "primary".to_string(),
            status: SHARD_PLACEMENT_STATUS_ACTIVE.to_string(),
            move_target_database_alias: None,
            route_version: 4,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };
        let outcome = shard_admin::ShardMoveAbortOutcome {
            run_id,
            run_shard: 4,
            source_database_alias: "primary".to_string(),
            target_database_alias: "shard_001".to_string(),
            aborted: true,
            placement,
        };

        let payload = move_abort_payload(&outcome);

        assert_eq!(payload["meta"]["aborted"], json!(true));
        assert_eq!(
            payload["data"]["placement"]["database_alias"],
            json!("primary")
        );
        assert_eq!(payload["data"]["placement"]["status"], json!("active"));
    }

    #[test]
    fn rebalance_plan_payload_reports_persisted_state() {
        let outcome = shard_admin::ShardRebalancePlanOutcome {
            operation: None,
            dry_run: true,
            items: vec![shard_admin::PlannedShardRebalanceItem {
                run_id: Uuid::now_v7(),
                run_shard: 4,
                source_database_alias: "primary".to_string(),
                target_database_alias: "shard_001".to_string(),
                planned_route_version: 1,
            }],
        };

        let payload = rebalance_plan_payload(&outcome);

        assert_eq!(payload["meta"]["dry_run"], json!(true));
        assert_eq!(payload["meta"]["persisted"], json!(false));
        assert_eq!(payload["meta"]["planned_item_count"], json!(1));
    }

    #[test]
    fn route_payload_excludes_secret_url_value() {
        let run_id = Uuid::now_v7();
        let route = shard_admin::ShardRouteInspection {
            run_id,
            run_shard: 4,
            database_alias: "shard_001".to_string(),
            shard_placement_status: "active".to_string(),
            move_target_database_alias: None,
            route_version: 7,
            database_role: "shard".to_string(),
            database_status: "active".to_string(),
            database_url_env: "VIGILO_SHARD_001_DATABASE_URL".to_string(),
            database_url_env_resolved: true,
            dispatchable: true,
            readable: true,
            routing_decision: "dispatchable",
            move_operation_id: None,
            move_phase: None,
            move_completed_page_count: None,
            move_copied_row_count: None,
            move_copied_byte_count: None,
        };

        let payload = route_payload(&route);

        assert_eq!(payload["data"]["run_id"], json!(run_id));
        assert_eq!(payload["data"]["run_shard"], json!(4));
        assert_eq!(payload["data"]["database_alias"], json!("shard_001"));
        assert_eq!(payload["data"]["route_version"], json!(7));
        assert_eq!(
            payload["data"]["database_url_env"],
            json!("VIGILO_SHARD_001_DATABASE_URL")
        );
        assert!(
            payload
                .to_string()
                .contains("VIGILO_SHARD_001_DATABASE_URL")
        );
        assert!(!payload.to_string().contains("postgres://"));
        assert_eq!(payload["data"]["routing_decision"], json!("dispatchable"));
    }
}
