//! Shard placement administration commands.
//!
//! These commands inspect and update routing metadata. They do not move data;
//! changing a placement that already owns shard-local rows is reserved for the
//! shard move workflow.

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
            SubCommand::Placements { command } => exec_placement_command(context, command).await,
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

#[cfg(test)]
mod tests {
    use chrono::Utc;

    use super::*;
    use crate::models::{
        database_placement::{
            DATABASE_PLACEMENT_ROLE_SHARD,
            DATABASE_PLACEMENT_STATUS_ACTIVE,
        },
        shard_placement::SHARD_PLACEMENT_STATUS_ACTIVE,
    };

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
}
