//! Process-local circuit breakers for runtime execution-database work.
//!
//! Breakers are keyed by database alias and affect only runtime admission.
//! Durable routing and work remain authoritative in PostgreSQL, and explicit
//! administrative database access bypasses these transient states.

use std::{
    collections::{
        HashMap,
        hash_map::DefaultHasher,
    },
    hash::{
        Hash,
        Hasher,
    },
    sync::Mutex,
    time::{
        Duration,
        Instant,
    },
};

use uuid::Uuid;

use super::DatabaseOperationTimeout;

const DEFAULT_JITTER_PERCENT: u8 = 20;
pub(crate) const DEFAULT_FAILURE_THRESHOLD: u32 = 3;
pub(crate) const DEFAULT_INITIAL_OPEN: Duration = Duration::from_secs(10);
pub(crate) const DEFAULT_MAX_OPEN: Duration = Duration::from_secs(120);

/// Process-local policy for one circuit per execution database alias.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct CircuitBreakerConfig {
    enabled: bool,
    failure_threshold: u32,
    initial_open: Duration,
    max_open: Duration,
    jitter_percent: u8,
}

impl CircuitBreakerConfig {
    pub(crate) fn new(
        enabled: bool,
        failure_threshold: u32,
        initial_open: Duration,
        max_open: Duration,
    ) -> anyhow::Result<Self> {
        if failure_threshold == 0 {
            anyhow::bail!("database circuit breaker failure threshold must be greater than zero");
        }
        if initial_open.is_zero() {
            anyhow::bail!(
                "database circuit breaker initial open duration must be greater than zero"
            );
        }
        if max_open < initial_open {
            anyhow::bail!(
                "database circuit breaker maximum open duration must be at least the initial open duration"
            );
        }

        Ok(Self {
            enabled,
            failure_threshold,
            initial_open,
            max_open,
            jitter_percent: DEFAULT_JITTER_PERCENT,
        })
    }

    #[cfg(test)]
    fn without_jitter(mut self) -> Self {
        self.jitter_percent = 0;
        self
    }
}

impl Default for CircuitBreakerConfig {
    fn default() -> Self {
        Self::new(
            true,
            DEFAULT_FAILURE_THRESHOLD,
            DEFAULT_INITIAL_OPEN,
            DEFAULT_MAX_OPEN,
        )
        .expect("default database circuit breaker configuration is valid")
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum FailureImpact {
    Available,
    Unavailable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct CircuitOpen {
    pub(crate) retry_after: Duration,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CircuitTransition {
    Opened { retry_after: Duration },
    Closed,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct CircuitPermit {
    database_alias: String,
    generation: u64,
    probe: bool,
}

impl CircuitPermit {
    pub(crate) fn is_probe(&self) -> bool {
        self.probe
    }
}

#[derive(Debug)]
pub(crate) struct DatabaseCircuitBreakers {
    config: CircuitBreakerConfig,
    jitter_seed: u64,
    entries: Mutex<HashMap<String, CircuitEntry>>,
}

impl DatabaseCircuitBreakers {
    pub(crate) fn new(config: CircuitBreakerConfig) -> Self {
        Self {
            config,
            jitter_seed: Uuid::now_v7().as_u128() as u64,
            entries: Mutex::new(HashMap::new()),
        }
    }

    pub(super) fn acquire(
        &self,
        database_alias: &str,
        now: Instant,
    ) -> Result<CircuitPermit, CircuitOpen> {
        if !self.config.enabled {
            return Ok(CircuitPermit {
                database_alias: database_alias.to_string(),
                generation: 0,
                probe: false,
            });
        }
        let mut entries = self
            .entries
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let entry = entries
            .entry(database_alias.to_string())
            .or_insert_with(|| CircuitEntry::new(self.config.initial_open));

        match entry.state {
            CircuitState::Closed => Ok(CircuitPermit {
                database_alias: database_alias.to_string(),
                generation: entry.generation,
                probe: false,
            }),
            CircuitState::Open { retry_at } if now < retry_at => Err(CircuitOpen {
                retry_after: retry_at.duration_since(now),
            }),
            CircuitState::Open { .. } => {
                entry.generation = entry.generation.wrapping_add(1);
                let probe_for = self.jittered_open_for(database_alias, entry);
                entry.state = CircuitState::HalfOpen {
                    probe_until: now + probe_for,
                };
                Ok(CircuitPermit {
                    database_alias: database_alias.to_string(),
                    generation: entry.generation,
                    probe: true,
                })
            }
            CircuitState::HalfOpen { probe_until } if now >= probe_until => {
                entry.generation = entry.generation.wrapping_add(1);
                let probe_for = self.jittered_open_for(database_alias, entry);
                entry.state = CircuitState::HalfOpen {
                    probe_until: now + probe_for,
                };
                Ok(CircuitPermit {
                    database_alias: database_alias.to_string(),
                    generation: entry.generation,
                    probe: true,
                })
            }
            CircuitState::HalfOpen { probe_until } => Err(CircuitOpen {
                retry_after: probe_until.duration_since(now),
            }),
        }
    }

    pub(super) fn record_success(&self, permit: CircuitPermit) -> Option<CircuitTransition> {
        if !self.config.enabled {
            return None;
        }
        let mut entries = self
            .entries
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let entry = entries.get_mut(&permit.database_alias)?;
        if entry.generation != permit.generation {
            return None;
        }

        entry.consecutive_failures = 0;
        entry.open_for = self.config.initial_open;
        if permit.probe {
            entry.generation = entry.generation.wrapping_add(1);
            entry.state = CircuitState::Closed;
            Some(CircuitTransition::Closed)
        } else {
            None
        }
    }

    fn record_failure(
        &self,
        permit: CircuitPermit,
        now: Instant,
        impact: FailureImpact,
    ) -> Option<CircuitTransition> {
        if !self.config.enabled {
            return None;
        }
        if impact == FailureImpact::Available {
            return self.record_success(permit);
        }

        let mut entries = self
            .entries
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let entry = entries.get_mut(&permit.database_alias)?;
        if entry.generation != permit.generation {
            return None;
        }

        match entry.state {
            CircuitState::Closed => {
                entry.consecutive_failures = entry.consecutive_failures.saturating_add(1);
                if entry.consecutive_failures < self.config.failure_threshold {
                    return None;
                }
            }
            CircuitState::HalfOpen { .. } => {
                entry.open_for = entry.open_for.saturating_mul(2).min(self.config.max_open);
            }
            CircuitState::Open { .. } => return None,
        }

        entry.generation = entry.generation.wrapping_add(1);
        let open_for = self.jittered_open_for(&permit.database_alias, entry);
        entry.state = CircuitState::Open {
            retry_at: now + open_for,
        };
        Some(CircuitTransition::Opened {
            retry_after: open_for,
        })
    }

    pub(super) fn record_error(
        &self,
        permit: CircuitPermit,
        now: Instant,
        error: &anyhow::Error,
    ) -> (FailureImpact, Option<CircuitTransition>) {
        let impact = classify_error(error);
        let transition = self.record_failure(permit, now, impact);
        (impact, transition)
    }

    fn jittered_open_for(&self, database_alias: &str, entry: &CircuitEntry) -> Duration {
        if self.config.jitter_percent == 0 {
            return entry.open_for;
        }

        let mut hasher = DefaultHasher::new();
        self.jitter_seed.hash(&mut hasher);
        database_alias.hash(&mut hasher);
        entry.generation.hash(&mut hasher);
        let jitter_range = u64::from(self.config.jitter_percent) + 1;
        let reduction_percent = hasher.finish() % jitter_range;
        entry
            .open_for
            .mul_f64((100 - reduction_percent) as f64 / 100.0)
    }
}

#[derive(Debug)]
struct CircuitEntry {
    state: CircuitState,
    generation: u64,
    consecutive_failures: u32,
    open_for: Duration,
}

impl CircuitEntry {
    fn new(initial_open: Duration) -> Self {
        Self {
            state: CircuitState::Closed,
            generation: 0,
            consecutive_failures: 0,
            open_for: initial_open,
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum CircuitState {
    Closed,
    Open { retry_at: Instant },
    HalfOpen { probe_until: Instant },
}

pub(super) fn classify_error(error: &anyhow::Error) -> FailureImpact {
    if error
        .chain()
        .any(|cause| cause.downcast_ref::<DatabaseOperationTimeout>().is_some())
    {
        return FailureImpact::Unavailable;
    }
    let Some(error) = error
        .chain()
        .find_map(|cause| cause.downcast_ref::<sqlx::Error>())
    else {
        return FailureImpact::Available;
    };

    match error {
        sqlx::Error::Io(_)
        | sqlx::Error::Tls(_)
        | sqlx::Error::Protocol(_)
        | sqlx::Error::PoolTimedOut
        | sqlx::Error::PoolClosed
        | sqlx::Error::WorkerCrashed
        | sqlx::Error::BeginFailed => FailureImpact::Unavailable,
        sqlx::Error::Database(error) => classify_database_error_code(error.code().as_deref()),
        _ => FailureImpact::Available,
    }
}

pub(crate) fn is_database_contention(error: &anyhow::Error) -> bool {
    error
        .chain()
        .find_map(|cause| cause.downcast_ref::<sqlx::Error>())
        .and_then(|error| match error {
            sqlx::Error::Database(error) => error.code(),
            _ => None,
        })
        .is_some_and(|code| is_database_contention_code(Some(code.as_ref())))
}

pub(crate) fn is_database_unavailable(error: &anyhow::Error) -> bool {
    classify_error(error) == FailureImpact::Unavailable
}

fn classify_database_error_code(code: Option<&str>) -> FailureImpact {
    let Some(code) = code else {
        return FailureImpact::Available;
    };

    if code == "57014"
        || code.starts_with("08")
        || code.starts_with("53")
        || matches!(code, "57P01" | "57P02" | "57P03" | "58030")
    {
        FailureImpact::Unavailable
    } else {
        FailureImpact::Available
    }
}

fn is_database_contention_code(code: Option<&str>) -> bool {
    matches!(code, Some("40001" | "40P01" | "55P03"))
}

#[cfg(test)]
mod tests {
    use std::time::{
        Duration,
        Instant,
    };

    use sqlx::PgPool;

    use super::*;

    fn config() -> CircuitBreakerConfig {
        CircuitBreakerConfig::new(true, 2, Duration::from_secs(10), Duration::from_secs(40))
            .unwrap()
            .without_jitter()
    }

    #[test]
    fn availability_failures_open_only_the_affected_database() {
        let breakers = DatabaseCircuitBreakers::new(config());
        let now = Instant::now();

        let first = breakers.acquire("shard_001", now).unwrap();
        let _ = breakers.record_failure(first, now, FailureImpact::Unavailable);
        let second = breakers.acquire("shard_001", now).unwrap();
        let _ = breakers.record_failure(second, now, FailureImpact::Unavailable);

        assert_eq!(
            breakers.acquire("shard_001", now),
            Err(CircuitOpen {
                retry_after: Duration::from_secs(10),
            })
        );
        assert!(breakers.acquire("shard_002", now).is_ok());
    }

    #[test]
    fn available_failures_do_not_open_the_circuit() {
        let breakers = DatabaseCircuitBreakers::new(config());
        let now = Instant::now();

        for _ in 0..3 {
            let permit = breakers.acquire("shard_001", now).unwrap();
            let _ = breakers.record_failure(permit, now, FailureImpact::Available);
        }

        assert!(breakers.acquire("shard_001", now).is_ok());
    }

    #[test]
    fn elapsed_open_circuit_allows_only_one_probe() {
        let breakers = DatabaseCircuitBreakers::new(config());
        let opened_at = Instant::now();
        open(&breakers, "shard_001", opened_at);
        let probe_at = opened_at + Duration::from_secs(10);

        let probe = breakers.acquire("shard_001", probe_at).unwrap();

        assert!(probe.is_probe());
        assert_eq!(
            breakers.acquire("shard_001", probe_at),
            Err(CircuitOpen {
                retry_after: Duration::from_secs(10),
            })
        );
    }

    #[test]
    fn successful_probe_closes_the_circuit() {
        let breakers = DatabaseCircuitBreakers::new(config());
        let opened_at = Instant::now();
        open(&breakers, "shard_001", opened_at);
        let probe_at = opened_at + Duration::from_secs(10);
        let probe = breakers.acquire("shard_001", probe_at).unwrap();

        let _ = breakers.record_success(probe);

        let permit = breakers.acquire("shard_001", probe_at).unwrap();
        assert!(!permit.is_probe());
    }

    #[test]
    fn failed_probes_back_off_to_the_configured_maximum() {
        let breakers = DatabaseCircuitBreakers::new(config());
        let opened_at = Instant::now();
        open(&breakers, "shard_001", opened_at);

        let first_probe_at = opened_at + Duration::from_secs(10);
        let first_probe = breakers.acquire("shard_001", first_probe_at).unwrap();
        let _ = breakers.record_failure(first_probe, first_probe_at, FailureImpact::Unavailable);
        assert_eq!(
            breakers.acquire("shard_001", first_probe_at),
            Err(CircuitOpen {
                retry_after: Duration::from_secs(20),
            })
        );

        let second_probe_at = first_probe_at + Duration::from_secs(20);
        let second_probe = breakers.acquire("shard_001", second_probe_at).unwrap();
        let _ = breakers.record_failure(second_probe, second_probe_at, FailureImpact::Unavailable);
        let third_probe_at = second_probe_at + Duration::from_secs(40);
        let third_probe = breakers.acquire("shard_001", third_probe_at).unwrap();
        let _ = breakers.record_failure(third_probe, third_probe_at, FailureImpact::Unavailable);

        assert_eq!(
            breakers.acquire("shard_001", third_probe_at),
            Err(CircuitOpen {
                retry_after: Duration::from_secs(40),
            })
        );
    }

    #[test]
    fn stale_completion_cannot_close_a_newer_circuit_generation() {
        let breakers = DatabaseCircuitBreakers::new(config());
        let now = Instant::now();
        let stale_success = breakers.acquire("shard_001", now).unwrap();
        open(&breakers, "shard_001", now);

        let _ = breakers.record_success(stale_success);

        assert!(breakers.acquire("shard_001", now).is_err());
    }

    #[test]
    fn concurrent_closed_permits_remain_valid_after_a_success() {
        let breakers = DatabaseCircuitBreakers::new(config());
        let now = Instant::now();
        let success = breakers.acquire("shard_001", now).unwrap();
        let first_failure = breakers.acquire("shard_001", now).unwrap();
        let second_failure = breakers.acquire("shard_001", now).unwrap();

        let _ = breakers.record_success(success);
        let _ = breakers.record_failure(first_failure, now, FailureImpact::Unavailable);
        let _ = breakers.record_failure(second_failure, now, FailureImpact::Unavailable);

        assert!(breakers.acquire("shard_001", now).is_err());
    }

    #[test]
    fn database_failure_codes_distinguish_outages_from_contention() {
        assert_eq!(
            classify_database_error_code(Some("08006")),
            FailureImpact::Unavailable
        );
        assert_eq!(
            classify_database_error_code(Some("57P01")),
            FailureImpact::Unavailable
        );
        assert_eq!(
            classify_database_error_code(Some("40001")),
            FailureImpact::Available
        );
        assert_eq!(
            classify_database_error_code(Some("23505")),
            FailureImpact::Available
        );
        assert_eq!(
            classify_error(&anyhow::Error::new(sqlx::Error::PoolTimedOut)),
            FailureImpact::Unavailable
        );
        assert_eq!(
            classify_database_error_code(Some("57014")),
            FailureImpact::Unavailable
        );
        assert_eq!(
            classify_database_error_code(Some("55P03")),
            FailureImpact::Available
        );
        assert_eq!(
            classify_error(&anyhow::Error::new(DatabaseOperationTimeout::new(
                "shard_001",
                "test",
                Duration::from_secs(30),
            ))),
            FailureImpact::Unavailable
        );
        assert!(is_database_contention_code(Some("40001")));
        assert!(is_database_contention_code(Some("40P01")));
        assert!(is_database_contention_code(Some("55P03")));
        assert!(!is_database_contention_code(Some("57014")));
        assert!(!is_database_contention_code(Some("23505")));
    }

    #[test]
    fn disabled_breakers_never_reject_database_work() {
        let config =
            CircuitBreakerConfig::new(false, 1, Duration::from_secs(10), Duration::from_secs(10))
                .unwrap();
        let breakers = DatabaseCircuitBreakers::new(config);
        let now = Instant::now();

        for _ in 0..3 {
            let permit = breakers.acquire("shard_001", now).unwrap();
            let _ = breakers.record_failure(permit, now, FailureImpact::Unavailable);
        }

        assert!(breakers.acquire("shard_001", now).is_ok());
    }

    #[test]
    fn configuration_rejects_a_maximum_below_the_initial_open_duration() {
        let error =
            CircuitBreakerConfig::new(true, 3, Duration::from_secs(20), Duration::from_secs(10))
                .unwrap_err();

        assert!(error.to_string().contains("maximum open duration"));
    }

    #[sqlx::test(migrations = "../migrations")]
    #[ignore = "requires a PostgreSQL DATABASE_URL for sqlx migration tests"]
    async fn open_database_circuit_does_not_block_healthy_database_work(pool: PgPool) {
        let breakers = DatabaseCircuitBreakers::new(config());
        let now = Instant::now();
        open(&breakers, "shard_001", now);

        assert!(breakers.acquire("shard_001", now).is_err());
        let healthy = breakers.acquire("primary", now).unwrap();
        let value = sqlx::query_scalar::<_, i32>("SELECT 1")
            .fetch_one(&pool)
            .await
            .unwrap();
        let _ = breakers.record_success(healthy);

        assert_eq!(value, 1);
        assert!(breakers.acquire("primary", now).is_ok());
    }

    #[sqlx::test(migrations = "../migrations")]
    #[ignore = "requires a PostgreSQL DATABASE_URL for sqlx migration tests"]
    async fn successful_database_probe_restores_normal_admission(pool: PgPool) {
        let breakers = DatabaseCircuitBreakers::new(config());
        let opened_at = Instant::now();
        open(&breakers, "primary", opened_at);
        let probe_at = opened_at + Duration::from_secs(10);
        let probe = breakers.acquire("primary", probe_at).unwrap();

        sqlx::query("SELECT 1").execute(&pool).await.unwrap();
        let _ = breakers.record_success(probe);

        assert!(!breakers.acquire("primary", probe_at).unwrap().is_probe());
    }

    fn open(breakers: &DatabaseCircuitBreakers, alias: &str, now: Instant) {
        for _ in 0..2 {
            let permit = breakers.acquire(alias, now).unwrap();
            let _ = breakers.record_failure(permit, now, FailureImpact::Unavailable);
        }
    }
}
