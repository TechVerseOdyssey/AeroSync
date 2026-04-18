//! Circuit breaker for HTTP/QUIC transfers.
//!
//! Three states: Closed (normal) → Open (failing fast) → HalfOpen (probing).
//!
//! # State transitions
//! - Closed → Open: consecutive failure count reaches `failure_threshold`
//! - Open → HalfOpen: `reset_timeout` has elapsed
//! - HalfOpen → Closed: one probe request succeeds
//! - HalfOpen → Open: probe request fails

use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Circuit breaker state
#[derive(Debug, Clone)]
pub enum CircuitState {
    /// Normal operation; calls are forwarded.
    Closed,
    /// Too many failures; calls are rejected immediately.
    Open { opened_at: Instant },
    /// Trying one probe request to see if the service recovered.
    HalfOpen,
}

/// Thread-safe circuit breaker.
pub struct CircuitBreaker {
    state: Mutex<CircuitState>,
    consecutive_failures: Mutex<u32>,
    failure_threshold: u32,
    reset_timeout: Duration,
}

impl std::fmt::Debug for CircuitBreaker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let state = self.state.lock().unwrap();
        let failures = self.consecutive_failures.lock().unwrap();
        f.debug_struct("CircuitBreaker")
            .field("state", &*state)
            .field("consecutive_failures", &*failures)
            .field("failure_threshold", &self.failure_threshold)
            .field("reset_timeout", &self.reset_timeout)
            .finish()
    }
}

impl CircuitBreaker {
    /// Create a new circuit breaker.
    ///
    /// - `failure_threshold`: consecutive failures before opening (default 5)
    /// - `reset_timeout`: time to wait before probing after opening (default 30s)
    pub fn new(failure_threshold: u32, reset_timeout: Duration) -> Self {
        Self {
            state: Mutex::new(CircuitState::Closed),
            consecutive_failures: Mutex::new(0),
            failure_threshold,
            reset_timeout,
        }
    }

    pub fn with_defaults() -> Self {
        Self::new(5, Duration::from_secs(30))
    }

    /// Returns true if the circuit allows the call to proceed.
    pub fn allow_request(&self) -> bool {
        let mut state = self.state.lock().unwrap();
        match &*state {
            CircuitState::Closed => true,
            CircuitState::Open { opened_at } => {
                if opened_at.elapsed() >= self.reset_timeout {
                    tracing::info!("Circuit breaker: Open → HalfOpen (probing)");
                    *state = CircuitState::HalfOpen;
                    true
                } else {
                    false
                }
            }
            CircuitState::HalfOpen => true, // allow probe
        }
    }

    /// Record a successful call. Resets to Closed.
    pub fn record_success(&self) {
        let mut state = self.state.lock().unwrap();
        if !matches!(*state, CircuitState::Closed) {
            tracing::info!("Circuit breaker: {:?} → Closed (success)", *state);
        }
        *state = CircuitState::Closed;
        *self.consecutive_failures.lock().unwrap() = 0;
    }

    /// Record a failed call. May open the circuit.
    pub fn record_failure(&self) {
        let mut failures = self.consecutive_failures.lock().unwrap();
        *failures += 1;
        if *failures >= self.failure_threshold {
            let mut state = self.state.lock().unwrap();
            if !matches!(*state, CircuitState::Open { .. }) {
                tracing::warn!(
                    "Circuit breaker: opening after {} consecutive failures",
                    failures
                );
                *state = CircuitState::Open {
                    opened_at: Instant::now(),
                };
            }
        }
    }

    /// Returns the current state name for logging/metrics.
    pub fn state_name(&self) -> &'static str {
        let state = self.state.lock().unwrap();
        match &*state {
            CircuitState::Closed => "closed",
            CircuitState::Open { .. } => "open",
            CircuitState::HalfOpen => "half_open",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_circuit_starts_closed() {
        let cb = CircuitBreaker::with_defaults();
        assert!(cb.allow_request());
        assert_eq!(cb.state_name(), "closed");
    }

    #[test]
    fn test_circuit_opens_after_threshold() {
        let cb = CircuitBreaker::new(3, Duration::from_secs(60));
        cb.record_failure();
        cb.record_failure();
        assert_eq!(cb.state_name(), "closed"); // still under threshold
        cb.record_failure();
        assert_eq!(cb.state_name(), "open");
        assert!(!cb.allow_request());
    }

    #[test]
    fn test_circuit_transitions_to_half_open_after_timeout() {
        let cb = CircuitBreaker::new(1, Duration::from_millis(10));
        cb.record_failure();
        assert_eq!(cb.state_name(), "open");
        std::thread::sleep(Duration::from_millis(20));
        assert!(cb.allow_request()); // timeout expired → HalfOpen → allow probe
        assert_eq!(cb.state_name(), "half_open");
    }

    #[test]
    fn test_success_resets_to_closed() {
        let cb = CircuitBreaker::new(1, Duration::from_millis(10));
        cb.record_failure();
        std::thread::sleep(Duration::from_millis(20));
        cb.allow_request(); // transition to HalfOpen
        cb.record_success();
        assert_eq!(cb.state_name(), "closed");
        assert!(cb.allow_request());
    }

    #[test]
    fn test_failure_in_half_open_reopens() {
        let cb = CircuitBreaker::new(1, Duration::from_millis(10));
        cb.record_failure(); // open
        std::thread::sleep(Duration::from_millis(20));
        cb.allow_request(); // → half_open
        cb.record_failure(); // probe failed → reopen
                             // failure_threshold is 1, counter is already 2
        assert_eq!(cb.state_name(), "open");
        assert!(!cb.allow_request());
    }
}
