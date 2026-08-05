//! Process-local circuit-breaker state shared by external dependencies.
//!
//! Dependency modules own error classification and operational policy. This
//! module only controls keyed admission and closed, open, and half-open state.

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

const DEFAULT_JITTER_PERCENT: u8 = 20;
pub(crate) const DEFAULT_FAILURE_THRESHOLD: u32 = 3;
pub(crate) const DEFAULT_INITIAL_OPEN: Duration = Duration::from_secs(10);
pub(crate) const DEFAULT_MAX_OPEN: Duration = Duration::from_secs(120);

/// Process-local circuit-breaker policy for one or more dependency keys.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Config {
    enabled: bool,
    failure_threshold: u32,
    initial_open: Duration,
    max_open: Duration,
    jitter_percent: u8,
}

impl Config {
    pub(crate) fn new(
        enabled: bool,
        failure_threshold: u32,
        initial_open: Duration,
        max_open: Duration,
    ) -> anyhow::Result<Self> {
        if failure_threshold == 0 {
            anyhow::bail!("circuit breaker failure threshold must be greater than zero");
        }
        if initial_open.is_zero() {
            anyhow::bail!("circuit breaker initial open duration must be greater than zero");
        }
        if max_open < initial_open {
            anyhow::bail!(
                "circuit breaker maximum open duration must be at least the initial open duration"
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
    pub(crate) fn without_jitter(mut self) -> Self {
        self.jitter_percent = 0;
        self
    }
}

impl Default for Config {
    fn default() -> Self {
        Self::new(
            true,
            DEFAULT_FAILURE_THRESHOLD,
            DEFAULT_INITIAL_OPEN,
            DEFAULT_MAX_OPEN,
        )
        .expect("default circuit breaker configuration is valid")
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FailureImpact {
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
    key: String,
    generation: u64,
    probe: bool,
}

impl CircuitPermit {
    pub(crate) fn is_probe(&self) -> bool {
        self.probe
    }
}

/// Independent circuit state keyed by a dependency-defined resource name.
#[derive(Debug)]
pub(crate) struct CircuitBreakers {
    config: Config,
    jitter_seed: u64,
    entries: Mutex<HashMap<String, CircuitEntry>>,
}

impl CircuitBreakers {
    pub(crate) fn new(config: Config) -> Self {
        Self {
            config,
            jitter_seed: Uuid::now_v7().as_u128() as u64,
            entries: Mutex::new(HashMap::new()),
        }
    }

    pub(crate) fn acquire(&self, key: &str, now: Instant) -> Result<CircuitPermit, CircuitOpen> {
        if !self.config.enabled {
            return Ok(CircuitPermit {
                key: key.to_string(),
                generation: 0,
                probe: false,
            });
        }
        let mut entries = self
            .entries
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let entry = entries
            .entry(key.to_string())
            .or_insert_with(|| CircuitEntry::new(self.config.initial_open));

        match entry.state {
            CircuitState::Closed => Ok(CircuitPermit {
                key: key.to_string(),
                generation: entry.generation,
                probe: false,
            }),
            CircuitState::Open { retry_at } if now < retry_at => Err(CircuitOpen {
                retry_after: retry_at.duration_since(now),
            }),
            CircuitState::Open { .. } => {
                entry.generation = entry.generation.wrapping_add(1);
                let probe_for = self.jittered_open_for(key, entry);
                entry.state = CircuitState::HalfOpen {
                    probe_until: now + probe_for,
                };
                Ok(CircuitPermit {
                    key: key.to_string(),
                    generation: entry.generation,
                    probe: true,
                })
            }
            CircuitState::HalfOpen { probe_until } if now >= probe_until => {
                entry.generation = entry.generation.wrapping_add(1);
                let probe_for = self.jittered_open_for(key, entry);
                entry.state = CircuitState::HalfOpen {
                    probe_until: now + probe_for,
                };
                Ok(CircuitPermit {
                    key: key.to_string(),
                    generation: entry.generation,
                    probe: true,
                })
            }
            CircuitState::HalfOpen { probe_until } => Err(CircuitOpen {
                retry_after: probe_until.duration_since(now),
            }),
        }
    }

    pub(crate) fn record_success(&self, permit: CircuitPermit) -> Option<CircuitTransition> {
        if !self.config.enabled {
            return None;
        }
        let mut entries = self
            .entries
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let entry = entries.get_mut(&permit.key)?;
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

    pub(crate) fn record_failure(
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
        let entry = entries.get_mut(&permit.key)?;
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
        let open_for = self.jittered_open_for(&permit.key, entry);
        entry.state = CircuitState::Open {
            retry_at: now + open_for,
        };
        Some(CircuitTransition::Opened {
            retry_after: open_for,
        })
    }

    fn jittered_open_for(&self, key: &str, entry: &CircuitEntry) -> Duration {
        if self.config.jitter_percent == 0 {
            return entry.open_for;
        }

        let mut hasher = DefaultHasher::new();
        self.jitter_seed.hash(&mut hasher);
        key.hash(&mut hasher);
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

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> Config {
        Config::new(true, 2, Duration::from_secs(10), Duration::from_secs(40))
            .unwrap()
            .without_jitter()
    }

    #[test]
    fn availability_failures_open_only_the_affected_resource() {
        let breakers = CircuitBreakers::new(config());
        let now = Instant::now();

        open(&breakers, "resource-1", now);

        assert_eq!(
            breakers.acquire("resource-1", now),
            Err(CircuitOpen {
                retry_after: Duration::from_secs(10),
            })
        );
        assert!(breakers.acquire("resource-2", now).is_ok());
    }

    #[test]
    fn available_failures_do_not_open_the_circuit() {
        let breakers = CircuitBreakers::new(config());
        let now = Instant::now();

        for _ in 0..3 {
            let permit = breakers.acquire("resource", now).unwrap();
            let _ = breakers.record_failure(permit, now, FailureImpact::Available);
        }

        assert!(breakers.acquire("resource", now).is_ok());
    }

    #[test]
    fn elapsed_open_circuit_allows_only_one_probe() {
        let breakers = CircuitBreakers::new(config());
        let opened_at = Instant::now();
        open(&breakers, "resource", opened_at);
        let probe_at = opened_at + Duration::from_secs(10);

        let probe = breakers.acquire("resource", probe_at).unwrap();

        assert!(probe.is_probe());
        assert_eq!(
            breakers.acquire("resource", probe_at),
            Err(CircuitOpen {
                retry_after: Duration::from_secs(10),
            })
        );
    }

    #[test]
    fn abandoned_probe_is_replaced_and_stale_completion_is_ignored() {
        let breakers = CircuitBreakers::new(config());
        let opened_at = Instant::now();
        open(&breakers, "resource", opened_at);
        let first_probe_at = opened_at + Duration::from_secs(10);
        let stale_probe = breakers.acquire("resource", first_probe_at).unwrap();
        let replacement_at = first_probe_at + Duration::from_secs(10);

        let replacement = breakers.acquire("resource", replacement_at).unwrap();
        assert!(replacement.is_probe());
        let _ = breakers.record_success(stale_probe);
        assert!(breakers.acquire("resource", replacement_at).is_err());

        let _ = breakers.record_success(replacement);
        assert!(
            !breakers
                .acquire("resource", replacement_at)
                .unwrap()
                .is_probe()
        );
    }

    #[test]
    fn successful_probe_closes_the_circuit() {
        let breakers = CircuitBreakers::new(config());
        let opened_at = Instant::now();
        open(&breakers, "resource", opened_at);
        let probe_at = opened_at + Duration::from_secs(10);
        let probe = breakers.acquire("resource", probe_at).unwrap();

        let _ = breakers.record_success(probe);

        assert!(!breakers.acquire("resource", probe_at).unwrap().is_probe());
    }

    #[test]
    fn failed_probes_back_off_to_the_configured_maximum() {
        let breakers = CircuitBreakers::new(config());
        let opened_at = Instant::now();
        open(&breakers, "resource", opened_at);

        let first_probe_at = opened_at + Duration::from_secs(10);
        let first_probe = breakers.acquire("resource", first_probe_at).unwrap();
        let _ = breakers.record_failure(first_probe, first_probe_at, FailureImpact::Unavailable);
        assert_eq!(
            breakers.acquire("resource", first_probe_at),
            Err(CircuitOpen {
                retry_after: Duration::from_secs(20),
            })
        );

        let second_probe_at = first_probe_at + Duration::from_secs(20);
        let second_probe = breakers.acquire("resource", second_probe_at).unwrap();
        let _ = breakers.record_failure(second_probe, second_probe_at, FailureImpact::Unavailable);
        let third_probe_at = second_probe_at + Duration::from_secs(40);
        let third_probe = breakers.acquire("resource", third_probe_at).unwrap();
        let _ = breakers.record_failure(third_probe, third_probe_at, FailureImpact::Unavailable);

        assert_eq!(
            breakers.acquire("resource", third_probe_at),
            Err(CircuitOpen {
                retry_after: Duration::from_secs(40),
            })
        );
    }

    #[test]
    fn stale_completion_cannot_close_a_newer_circuit_generation() {
        let breakers = CircuitBreakers::new(config());
        let now = Instant::now();
        let stale_success = breakers.acquire("resource", now).unwrap();
        open(&breakers, "resource", now);

        let _ = breakers.record_success(stale_success);

        assert!(breakers.acquire("resource", now).is_err());
    }

    #[test]
    fn concurrent_closed_permits_remain_valid_after_a_success() {
        let breakers = CircuitBreakers::new(config());
        let now = Instant::now();
        let success = breakers.acquire("resource", now).unwrap();
        let first_failure = breakers.acquire("resource", now).unwrap();
        let second_failure = breakers.acquire("resource", now).unwrap();

        let _ = breakers.record_success(success);
        let _ = breakers.record_failure(first_failure, now, FailureImpact::Unavailable);
        let _ = breakers.record_failure(second_failure, now, FailureImpact::Unavailable);

        assert!(breakers.acquire("resource", now).is_err());
    }

    #[test]
    fn disabled_breakers_never_reject_work() {
        let config =
            Config::new(false, 1, Duration::from_secs(10), Duration::from_secs(10)).unwrap();
        let breakers = CircuitBreakers::new(config);
        let now = Instant::now();

        for _ in 0..3 {
            let permit = breakers.acquire("resource", now).unwrap();
            let _ = breakers.record_failure(permit, now, FailureImpact::Unavailable);
        }

        assert!(breakers.acquire("resource", now).is_ok());
    }

    #[test]
    fn configuration_rejects_a_maximum_below_the_initial_open_duration() {
        let error =
            Config::new(true, 3, Duration::from_secs(20), Duration::from_secs(10)).unwrap_err();

        assert!(error.to_string().contains("maximum open duration"));
    }

    fn open(breakers: &CircuitBreakers, key: &str, now: Instant) {
        for _ in 0..2 {
            let permit = breakers.acquire(key, now).unwrap();
            let _ = breakers.record_failure(permit, now, FailureImpact::Unavailable);
        }
    }
}
