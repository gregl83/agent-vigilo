//! Execution-local shard write admission.

use std::fmt;

use sqlx::{
    Executor,
    Postgres,
};
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LocalShardAdmissionState {
    Open,
    Draining,
    Prepared,
    Closed,
}

impl LocalShardAdmissionState {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Draining => "draining",
            Self::Prepared => "prepared",
            Self::Closed => "closed",
        }
    }

    pub(crate) fn allows_new_work(self) -> bool {
        self == Self::Open
    }

    pub(crate) fn allows_settlement(self) -> bool {
        matches!(self, Self::Open | Self::Draining)
    }
}

impl TryFrom<&str> for LocalShardAdmissionState {
    type Error = anyhow::Error;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        match value {
            "open" => Ok(Self::Open),
            "draining" => Ok(Self::Draining),
            "prepared" => Ok(Self::Prepared),
            "closed" => Ok(Self::Closed),
            other => anyhow::bail!("unsupported local shard admission state {other}"),
        }
    }
}

impl fmt::Display for LocalShardAdmissionState {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

#[derive(Debug, Clone)]
pub(crate) struct LocalShardAdmissionDraft {
    pub(crate) run_id: Uuid,
    pub(crate) run_shard: i16,
    pub(crate) database_alias: String,
    pub(crate) write_epoch: i64,
    pub(crate) state: LocalShardAdmissionState,
    pub(crate) redirect_database_alias: Option<String>,
}

#[derive(Debug, Clone)]
pub(crate) struct LocalShardRouteHint {
    pub(crate) run_id: Uuid,
    pub(crate) run_shard: i16,
    pub(crate) database_alias: String,
    pub(crate) write_epoch: i64,
}

#[derive(Debug, Clone, Copy)]
pub(crate) enum LocalShardWriteKind {
    NewWork,
    Settlement,
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub(crate) struct LocalShardAdmission {
    pub(crate) run_id: Uuid,
    pub(crate) run_shard: i16,
    pub(crate) database_alias: String,
    pub(crate) write_epoch: i64,
    state: String,
    pub(crate) redirect_database_alias: Option<String>,
}

impl LocalShardAdmission {
    pub(crate) fn parsed_state(&self) -> anyhow::Result<LocalShardAdmissionState> {
        LocalShardAdmissionState::try_from(self.state.as_str())
    }
}

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

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub(crate) enum LocalShardAdmissionError {
    #[error(
        "database {database_alias} has no local write admission for run {run_id} shard {run_shard}"
    )]
    Missing {
        run_id: Uuid,
        run_shard: i16,
        database_alias: String,
    },
    #[error(
        "stale write epoch for run {run_id} shard {run_shard} on {database_alias}; expected {expected_write_epoch}, local epoch is {actual_write_epoch} with state {actual_state}"
    )]
    StaleWriteEpoch {
        run_id: Uuid,
        run_shard: i16,
        database_alias: String,
        expected_write_epoch: i64,
        actual_write_epoch: i64,
        actual_state: String,
        redirect_database_alias: Option<String>,
    },
    #[error(
        "local write admission for run {run_id} shard {run_shard} on {database_alias} is {actual_state}, which rejects {write_kind}"
    )]
    RejectedState {
        run_id: Uuid,
        run_shard: i16,
        database_alias: String,
        actual_state: String,
        write_kind: &'static str,
        redirect_database_alias: Option<String>,
    },
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

pub(crate) async fn validate_local_shard_admission<'e, E>(
    executor: E,
    hint: &LocalShardRouteHint,
    write_kind: LocalShardWriteKind,
) -> anyhow::Result<LocalShardAdmission>
where
    E: Executor<'e, Database = Postgres>,
{
    let admission = select_local_shard_admission(executor, hint.run_id, hint.run_shard)
        .await?
        .ok_or_else(|| LocalShardAdmissionError::Missing {
            run_id: hint.run_id,
            run_shard: hint.run_shard,
            database_alias: hint.database_alias.clone(),
        })?;

    let state = admission.parsed_state()?;
    if admission.database_alias != hint.database_alias || admission.write_epoch != hint.write_epoch
    {
        return Err(LocalShardAdmissionError::StaleWriteEpoch {
            run_id: admission.run_id,
            run_shard: admission.run_shard,
            database_alias: admission.database_alias.clone(),
            expected_write_epoch: hint.write_epoch,
            actual_write_epoch: admission.write_epoch,
            actual_state: state.to_string(),
            redirect_database_alias: admission.redirect_database_alias.clone(),
        }
        .into());
    }

    let allowed = match write_kind {
        LocalShardWriteKind::NewWork => state.allows_new_work(),
        LocalShardWriteKind::Settlement => state.allows_settlement(),
    };
    if !allowed {
        return Err(LocalShardAdmissionError::RejectedState {
            run_id: admission.run_id,
            run_shard: admission.run_shard,
            database_alias: admission.database_alias.clone(),
            actual_state: state.to_string(),
            write_kind: match write_kind {
                LocalShardWriteKind::NewWork => "new work",
                LocalShardWriteKind::Settlement => "settlement",
            },
            redirect_database_alias: admission.redirect_database_alias.clone(),
        }
        .into());
    }

    Ok(admission)
}

#[cfg(test)]
mod tests {
    use sqlx::PgPool;
    use uuid::Uuid;

    use super::*;

    #[test]
    fn admission_states_separate_new_work_from_owned_settlement() {
        assert!(LocalShardAdmissionState::Open.allows_new_work());
        assert!(LocalShardAdmissionState::Open.allows_settlement());
        assert!(!LocalShardAdmissionState::Draining.allows_new_work());
        assert!(LocalShardAdmissionState::Draining.allows_settlement());
        assert!(!LocalShardAdmissionState::Prepared.allows_new_work());
        assert!(!LocalShardAdmissionState::Prepared.allows_settlement());
        assert!(!LocalShardAdmissionState::Closed.allows_new_work());
        assert!(!LocalShardAdmissionState::Closed.allows_settlement());
    }

    #[sqlx::test(migrations = "../migrations")]
    #[ignore = "requires a PostgreSQL DATABASE_URL for local shard admission tests"]
    async fn stale_write_epoch_is_rejected_without_mutating_local_state(pool: PgPool) {
        let run_id = Uuid::now_v7();
        upsert_local_shard_admission(
            &pool,
            LocalShardAdmissionDraft {
                run_id,
                run_shard: 7,
                database_alias: "primary".to_string(),
                write_epoch: 2,
                state: LocalShardAdmissionState::Open,
                redirect_database_alias: None,
            },
        )
        .await
        .unwrap();

        let error = validate_local_shard_admission(
            &pool,
            &LocalShardRouteHint {
                run_id,
                run_shard: 7,
                database_alias: "primary".to_string(),
                write_epoch: 1,
            },
            LocalShardWriteKind::NewWork,
        )
        .await
        .unwrap_err();

        assert!(matches!(
            error.downcast_ref::<LocalShardAdmissionError>(),
            Some(LocalShardAdmissionError::StaleWriteEpoch {
                expected_write_epoch: 1,
                actual_write_epoch: 2,
                ..
            })
        ));
    }

    #[sqlx::test(migrations = "../migrations")]
    #[ignore = "requires a PostgreSQL DATABASE_URL for local shard admission tests"]
    async fn draining_allows_settlement_but_rejects_new_work(pool: PgPool) {
        let run_id = Uuid::now_v7();
        let hint = LocalShardRouteHint {
            run_id,
            run_shard: 3,
            database_alias: "primary".to_string(),
            write_epoch: 4,
        };
        upsert_local_shard_admission(
            &pool,
            LocalShardAdmissionDraft {
                run_id,
                run_shard: 3,
                database_alias: "primary".to_string(),
                write_epoch: 4,
                state: LocalShardAdmissionState::Draining,
                redirect_database_alias: Some("shard_001".to_string()),
            },
        )
        .await
        .unwrap();

        validate_local_shard_admission(&pool, &hint, LocalShardWriteKind::Settlement)
            .await
            .unwrap();
        let error = validate_local_shard_admission(&pool, &hint, LocalShardWriteKind::NewWork)
            .await
            .unwrap_err();
        assert!(matches!(
            error.downcast_ref::<LocalShardAdmissionError>(),
            Some(LocalShardAdmissionError::RejectedState { .. })
        ));
    }

    #[sqlx::test(migrations = "../migrations")]
    #[ignore = "requires a PostgreSQL DATABASE_URL for local shard admission tests"]
    async fn same_epoch_transition_cannot_reopen_closed_owner(pool: PgPool) {
        let run_id = Uuid::now_v7();
        upsert_local_shard_admission(
            &pool,
            LocalShardAdmissionDraft {
                run_id,
                run_shard: 9,
                database_alias: "primary".to_string(),
                write_epoch: 8,
                state: LocalShardAdmissionState::Closed,
                redirect_database_alias: Some("shard_001".to_string()),
            },
        )
        .await
        .unwrap();

        let error = transition_local_shard_admission(
            &pool,
            LocalShardAdmissionDraft {
                run_id,
                run_shard: 9,
                database_alias: "primary".to_string(),
                write_epoch: 8,
                state: LocalShardAdmissionState::Open,
                redirect_database_alias: None,
            },
            &[LocalShardAdmissionState::Open],
        )
        .await
        .unwrap_err();

        assert!(error.to_string().contains("rejected transition"));
    }
}
