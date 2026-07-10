//! Run snapshot table access.
//!
//! Snapshots copy the immutable run context workers need into the execution
//! placement before chunk-ready events become visible.

use sqlx::PgPool;
use uuid::Uuid;

/// Finds only the run profile payload from a run snapshot.
///
/// Worker hot paths use this instead of reading the authoritative control
/// `runs` row.
pub(crate) async fn select_run_profile_snapshot(
    db: &PgPool,
    run_id: Uuid,
    run_shard: i16,
) -> anyhow::Result<Option<serde_json::Value>> {
    let profile = sqlx::query_scalar::<_, Option<serde_json::Value>>(
        r#"
        SELECT config_snapshot->'profile' AS profile_snapshot
        FROM run_snapshots
        WHERE run_id = $1::uuid
          AND run_shard = $2
        "#,
    )
    .bind(run_id)
    .bind(run_shard)
    .fetch_optional(db)
    .await?;

    Ok(profile.flatten())
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use sqlx::PgPool;
    use uuid::Uuid;

    use super::*;

    #[sqlx::test(migrations = "../migrations")]
    #[ignore = "requires a PostgreSQL DATABASE_URL for sqlx snapshot tests"]
    async fn select_run_profile_snapshot_reads_local_profile(pool: PgPool) {
        let run_id = Uuid::now_v7();
        let dataset_id = Uuid::now_v7();
        let dataset_version_id = Uuid::now_v7();
        let profile = json!({"profile_id": "profile", "profile_version": "1.0.0"});

        sqlx::query(
            r#"
            INSERT INTO run_snapshots (
                run_id,
                run_shard,
                run_key,
                dataset_id,
                dataset_version_id,
                dataset_version,
                evaluation_profile_id,
                evaluation_profile_version,
                profile_version_id,
                profile_hash,
                aggregation_policy_id,
                aggregation_policy_version,
                aggregation_policy_hash,
                agent_provider,
                agent_name,
                prompt_config_id,
                prompt_config_version,
                config_snapshot,
                expected_execution_count
            )
            VALUES (
                $1::uuid,
                7,
                'run-key',
                $2::uuid,
                $3::uuid,
                'dataset',
                'profile',
                '1.0.0',
                'profile-version',
                'profile-hash',
                'aggregation',
                '1.0.0',
                'aggregation-hash',
                'example',
                'agent',
                'prompt',
                '1.0.0',
                jsonb_build_object('profile', $4::jsonb),
                1
            )
            "#,
        )
        .bind(run_id)
        .bind(dataset_id)
        .bind(dataset_version_id)
        .bind(&profile)
        .execute(&pool)
        .await
        .unwrap();

        let selected = select_run_profile_snapshot(&pool, run_id, 7)
            .await
            .unwrap()
            .unwrap();

        assert_eq!(selected, profile);
    }
}
