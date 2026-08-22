use std::time::{Duration, Instant};

use dashmap::DashMap;

#[derive(Debug)]
pub struct CircuitBreaker {
    states: DashMap<String, CircuitState>,
    failure_threshold: u32,
    recovery_timeout: Duration,
}

impl CircuitBreaker {
    pub fn new(failure_threshold: u32, recovery_timeout: Duration) -> Self {
        Self {
            states: DashMap::new(),
            failure_threshold,
            recovery_timeout,
        }
    }

    pub fn allow(&self, repository: &str) -> bool {
        let mut state = self.states.entry(repository.to_owned()).or_default();
        match state.mode {
            CircuitMode::Closed => true,
            CircuitMode::Open { since } if since.elapsed() >= self.recovery_timeout => {
                state.mode = CircuitMode::HalfOpen { in_flight: true };
                true
            }
            CircuitMode::Open { .. } => false,
            CircuitMode::HalfOpen { in_flight: false } => {
                state.mode = CircuitMode::HalfOpen { in_flight: true };
                true
            }
            CircuitMode::HalfOpen { in_flight: true } => false,
        }
    }

    pub fn record_success(&self, repository: &str) {
        self.states
            .insert(repository.to_owned(), CircuitState::default());
    }

    pub fn record_failure(&self, repository: &str) {
        let mut state = self.states.entry(repository.to_owned()).or_default();
        match state.mode {
            CircuitMode::HalfOpen { .. } => {
                state.failures = self.failure_threshold;
                state.mode = CircuitMode::Open {
                    since: Instant::now(),
                };
            }
            CircuitMode::Open { .. } => {}
            CircuitMode::Closed => {
                state.failures = state.failures.saturating_add(1);
                if state.failures >= self.failure_threshold {
                    state.mode = CircuitMode::Open {
                        since: Instant::now(),
                    };
                }
            }
        }
    }

    pub fn status(&self, repository: &str) -> CircuitStatus {
        let Some(state) = self.states.get(repository) else {
            return CircuitStatus {
                state: "closed",
                failures: 0,
            };
        };
        CircuitStatus {
            state: match state.mode {
                CircuitMode::Closed => "closed",
                CircuitMode::Open { .. } => "open",
                CircuitMode::HalfOpen { .. } => "half_open",
            },
            failures: state.failures,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CircuitStatus {
    pub state: &'static str,
    pub failures: u32,
}

#[derive(Debug)]
struct CircuitState {
    failures: u32,
    mode: CircuitMode,
}

impl Default for CircuitState {
    fn default() -> Self {
        Self {
            failures: 0,
            mode: CircuitMode::Closed,
        }
    }
}

#[derive(Clone, Copy, Debug)]
enum CircuitMode {
    Closed,
    Open { since: Instant },
    HalfOpen { in_flight: bool },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opens_at_threshold_and_success_resets_failures() {
        let breaker = CircuitBreaker::new(2, Duration::from_secs(60));
        assert!(breaker.allow("central"));
        breaker.record_failure("central");
        assert!(breaker.allow("central"));
        breaker.record_success("central");
        breaker.record_failure("central");
        assert!(breaker.allow("central"));
        breaker.record_failure("central");
        assert!(!breaker.allow("central"));
    }

    #[test]
    fn half_open_allows_only_one_probe() {
        let breaker = CircuitBreaker::new(1, Duration::ZERO);
        breaker.record_failure("central");
        assert!(breaker.allow("central"));
        assert!(!breaker.allow("central"));
        breaker.record_success("central");
        assert!(breaker.allow("central"));
    }

    #[test]
    fn reports_current_status_without_mutating_unknown_repositories() {
        let breaker = CircuitBreaker::new(1, Duration::from_secs(60));
        assert_eq!(
            breaker.status("central"),
            CircuitStatus {
                state: "closed",
                failures: 0
            }
        );
        breaker.record_failure("central");
        assert_eq!(breaker.status("central").state, "open");
    }
}
