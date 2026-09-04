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

    /// Whether a request to `repository` may proceed. In the half-open state
    /// exactly one probe is admitted; a probe whose lease (the recovery
    /// timeout) expires without reporting an outcome — for example because the
    /// requesting task was canceled mid-download — releases the slot so a new
    /// probe can run and the repository cannot stay wedged.
    pub fn allow(&self, repository: &str) -> bool {
        let mut state = self.states.entry(repository.to_owned()).or_default();
        match state.mode {
            CircuitMode::Closed => true,
            CircuitMode::Open { since } if since.elapsed() >= self.recovery_timeout => {
                state.mode = CircuitMode::HalfOpen {
                    probe_since: Instant::now(),
                };
                true
            }
            CircuitMode::Open { .. } => false,
            CircuitMode::HalfOpen { probe_since }
                if probe_since.elapsed() >= self.recovery_timeout =>
            {
                state.mode = CircuitMode::HalfOpen {
                    probe_since: Instant::now(),
                };
                true
            }
            CircuitMode::HalfOpen { .. } => false,
        }
    }

    /// Records a successful upstream exchange. A success observed while the
    /// circuit is open (for example a long download that was admitted before
    /// the circuit opened) leaves the open circuit and its failure count
    /// untouched.
    pub fn record_success(&self, repository: &str) {
        let mut state = self.states.entry(repository.to_owned()).or_default();
        if matches!(state.mode, CircuitMode::Open { .. }) {
            return;
        }
        *state = CircuitState::default();
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
    HalfOpen { probe_since: Instant },
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
        let breaker = CircuitBreaker::new(1, Duration::from_millis(20));
        breaker.record_failure("central");
        std::thread::sleep(Duration::from_millis(40));
        assert!(breaker.allow("central"));
        assert!(!breaker.allow("central"));
        breaker.record_success("central");
        assert!(breaker.allow("central"));
    }

    #[test]
    fn abandoned_half_open_probe_releases_the_slot_after_the_recovery_timeout() {
        let breaker = CircuitBreaker::new(1, Duration::from_millis(20));
        breaker.record_failure("central");
        std::thread::sleep(Duration::from_millis(40));
        assert!(breaker.allow("central"));
        assert!(!breaker.allow("central"));
        std::thread::sleep(Duration::from_millis(40));
        assert!(breaker.allow("central"));
    }

    #[test]
    fn success_does_not_close_an_open_circuit() {
        let breaker = CircuitBreaker::new(1, Duration::from_secs(60));
        assert!(breaker.allow("central"));
        breaker.record_failure("central");
        assert!(!breaker.allow("central"));
        breaker.record_success("central");
        assert_eq!(breaker.status("central").state, "open");
        assert!(!breaker.allow("central"));
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
