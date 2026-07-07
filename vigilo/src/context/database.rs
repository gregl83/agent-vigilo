//! Lazy PostgreSQL pool and placement configuration context.
//!
//! Commands call this module through [`crate::context::Context::db`] when they
//! first need database access. Connection options are process-wide and should
//! be supplied by CLI/env configuration. The placement catalog lives in the
//! database; environment variables only provide secret connection values.

use std::collections::HashMap;

use sqlx::{
    PgPool,
    postgres::PgPoolOptions,
};
use tokio::sync::OnceCell;
use tracing::debug;

use crate::{
    db::tables::database_placements,
    models::database_placement::DEFAULT_DATABASE_URL_ENV,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PlacementConfig {
    pub(crate) control_database_alias: String,
    pub(crate) default_shard_database_alias: String,
}

impl PlacementConfig {
    pub(crate) fn new(
        control_database_alias: String,
        default_shard_database_alias: String,
    ) -> anyhow::Result<Self> {
        let control_database_alias = normalize_alias(control_database_alias, "control alias")?;
        let default_shard_database_alias =
            normalize_alias(default_shard_database_alias, "default shard alias")?;

        Ok(Self {
            control_database_alias,
            default_shard_database_alias,
        })
    }

    #[cfg(test)]
    pub(crate) fn default_single_database() -> Self {
        Self {
            control_database_alias: crate::models::database_placement::DEFAULT_DATABASE_ALIAS
                .to_string(),
            default_shard_database_alias: crate::models::database_placement::DEFAULT_DATABASE_ALIAS
                .to_string(),
        }
    }
}

pub struct Context {
    pub(crate) uri: String,
    pub(crate) max_connections: u32,
    pub(crate) placement_config: PlacementConfig,
    pub(crate) cell: OnceCell<PgPool>,
}

impl Context {
    pub async fn get(&self) -> anyhow::Result<&PgPool> {
        self.cell
            .get_or_try_init(|| async {
                debug!("initializing postgres database connection");

                PgPoolOptions::new()
                    .max_connections(self.max_connections)
                    .connect(&self.uri)
                    .await
                    .map_err(|e| anyhow::anyhow!("database connection failed: {}", e))
            })
            .await
    }

    pub(crate) async fn validate_placement_config(&self) -> anyhow::Result<()> {
        let db = self.get().await?;
        let placements = database_placements::list_active_database_placements(db).await?;

        if placements.is_empty() {
            anyhow::bail!("database_placements has no active placements");
        }

        for placement in &placements {
            self.resolve_database_url_env(&placement.database_url_env)?;
        }

        let control_placements = placements
            .iter()
            .filter(|placement| placement.is_control_capable())
            .collect::<Vec<_>>();

        let [active_control] = control_placements.as_slice() else {
            anyhow::bail!(
                "expected exactly one active control-capable database placement, found {}",
                control_placements.len()
            );
        };

        if active_control.alias != self.placement_config.control_database_alias {
            anyhow::bail!(
                "VIGILO_CONTROL_DATABASE_ALIAS={} does not match active control placement {}",
                self.placement_config.control_database_alias,
                active_control.alias
            );
        }

        let placements_by_alias = placements
            .iter()
            .map(|placement| (placement.alias.as_str(), placement))
            .collect::<HashMap<_, _>>();

        validate_shard_capable_alias(
            &placements_by_alias,
            &self.placement_config.default_shard_database_alias,
            "VIGILO_DEFAULT_SHARD_DATABASE_ALIAS",
        )?;

        let disabled_routes =
            database_placements::count_shard_placements_on_disabled_databases(db).await?;
        if disabled_routes > 0 {
            anyhow::bail!(
                "{} shard placement row(s) route to disabled database placements",
                disabled_routes
            );
        }

        Ok(())
    }

    fn resolve_database_url_env(&self, database_url_env: &str) -> anyhow::Result<String> {
        if database_url_env == DEFAULT_DATABASE_URL_ENV {
            return Ok(self.uri.clone());
        }

        std::env::var(database_url_env).map_err(|_| {
            anyhow::anyhow!(
                "active database placement references unset env var {}",
                database_url_env
            )
        })
    }
}

fn normalize_alias(alias: String, label: &str) -> anyhow::Result<String> {
    let alias = alias.trim().to_string();
    if alias.is_empty() {
        anyhow::bail!("{} must not be empty", label);
    }

    Ok(alias)
}

fn validate_shard_capable_alias(
    placements_by_alias: &HashMap<&str, &crate::models::database_placement::DatabasePlacement>,
    alias: &str,
    config_name: &str,
) -> anyhow::Result<()> {
    let Some(placement) = placements_by_alias.get(alias) else {
        anyhow::bail!(
            "{}={} does not match an active database placement",
            config_name,
            alias
        );
    };

    if !placement.is_shard_capable() {
        anyhow::bail!(
            "{}={} points to placement role {}, which is not shard-capable",
            config_name,
            alias,
            placement.role
        );
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::database_placement::DEFAULT_DATABASE_ALIAS;

    #[test]
    fn default_config_uses_primary_aliases() {
        let config = PlacementConfig::default_single_database();

        assert_eq!(config.control_database_alias, DEFAULT_DATABASE_ALIAS);
        assert_eq!(config.default_shard_database_alias, DEFAULT_DATABASE_ALIAS);
    }

    #[test]
    fn config_normalizes_aliases() {
        let config = PlacementConfig::new(" primary ".to_string(), " exec_a ".to_string()).unwrap();

        assert_eq!(config.control_database_alias, "primary");
        assert_eq!(config.default_shard_database_alias, "exec_a");
    }

    #[test]
    fn config_rejects_empty_control_alias() {
        let error = PlacementConfig::new(" ".to_string(), "primary".to_string()).unwrap_err();

        assert!(
            error
                .to_string()
                .contains("control alias must not be empty")
        );
    }

    #[test]
    fn config_rejects_empty_default_shard_alias() {
        let error = PlacementConfig::new("primary".to_string(), " ".to_string()).unwrap_err();

        assert!(
            error
                .to_string()
                .contains("default shard alias must not be empty")
        );
    }
}
