//! Execution-local shard write admission.
//!
//! Runtime authority is fenced by route epoch and lifecycle state. A prepared
//! move target additionally requires the exact move ID, monotonic claim
//! generation, and opaque claim token in every target mutation transaction.

use std::fmt;

use sqlx::{
    Executor,
    Postgres,
};
use uuid::Uuid;

mod queries;

pub(crate) use queries::{
    install_local_shard_move_fence,
    select_local_shard_admission,
    transition_local_shard_admission,
    upsert_local_shard_admission,
};

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
    pub(crate) move_fence: Option<LocalShardMoveFence>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct LocalShardMoveFence {
    pub(crate) move_id: Uuid,
    pub(crate) claim_generation: i64,
    pub(crate) claim_token: Uuid,
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
    move_id: Option<Uuid>,
    move_claim_generation: Option<i64>,
    move_claim_token: Option<Uuid>,
}

impl LocalShardAdmission {
    pub(crate) fn parsed_state(&self) -> anyhow::Result<LocalShardAdmissionState> {
        LocalShardAdmissionState::try_from(self.state.as_str())
    }

    pub(crate) fn move_fence(&self) -> Option<LocalShardMoveFence> {
        Some(LocalShardMoveFence {
            move_id: self.move_id?,
            claim_generation: self.move_claim_generation?,
            claim_token: self.move_claim_token?,
        })
    }
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
    #[error(
        "stale shard mover for run {run_id} shard {run_shard} on {database_alias}; expected move {expected_move_id} generation {expected_generation} token {expected_token}, local authority is {actual_authority}"
    )]
    StaleMoveClaim {
        run_id: Uuid,
        run_shard: i16,
        database_alias: String,
        expected_move_id: Uuid,
        expected_generation: i64,
        expected_token: Uuid,
        actual_authority: String,
    },
}

pub(crate) async fn validate_local_shard_move_fence<'e, E>(
    executor: E,
    run_id: Uuid,
    run_shard: i16,
    database_alias: &str,
    write_epoch: i64,
    expected: LocalShardMoveFence,
) -> anyhow::Result<LocalShardAdmission>
where
    E: Executor<'e, Database = Postgres>,
{
    let admission = select_local_shard_admission(executor, run_id, run_shard)
        .await?
        .ok_or_else(|| LocalShardAdmissionError::Missing {
            run_id,
            run_shard,
            database_alias: database_alias.to_string(),
        })?;
    let actual = admission.move_fence();
    if admission.database_alias != database_alias
        || admission.write_epoch != write_epoch
        || admission.parsed_state()? != LocalShardAdmissionState::Prepared
        || actual != Some(expected)
    {
        return Err(LocalShardAdmissionError::StaleMoveClaim {
            run_id,
            run_shard,
            database_alias: database_alias.to_string(),
            expected_move_id: expected.move_id,
            expected_generation: expected.claim_generation,
            expected_token: expected.claim_token,
            actual_authority: actual.map_or_else(
                || "none".to_string(),
                |actual| {
                    format!(
                        "move {} generation {} token {}",
                        actual.move_id, actual.claim_generation, actual.claim_token
                    )
                },
            ),
        }
        .into());
    }
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

    validate_loaded_local_shard_admission(admission, hint, write_kind)
}

fn validate_loaded_local_shard_admission(
    admission: LocalShardAdmission,
    hint: &LocalShardRouteHint,
    write_kind: LocalShardWriteKind,
) -> anyhow::Result<LocalShardAdmission> {
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
#[path = "local_shard_admission/postgres_tests.rs"]
mod postgres_tests;
#[cfg(test)]
mod tests {
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

    #[test]
    fn admission_states_round_trip_through_persisted_values() {
        for state in [
            LocalShardAdmissionState::Open,
            LocalShardAdmissionState::Draining,
            LocalShardAdmissionState::Prepared,
            LocalShardAdmissionState::Closed,
        ] {
            assert_eq!(
                LocalShardAdmissionState::try_from(state.as_str()).unwrap(),
                state
            );
            assert_eq!(state.to_string(), state.as_str());
        }

        for invalid in ["", "OPEN", "unknown", " open"] {
            assert!(LocalShardAdmissionState::try_from(invalid).is_err());
        }
    }

    #[test]
    fn loaded_admission_rejects_invalid_persisted_state() {
        let hint = route_hint("primary", 3);
        let error = validate_loaded_local_shard_admission(
            admission("invalid", "primary", 3, None),
            &hint,
            LocalShardWriteKind::NewWork,
        )
        .unwrap_err();

        assert!(
            error
                .to_string()
                .contains("unsupported local shard admission state")
        );
    }

    #[test]
    fn loaded_admission_rejects_stale_alias_or_epoch() {
        let hint = route_hint("primary", 3);

        for stale in [
            admission("open", "shard_001", 3, None),
            admission("open", "primary", 2, Some("shard_001")),
            admission("open", "primary", 4, None),
        ] {
            let error =
                validate_loaded_local_shard_admission(stale, &hint, LocalShardWriteKind::NewWork)
                    .unwrap_err();
            assert!(matches!(
                error.downcast_ref::<LocalShardAdmissionError>(),
                Some(LocalShardAdmissionError::StaleWriteEpoch { .. })
            ));
        }
    }

    #[test]
    fn loaded_admission_applies_write_policy_after_route_validation() {
        let hint = route_hint("primary", 3);
        assert!(
            validate_loaded_local_shard_admission(
                admission("open", "primary", 3, None),
                &hint,
                LocalShardWriteKind::NewWork,
            )
            .is_ok()
        );
        assert!(
            validate_loaded_local_shard_admission(
                admission("draining", "primary", 3, Some("shard_001")),
                &hint,
                LocalShardWriteKind::Settlement,
            )
            .is_ok()
        );

        for (state, write_kind) in [
            ("draining", LocalShardWriteKind::NewWork),
            ("prepared", LocalShardWriteKind::NewWork),
            ("prepared", LocalShardWriteKind::Settlement),
            ("closed", LocalShardWriteKind::Settlement),
        ] {
            let error = validate_loaded_local_shard_admission(
                admission(state, "primary", 3, Some("shard_001")),
                &hint,
                write_kind,
            )
            .unwrap_err();
            assert!(matches!(
                error.downcast_ref::<LocalShardAdmissionError>(),
                Some(LocalShardAdmissionError::RejectedState {
                    redirect_database_alias: Some(alias),
                    ..
                }) if alias == "shard_001"
            ));
        }
    }

    fn route_hint(database_alias: &str, write_epoch: i64) -> LocalShardRouteHint {
        LocalShardRouteHint {
            run_id: Uuid::nil(),
            run_shard: 7,
            database_alias: database_alias.to_string(),
            write_epoch,
        }
    }

    fn admission(
        state: &str,
        database_alias: &str,
        write_epoch: i64,
        redirect_database_alias: Option<&str>,
    ) -> LocalShardAdmission {
        LocalShardAdmission {
            run_id: Uuid::nil(),
            run_shard: 7,
            database_alias: database_alias.to_string(),
            write_epoch,
            state: state.to_string(),
            redirect_database_alias: redirect_database_alias.map(ToOwned::to_owned),
            move_id: None,
            move_claim_generation: None,
            move_claim_token: None,
        }
    }
}
