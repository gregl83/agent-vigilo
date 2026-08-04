//! PostgreSQL operations for execution-local shard admission.

use sqlx::{
    Executor,
    Postgres,
};
use uuid::Uuid;

use super::{
    LocalShardAdmission,
    LocalShardAdmissionDraft,
    LocalShardAdmissionState,
};

pub(crate) async fn select_local_shard_admission<'e, E>(
    executor: E,
    run_id: Uuid,
    run_shard: i16,
) -> anyhow::Result<Option<LocalShardAdmission>>
where
    E: Executor<'e, Database = Postgres>,
{
    Ok(sqlx::query_as::<_, LocalShardAdmission>(
        r#"
        SELECT run_id, run_shard, database_alias, write_epoch, state,
               redirect_database_alias
        FROM local_shard_admissions
        WHERE run_id = $1
          AND run_shard = $2
        "#,
    )
    .bind(run_id)
    .bind(run_shard)
    .fetch_optional(executor)
    .await?)
}

pub(crate) async fn upsert_local_shard_admission<'e, E>(
    executor: E,
    draft: LocalShardAdmissionDraft,
) -> anyhow::Result<LocalShardAdmission>
where
    E: Executor<'e, Database = Postgres>,
{
    let admission = sqlx::query_as::<_, LocalShardAdmission>(
        r#"
        INSERT INTO local_shard_admissions (
            run_id, run_shard, database_alias, write_epoch, state,
            redirect_database_alias
        )
        VALUES ($1, $2, $3, $4, $5, $6)
        ON CONFLICT (run_id, run_shard) DO UPDATE
        SET database_alias = EXCLUDED.database_alias,
            write_epoch = EXCLUDED.write_epoch,
            state = EXCLUDED.state,
            redirect_database_alias = EXCLUDED.redirect_database_alias,
            updated_at = now()
        WHERE local_shard_admissions.write_epoch < EXCLUDED.write_epoch
           OR (
                local_shard_admissions.write_epoch = EXCLUDED.write_epoch
                AND local_shard_admissions.database_alias = EXCLUDED.database_alias
                AND local_shard_admissions.state = EXCLUDED.state
           )
        RETURNING run_id, run_shard, database_alias, write_epoch, state,
                  redirect_database_alias
        "#,
    )
    .bind(draft.run_id)
    .bind(draft.run_shard)
    .bind(&draft.database_alias)
    .bind(draft.write_epoch)
    .bind(draft.state.as_str())
    .bind(&draft.redirect_database_alias)
    .fetch_one(executor)
    .await?;
    Ok(admission)
}

/// Advances local authority monotonically, with explicit same-epoch states.
pub(crate) async fn transition_local_shard_admission<'e, E>(
    executor: E,
    draft: LocalShardAdmissionDraft,
    allowed_same_epoch_states: &[LocalShardAdmissionState],
) -> anyhow::Result<LocalShardAdmission>
where
    E: Executor<'e, Database = Postgres>,
{
    let allowed_states = allowed_same_epoch_states
        .iter()
        .map(|state| state.as_str())
        .collect::<Vec<_>>();
    let admission = sqlx::query_as::<_, LocalShardAdmission>(
        r#"
        INSERT INTO local_shard_admissions (
            run_id, run_shard, database_alias, write_epoch, state,
            redirect_database_alias
        )
        VALUES ($1, $2, $3, $4, $5, $6)
        ON CONFLICT (run_id, run_shard) DO UPDATE
        SET database_alias = EXCLUDED.database_alias,
            write_epoch = EXCLUDED.write_epoch,
            state = EXCLUDED.state,
            redirect_database_alias = EXCLUDED.redirect_database_alias,
            updated_at = now()
        WHERE local_shard_admissions.write_epoch < EXCLUDED.write_epoch
           OR (
                local_shard_admissions.write_epoch = EXCLUDED.write_epoch
                AND local_shard_admissions.state = ANY($7::text[])
           )
        RETURNING run_id, run_shard, database_alias, write_epoch, state,
                  redirect_database_alias
        "#,
    )
    .bind(draft.run_id)
    .bind(draft.run_shard)
    .bind(&draft.database_alias)
    .bind(draft.write_epoch)
    .bind(draft.state.as_str())
    .bind(&draft.redirect_database_alias)
    .bind(allowed_states)
    .fetch_optional(executor)
    .await?
    .ok_or_else(|| {
        anyhow::anyhow!(
            "local shard admission for run {} shard {} rejected transition to {} epoch {}",
            draft.run_id,
            draft.run_shard,
            draft.state,
            draft.write_epoch
        )
    })?;
    Ok(admission)
}
