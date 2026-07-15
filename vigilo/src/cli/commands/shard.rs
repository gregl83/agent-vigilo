//! Shard placement administration commands.
//!
//! These commands inspect placement metadata, update empty shard routes, and
//! run the explicit shard move workflow for routes that already own data.

use async_trait::async_trait;
use clap::{
    Args,
    Subcommand,
};
use serde_json::{
    Value,
    json,
};
use tracing::info;
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

        /// Allow copying while leased chunks or running attempts still exist
        #[arg(long, default_value_t = false)]
        force: bool,
    },
}

#[derive(Debug, Subcommand)]
/// Database placement operations.
pub(crate) enum DatabaseSubCommand {
    /// List database placements
    List,

    /// Add an active shard database placement
    Add {
        /// Stable placement alias
        alias: String,

        /// Environment variable containing this placement's database URL
        #[arg(long, value_name = "ENV")]
        database_url_env: String,

        /// Add the placement even when database_url_env is not set in this process
        #[arg(long, default_value_t = false)]
        defer_env_validation: bool,
    },

    /// Disable a non-control database placement
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

#[derive(Debug, Args)]
/// Arguments for `vigilo shard`.
pub(crate) struct Command {
    #[command(subcommand)]
    pub command: SubCommand,
}

#[async_trait]
impl Executable for Command {
    async fn exec(self, context: Context) -> anyhow::Result<()> {
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
            SubCommand::Placements { command } => exec_placement_command(context, command).await,
            SubCommand::Route { run_id, run_shard } => {
                exec_route_command(context, run_id, run_shard).await
            }
        }
    }
}

async fn exec_database_command(
    context: Context,
    command: DatabaseSubCommand,
) -> anyhow::Result<()> {
    let database = context.db().await?;
    let control_db = database.control().await?;
    let out = context.out().await?;

    match command {
        DatabaseSubCommand::List => {
            let placements = shard_admin::list_database_placements(control_db).await?;
            out.write_value(&database_list_payload(&placements))?;
        }
        DatabaseSubCommand::Add {
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
            info!(database_alias = %placement.alias, "added shard database placement");
            out.write_value(&database_added_payload(&placement, !defer_env_validation))?;
        }
        DatabaseSubCommand::Disable { alias } => {
            let placement = shard_admin::disable_database_placement(control_db, &alias).await?;
            info!(database_alias = %placement.alias, "disabled database placement");
            out.write_value(&database_disabled_payload(&placement))?;
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
    let database = context.db().await?;
    let out = context.out().await?;
    let outcome = shard_admin::move_shard_placement(
        database,
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

async fn exec_route_command(
    context: Context,
    run_id: String,
    run_shard: i16,
) -> anyhow::Result<()> {
    let run_id = parse_run_id(&run_id)?;
    let database = context.db().await?;
    let out = context.out().await?;
    let route = shard_admin::inspect_shard_route(database, run_id, run_shard).await?;

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

async fn exec_placement_command(
    context: Context,
    command: PlacementSubCommand,
) -> anyhow::Result<()> {
    let database = context.db().await?;
    let control_db = database.control().await?;
    let out = context.out().await?;

    match command {
        PlacementSubCommand::List { run_id } => {
            let run_id = parse_run_id(&run_id)?;
            let placements = shard_admin::list_shard_placements(control_db, run_id).await?;
            out.write_value(&placement_list_payload(run_id, &placements))?;
        }
        PlacementSubCommand::Show { run_id, run_shard } => {
            let run_id = parse_run_id(&run_id)?;
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
        }
        PlacementSubCommand::Set {
            run_id,
            run_shard,
            alias,
        } => {
            let run_id = parse_run_id(&run_id)?;
            let outcome =
                shard_admin::set_shard_placement(database, run_id, run_shard, &alias).await?;
            info!(
                run_id = %run_id,
                run_shard,
                database_alias = %outcome.placement.database_alias,
                "set shard placement"
            );
            out.write_value(&placement_set_payload(&outcome))?;
        }
    }

    Ok(())
}

fn parse_run_id(raw: &str) -> anyhow::Result<Uuid> {
    Uuid::parse_str(raw).map_err(|err| anyhow::anyhow!("invalid run_id '{}': {}", raw, err))
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

fn database_added_payload(placement: &DatabasePlacement, env_validated: bool) -> Value {
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

fn placement_set_payload(outcome: &shard_admin::ShardPlacementSetOutcome) -> Value {
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

fn route_payload(route: &shard_admin::ShardRouteInspection) -> Value {
    json!({
        "data": {
            "run_id": route.run_id,
            "run_shard": route.run_shard,
            "database_alias": route.database_alias,
            "shard_placement_status": route.shard_placement_status,
            "route_version": route.route_version,
            "database_role": route.database_role,
            "database_status": route.database_status,
            "database_url_env": route.database_url_env,
            "database_url_env_resolved": route.database_url_env_resolved,
            "dispatchable": route.dispatchable,
            "readable": route.readable,
            "routing_decision": route.routing_decision,
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
        },
        shard_placement::SHARD_PLACEMENT_STATUS_ACTIVE,
    };

    #[derive(Debug, Parser)]
    struct TestCli {
        #[command(subcommand)]
        command: SubCommand,
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
    fn placement_set_payload_reports_previous_alias() {
        let run_id = Uuid::now_v7();
        let placement = ShardPlacement {
            run_id,
            run_shard: 7,
            database_alias: "shard_002".to_string(),
            status: SHARD_PLACEMENT_STATUS_ACTIVE.to_string(),
            route_version: 1,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };
        let outcome = shard_admin::ShardPlacementSetOutcome {
            placement,
            previous_database_alias: Some("shard_001".to_string()),
            changed: true,
        };

        let payload = placement_set_payload(&outcome);

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
                source_row_count: 1,
                target_row_count: 1,
                copied_row_count: 1,
                source_checksum: "a".to_string(),
                target_checksum: "a".to_string(),
                verified: true,
            }],
        };

        let payload = move_payload(&outcome);

        assert_eq!(payload["meta"]["moved"], json!(true));
        assert_eq!(payload["meta"]["verified"], json!(true));
        assert_eq!(payload["data"]["tables"][0]["table"], json!("run_chunks"));
    }

    #[test]
    fn route_payload_excludes_secret_url_value() {
        let run_id = Uuid::now_v7();
        let route = shard_admin::ShardRouteInspection {
            run_id,
            run_shard: 4,
            database_alias: "shard_001".to_string(),
            shard_placement_status: "active".to_string(),
            route_version: 7,
            database_role: "shard".to_string(),
            database_status: "active".to_string(),
            database_url_env: "VIGILO_SHARD_001_DATABASE_URL".to_string(),
            database_url_env_resolved: true,
            dispatchable: true,
            readable: true,
            routing_decision: "dispatchable",
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
