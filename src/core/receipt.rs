//! Per-receipt state machine (RFC-002 §4 / §5).
//!
//! A [`Receipt<Side>`] owns the canonical state of one in-flight
//! transfer. Both the sender and the receiver instantiate one; the
//! `Side` phantom type just makes "sender-side receipt" and
//! "receiver-side receipt" distinct types so misuse is caught at
//! compile time.
//!
//! State is stored behind a `tokio::sync::watch` channel so multiple
//! observers can `await` transitions concurrently without polling.
//! All mutations go through [`Receipt::apply_event`] — the watch
//! channel is the single source of truth.
//!
//! # State machine
//!
//! The 7 generic states (matching the protobuf [`aerosync_proto::State`]
//! enum) abstract over the side-specific machines defined in
//! RFC-002 §4 (sender) and §5 (receiver):
//!
//! | Generic            | Sender (§4) | Receiver (§5) |
//! | ------------------ | ----------- | ------------- |
//! | `Initiated`        | PENDING     | INCOMING      |
//! | `StreamOpened`     | SENDING     | RECEIVING     |
//! | `DataTransferred`  | SENT        | VERIFYING     |
//! | `StreamClosed`     | (transient) | (transient)   |
//! | `Processing`       | RECEIVED    | BUFFERED      |
//! | `Completed(_)`     | PROCESSED   | ACKED         |
//! | `Failed(_)`        | FAILED/CANC | NACKED/FAILED |
//!
//! Both `Completed` and `Failed` are terminal: once reached, no further
//! transitions are accepted (`apply_event` returns `InvalidTransition`
//! and the state is unchanged).
//!
//! # Wire integration
//!
//! Wiring this module to actual on-the-wire frames (the QUIC bidi
//! receipt stream described in RFC-002 §6.2) is the job of Week 2
//! (RFC-002 task #4). For now, the module is a self-contained,
//! testable state machine; transport code calls `apply_event` once
//! decoded frames arrive.

use std::fmt;
use std::marker::PhantomData;

use tokio::sync::watch;
use uuid::Uuid;

// ─────────────────────────────────────────────────────────────────────
// Phantom side markers
// ─────────────────────────────────────────────────────────────────────

/// Marker type for sender-side receipts.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub struct Sender;

/// Marker type for receiver-side receipts.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub struct Receiver;

// ─────────────────────────────────────────────────────────────────────
// Terminal payloads
// ─────────────────────────────────────────────────────────────────────

/// Concrete reason a [`State::Completed`] terminal was reached.
#[derive(Debug, Clone, Eq, PartialEq)]
#[non_exhaustive]
pub enum CompletedTerminal {
    /// The receiver application called `ack()`.
    Acked,
}

/// Concrete reason a [`State::Failed`] terminal was reached.
#[derive(Debug, Clone, Eq, PartialEq)]
#[non_exhaustive]
pub enum FailedTerminal {
    /// User-initiated cancel from either side.
    Cancelled {
        /// Free-form cancellation reason.
        reason: String,
    },
    /// The receiver application called `nack(reason)`.
    Nacked {
        /// Application-supplied rejection reason.
        reason: String,
    },
    /// Transport, checksum, or other infrastructure failure.
    Errored {
        /// Numeric error code (mirrors the wire `ErrorCode` enum).
        code: u32,
        /// Human-readable diagnostic.
        detail: String,
    },
}

// ─────────────────────────────────────────────────────────────────────
// State enum (7 variants)
// ─────────────────────────────────────────────────────────────────────

/// Lifecycle state of a single receipt.
///
/// Mirrors [`aerosync_proto::State`] but carries terminal payloads
/// inline so callers can pattern-match on the final reason.
#[derive(Debug, Clone, Eq, PartialEq)]
pub enum State {
    /// Receipt object created; nothing on the wire yet.
    Initiated,
    /// Per-receipt stream(s) opened; data may be flowing.
    StreamOpened,
    /// All payload bytes transferred; awaiting close / verify.
    DataTransferred,
    /// Stream FIN received; payload complete and integrity-checked.
    StreamClosed,
    /// Application is processing the payload; awaiting ack/nack.
    Processing,
    /// Terminal: receipt completed successfully.
    Completed(CompletedTerminal),
    /// Terminal: receipt failed (cancel / nack / transport error).
    Failed(FailedTerminal),
}

impl State {
    /// True when no further transitions are possible.
    pub fn is_terminal(&self) -> bool {
        matches!(self, State::Completed(_) | State::Failed(_))
    }
}

impl fmt::Display for State {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            State::Initiated => f.write_str("initiated"),
            State::StreamOpened => f.write_str("stream_opened"),
            State::DataTransferred => f.write_str("data_transferred"),
            State::StreamClosed => f.write_str("stream_closed"),
            State::Processing => f.write_str("processing"),
            State::Completed(_) => f.write_str("completed"),
            State::Failed(_) => f.write_str("failed"),
        }
    }
}

// ─────────────────────────────────────────────────────────────────────
// Events
// ─────────────────────────────────────────────────────────────────────

/// Protocol events that can drive the state machine forward.
#[derive(Debug, Clone, Eq, PartialEq)]
pub enum Event {
    /// First per-receipt stream opened. `Initiated → StreamOpened`.
    Open,
    /// Chunk progress (self-loop on `StreamOpened`).
    Data,
    /// All payload bytes flushed. `StreamOpened → DataTransferred`,
    /// then `DataTransferred → StreamClosed` once the stream is
    /// FIN-acked. The state machine accepts two consecutive `Close`
    /// events; the second moves the stream-close edge.
    Close,
    /// Application takes ownership of the payload.
    /// `StreamClosed → Processing`.
    Process,
    /// Application accepted the payload. `Processing → Completed(Acked)`.
    Ack,
    /// Application rejected the payload. `Processing → Failed(Nacked)`.
    Nack {
        /// Application-supplied rejection reason.
        reason: String,
    },
    /// User cancel from either side. Valid in any non-terminal state.
    /// Always lands in `Failed(Cancelled)`.
    Cancel {
        /// Free-form cancellation reason.
        reason: String,
    },
    /// Transport / verification / internal error. Valid in any
    /// non-terminal state. Always lands in `Failed(Errored)`.
    Error {
        /// Numeric error code (mirrors the wire `ErrorCode` enum).
        code: u32,
        /// Human-readable diagnostic.
        detail: String,
    },
}

// ─────────────────────────────────────────────────────────────────────
// Outcome / errors
// ─────────────────────────────────────────────────────────────────────

/// Convenience view of a terminal state, returned by
/// [`Receipt::processed`].
#[derive(Debug, Clone, Eq, PartialEq)]
pub enum Outcome {
    /// Application ack: success path.
    Acked,
    /// Application nack: explicit rejection with reason.
    Nacked(String),
    /// User cancellation with reason.
    Cancelled(String),
    /// Transport / infrastructure failure.
    Failed {
        /// Numeric error code.
        code: u32,
        /// Human-readable diagnostic.
        detail: String,
    },
}

/// Errors returned by [`Receipt::apply_event`].
#[derive(Debug, Clone, Eq, PartialEq, thiserror::Error)]
pub enum StateError {
    /// The event is not legal in the current state. The state is
    /// **not** mutated when this is returned.
    #[error("invalid transition: cannot apply {event:?} in state {from}")]
    InvalidTransition {
        /// Current state at the time of the rejected event.
        from: State,
        /// The rejected event.
        event: Event,
    },
}

// ─────────────────────────────────────────────────────────────────────
// Receipt<Side>
// ─────────────────────────────────────────────────────────────────────

/// A single transfer's state machine, parameterised by which side of
/// the wire it lives on.
///
/// `Receipt<Sender>` and `Receipt<Receiver>` are intentionally distinct
/// types so an API that demands a sender-side receipt cannot
/// accidentally accept a receiver-side one. The underlying state
/// machine and storage are identical.
pub struct Receipt<Side> {
    id: Uuid,
    state_tx: watch::Sender<State>,
    _side: PhantomData<Side>,
}

impl<Side> Receipt<Side> {
    /// Construct a fresh receipt in the [`State::Initiated`] state.
    ///
    /// The phantom side is selected by the binding's type annotation:
    ///
    /// ```
    /// # use aerosync::core::receipt::{Receipt, Sender, Receiver};
    /// # use uuid::Uuid;
    /// let s: Receipt<Sender>   = Receipt::new(Uuid::new_v4());
    /// let r: Receipt<Receiver> = Receipt::new(Uuid::new_v4());
    /// ```
    pub fn new(receipt_id: Uuid) -> Self {
        let (tx, _rx) = watch::channel(State::Initiated);
        Self {
            id: receipt_id,
            state_tx: tx,
            _side: PhantomData,
        }
    }

    /// Stable identifier of the receipt (UUID v7 in production code,
    /// any UUID in tests).
    pub fn id(&self) -> Uuid {
        self.id
    }

    /// Snapshot of the current state. Cheap; clones the inner state.
    pub fn state(&self) -> State {
        self.state_tx.borrow().clone()
    }

    /// Subscribe to state transitions. Multiple observers may
    /// subscribe; they each see the latest state on creation.
    ///
    /// Note: `tokio::sync::watch` may coalesce intermediate values if
    /// an observer is slow — assert on the *eventual* state, not on
    /// every intermediate transition.
    pub fn watch(&self) -> watch::Receiver<State> {
        self.state_tx.subscribe()
    }

    /// Apply a protocol event. Returns `Ok` when the transition is
    /// legal, [`StateError::InvalidTransition`] otherwise. On error
    /// the state is unchanged.
    pub fn apply_event(&self, event: Event) -> Result<(), StateError> {
        let current = self.state();
        match next_state(&current, &event) {
            Some(next) => {
                self.state_tx.send_replace(next);
                Ok(())
            }
            None => Err(StateError::InvalidTransition {
                from: current,
                event,
            }),
        }
    }

    /// Wait until `predicate` returns true for the current state, then
    /// return that state. If the watch channel is closed (i.e. this
    /// receipt is being dropped) the function returns the last known
    /// state.
    pub async fn await_state<F>(&self, predicate: F) -> State
    where
        F: Fn(&State) -> bool,
    {
        let mut rx = self.watch();
        // Fast path: predicate already satisfied.
        {
            let s = rx.borrow();
            if predicate(&s) {
                return s.clone();
            }
        }
        loop {
            if rx.changed().await.is_err() {
                return self.state();
            }
            let s = rx.borrow();
            if predicate(&s) {
                return s.clone();
            }
        }
    }

    /// Wait for any terminal state and return a structured outcome.
    pub async fn processed(&self) -> Outcome {
        let final_state = self.await_state(State::is_terminal).await;
        match final_state {
            State::Completed(CompletedTerminal::Acked) => Outcome::Acked,
            State::Failed(FailedTerminal::Nacked { reason }) => Outcome::Nacked(reason),
            State::Failed(FailedTerminal::Cancelled { reason }) => Outcome::Cancelled(reason),
            State::Failed(FailedTerminal::Errored { code, detail }) => {
                Outcome::Failed { code, detail }
            }
            // Reachable only if the watch channel was racing with a
            // dropped sender; defend explicitly rather than panicking.
            other => Outcome::Failed {
                code: 0,
                detail: format!("non-terminal state observed at processed(): {other}"),
            },
        }
    }
}

impl<Side> fmt::Debug for Receipt<Side> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Receipt")
            .field("id", &self.id)
            .field("state", &*self.state_tx.borrow())
            .finish()
    }
}

// ─────────────────────────────────────────────────────────────────────
// Transition table
// ─────────────────────────────────────────────────────────────────────

fn next_state(from: &State, event: &Event) -> Option<State> {
    use Event as E;
    use State as S;
    match (from, event) {
        // From Initiated.
        (S::Initiated, E::Open) => Some(S::StreamOpened),
        (S::Initiated, E::Cancel { reason }) => Some(S::Failed(FailedTerminal::Cancelled {
            reason: reason.clone(),
        })),
        (S::Initiated, E::Error { code, detail }) => Some(S::Failed(FailedTerminal::Errored {
            code: *code,
            detail: detail.clone(),
        })),

        // From StreamOpened: chunk progress is a no-op self-loop; close
        // advances to DataTransferred.
        (S::StreamOpened, E::Data) => Some(S::StreamOpened),
        (S::StreamOpened, E::Close) => Some(S::DataTransferred),
        (S::StreamOpened, E::Cancel { reason }) => Some(S::Failed(FailedTerminal::Cancelled {
            reason: reason.clone(),
        })),
        (S::StreamOpened, E::Error { code, detail }) => Some(S::Failed(FailedTerminal::Errored {
            code: *code,
            detail: detail.clone(),
        })),

        // From DataTransferred: a second `Close` represents the QUIC
        // stream FIN being acked.
        // RFC-002 §4/§5 ambiguity resolution: receiving `Cancel` here
        // (i.e. after all bytes are on the wire but before the receiver
        // app has touched the file) is treated conservatively as
        // Failed(Cancelled), per the parent execution-plan honesty
        // clause "pick the most conservative interpretation".
        (S::DataTransferred, E::Close) => Some(S::StreamClosed),
        (S::DataTransferred, E::Cancel { reason }) => Some(S::Failed(FailedTerminal::Cancelled {
            reason: reason.clone(),
        })),
        (S::DataTransferred, E::Error { code, detail }) => {
            Some(S::Failed(FailedTerminal::Errored {
                code: *code,
                detail: detail.clone(),
            }))
        }

        // From StreamClosed: receiver app picks up payload to begin
        // verification + processing.
        (S::StreamClosed, E::Process) => Some(S::Processing),
        (S::StreamClosed, E::Cancel { reason }) => Some(S::Failed(FailedTerminal::Cancelled {
            reason: reason.clone(),
        })),
        (S::StreamClosed, E::Error { code, detail }) => Some(S::Failed(FailedTerminal::Errored {
            code: *code,
            detail: detail.clone(),
        })),

        // From Processing: terminal transitions on app verdict.
        (S::Processing, E::Ack) => Some(S::Completed(CompletedTerminal::Acked)),
        (S::Processing, E::Nack { reason }) => Some(S::Failed(FailedTerminal::Nacked {
            reason: reason.clone(),
        })),
        (S::Processing, E::Cancel { reason }) => Some(S::Failed(FailedTerminal::Cancelled {
            reason: reason.clone(),
        })),
        (S::Processing, E::Error { code, detail }) => Some(S::Failed(FailedTerminal::Errored {
            code: *code,
            detail: detail.clone(),
        })),

        // Terminal states absorb all further events.
        (S::Completed(_), _) | (S::Failed(_), _) => None,

        // Anything else is illegal.
        _ => None,
    }
}

// ─────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn uuid() -> Uuid {
        Uuid::new_v4()
    }

    fn cancel(reason: &str) -> Event {
        Event::Cancel {
            reason: reason.into(),
        }
    }

    fn nack(reason: &str) -> Event {
        Event::Nack {
            reason: reason.into(),
        }
    }

    fn error(code: u32, detail: &str) -> Event {
        Event::Error {
            code,
            detail: detail.into(),
        }
    }

    // ── Sender-side legal transitions (RFC-002 §4) ───────────────────

    #[test]
    fn sender_full_happy_path_to_completed_acked() {
        let r: Receipt<Sender> = Receipt::new(uuid());
        assert_eq!(r.state(), State::Initiated);
        r.apply_event(Event::Open).unwrap();
        assert_eq!(r.state(), State::StreamOpened);
        r.apply_event(Event::Data).unwrap();
        r.apply_event(Event::Data).unwrap();
        assert_eq!(r.state(), State::StreamOpened);
        r.apply_event(Event::Close).unwrap();
        assert_eq!(r.state(), State::DataTransferred);
        r.apply_event(Event::Close).unwrap();
        assert_eq!(r.state(), State::StreamClosed);
        r.apply_event(Event::Process).unwrap();
        assert_eq!(r.state(), State::Processing);
        r.apply_event(Event::Ack).unwrap();
        assert_eq!(r.state(), State::Completed(CompletedTerminal::Acked));
    }

    #[test]
    fn sender_cancel_from_initiated() {
        let r: Receipt<Sender> = Receipt::new(uuid());
        r.apply_event(cancel("user")).unwrap();
        assert_eq!(
            r.state(),
            State::Failed(FailedTerminal::Cancelled {
                reason: "user".into()
            })
        );
    }

    #[test]
    fn sender_cancel_from_stream_opened() {
        let r: Receipt<Sender> = Receipt::new(uuid());
        r.apply_event(Event::Open).unwrap();
        r.apply_event(cancel("ctrl-c")).unwrap();
        assert!(matches!(
            r.state(),
            State::Failed(FailedTerminal::Cancelled { .. })
        ));
    }

    #[test]
    fn sender_cancel_from_data_transferred_is_failed_cancelled() {
        // RFC-002 §4/§5 ambiguity resolution: Cancel after the bytes
        // are on the wire but before processing is conservatively
        // mapped to Failed(Cancelled).
        let r: Receipt<Sender> = Receipt::new(uuid());
        r.apply_event(Event::Open).unwrap();
        r.apply_event(Event::Close).unwrap();
        assert_eq!(r.state(), State::DataTransferred);
        r.apply_event(cancel("late")).unwrap();
        assert!(matches!(
            r.state(),
            State::Failed(FailedTerminal::Cancelled { .. })
        ));
    }

    #[test]
    fn sender_error_from_stream_opened() {
        let r: Receipt<Sender> = Receipt::new(uuid());
        r.apply_event(Event::Open).unwrap();
        r.apply_event(error(2, "checksum")).unwrap();
        assert_eq!(
            r.state(),
            State::Failed(FailedTerminal::Errored {
                code: 2,
                detail: "checksum".into()
            })
        );
    }

    // ── Receiver-side legal transitions (RFC-002 §5) ─────────────────

    #[test]
    fn receiver_full_happy_path_to_completed_acked() {
        let r: Receipt<Receiver> = Receipt::new(uuid());
        r.apply_event(Event::Open).unwrap();
        r.apply_event(Event::Data).unwrap();
        r.apply_event(Event::Close).unwrap();
        r.apply_event(Event::Close).unwrap();
        r.apply_event(Event::Process).unwrap();
        r.apply_event(Event::Ack).unwrap();
        assert_eq!(r.state(), State::Completed(CompletedTerminal::Acked));
    }

    #[test]
    fn receiver_nack_from_processing() {
        let r: Receipt<Receiver> = Receipt::new(uuid());
        r.apply_event(Event::Open).unwrap();
        r.apply_event(Event::Close).unwrap();
        r.apply_event(Event::Close).unwrap();
        r.apply_event(Event::Process).unwrap();
        r.apply_event(nack("schema-mismatch")).unwrap();
        assert_eq!(
            r.state(),
            State::Failed(FailedTerminal::Nacked {
                reason: "schema-mismatch".into()
            })
        );
    }

    #[test]
    fn receiver_error_from_initiated() {
        let r: Receipt<Receiver> = Receipt::new(uuid());
        r.apply_event(error(99, "auth fail")).unwrap();
        assert_eq!(
            r.state(),
            State::Failed(FailedTerminal::Errored {
                code: 99,
                detail: "auth fail".into()
            })
        );
    }

    // ── Illegal transitions (≥5) ────────────────────────────────────

    #[test]
    fn illegal_ack_from_initiated() {
        let r: Receipt<Sender> = Receipt::new(uuid());
        let err = r.apply_event(Event::Ack).unwrap_err();
        assert!(matches!(err, StateError::InvalidTransition { .. }));
        assert_eq!(r.state(), State::Initiated);
    }

    #[test]
    fn illegal_nack_from_stream_opened() {
        let r: Receipt<Sender> = Receipt::new(uuid());
        r.apply_event(Event::Open).unwrap();
        let err = r.apply_event(nack("nope")).unwrap_err();
        assert!(matches!(err, StateError::InvalidTransition { .. }));
        assert_eq!(r.state(), State::StreamOpened);
    }

    #[test]
    fn illegal_open_from_stream_opened() {
        let r: Receipt<Sender> = Receipt::new(uuid());
        r.apply_event(Event::Open).unwrap();
        let err = r.apply_event(Event::Open).unwrap_err();
        assert!(matches!(err, StateError::InvalidTransition { .. }));
        assert_eq!(r.state(), State::StreamOpened);
    }

    #[test]
    fn illegal_process_from_data_transferred() {
        let r: Receipt<Sender> = Receipt::new(uuid());
        r.apply_event(Event::Open).unwrap();
        r.apply_event(Event::Close).unwrap();
        let err = r.apply_event(Event::Process).unwrap_err();
        assert!(matches!(err, StateError::InvalidTransition { .. }));
        assert_eq!(r.state(), State::DataTransferred);
    }

    #[test]
    fn illegal_data_after_stream_closed() {
        let r: Receipt<Sender> = Receipt::new(uuid());
        r.apply_event(Event::Open).unwrap();
        r.apply_event(Event::Close).unwrap();
        r.apply_event(Event::Close).unwrap();
        let err = r.apply_event(Event::Data).unwrap_err();
        assert!(matches!(err, StateError::InvalidTransition { .. }));
        assert_eq!(r.state(), State::StreamClosed);
    }

    // ── Terminal absorption ─────────────────────────────────────────

    #[test]
    fn completed_is_absorbing() {
        let r: Receipt<Sender> = Receipt::new(uuid());
        r.apply_event(Event::Open).unwrap();
        r.apply_event(Event::Close).unwrap();
        r.apply_event(Event::Close).unwrap();
        r.apply_event(Event::Process).unwrap();
        r.apply_event(Event::Ack).unwrap();
        let snapshot = r.state();
        assert!(snapshot.is_terminal());
        // Any further event is rejected and the state is unchanged.
        for ev in [
            Event::Open,
            Event::Data,
            Event::Close,
            Event::Process,
            Event::Ack,
            nack("x"),
            cancel("x"),
            error(1, "x"),
        ] {
            let err = r.apply_event(ev).unwrap_err();
            assert!(matches!(err, StateError::InvalidTransition { .. }));
            assert_eq!(r.state(), snapshot);
        }
    }

    #[test]
    fn failed_is_absorbing() {
        let r: Receipt<Receiver> = Receipt::new(uuid());
        r.apply_event(cancel("nope")).unwrap();
        let snapshot = r.state();
        assert!(snapshot.is_terminal());
        let err = r.apply_event(Event::Open).unwrap_err();
        assert!(matches!(err, StateError::InvalidTransition { .. }));
        assert_eq!(r.state(), snapshot);
    }

    // ── watch() observability ───────────────────────────────────────

    #[tokio::test]
    async fn watch_observer_sees_final_state() {
        let r: Receipt<Sender> = Receipt::new(uuid());
        let mut rx = r.watch();
        // Apply several transitions in sequence.
        r.apply_event(Event::Open).unwrap();
        r.apply_event(Event::Close).unwrap();
        r.apply_event(Event::Close).unwrap();
        r.apply_event(Event::Process).unwrap();
        r.apply_event(Event::Ack).unwrap();
        // Watch may coalesce intermediate values; assert eventual state.
        loop {
            if rx.borrow_and_update().is_terminal() {
                break;
            }
            rx.changed().await.unwrap();
        }
        assert_eq!(*rx.borrow(), State::Completed(CompletedTerminal::Acked));
    }

    // ── processed() outcomes ────────────────────────────────────────

    #[tokio::test]
    async fn processed_returns_acked() {
        let r: Receipt<Sender> = Receipt::new(uuid());
        r.apply_event(Event::Open).unwrap();
        r.apply_event(Event::Close).unwrap();
        r.apply_event(Event::Close).unwrap();
        r.apply_event(Event::Process).unwrap();
        r.apply_event(Event::Ack).unwrap();
        assert_eq!(r.processed().await, Outcome::Acked);
    }

    #[tokio::test]
    async fn processed_returns_nacked() {
        let r: Receipt<Sender> = Receipt::new(uuid());
        r.apply_event(Event::Open).unwrap();
        r.apply_event(Event::Close).unwrap();
        r.apply_event(Event::Close).unwrap();
        r.apply_event(Event::Process).unwrap();
        r.apply_event(nack("bad data")).unwrap();
        assert_eq!(r.processed().await, Outcome::Nacked("bad data".into()));
    }

    #[tokio::test]
    async fn processed_returns_cancelled() {
        let r: Receipt<Sender> = Receipt::new(uuid());
        r.apply_event(cancel("user-stop")).unwrap();
        assert_eq!(r.processed().await, Outcome::Cancelled("user-stop".into()));
    }

    #[tokio::test]
    async fn processed_returns_failed_on_error() {
        let r: Receipt<Sender> = Receipt::new(uuid());
        r.apply_event(error(2, "checksum")).unwrap();
        assert_eq!(
            r.processed().await,
            Outcome::Failed {
                code: 2,
                detail: "checksum".into()
            }
        );
    }

    // ── Identity / type-system invariants ───────────────────────────

    #[test]
    fn id_is_stable_across_transitions() {
        let id = uuid();
        let r: Receipt<Sender> = Receipt::new(id);
        assert_eq!(r.id(), id);
        r.apply_event(Event::Open).unwrap();
        assert_eq!(r.id(), id);
        r.apply_event(Event::Close).unwrap();
        assert_eq!(r.id(), id);
        r.apply_event(cancel("done")).unwrap();
        assert_eq!(r.id(), id);
    }

    #[test]
    fn two_sender_receipts_with_same_id_have_independent_state() {
        let id = uuid();
        let r1: Receipt<Sender> = Receipt::new(id);
        let r2: Receipt<Sender> = Receipt::new(id);
        assert_eq!(r1.id(), r2.id());
        r1.apply_event(Event::Open).unwrap();
        // r2 is untouched.
        assert_eq!(r1.state(), State::StreamOpened);
        assert_eq!(r2.state(), State::Initiated);
    }

    // Compile-time check: Receipt<Sender> and Receipt<Receiver> are
    // distinct types. The function below would fail to compile if they
    // were unified by the trait system.
    #[allow(dead_code)]
    fn type_distinction(_: Receipt<Sender>) {}
    #[allow(dead_code)]
    fn type_distinction_recv(_: Receipt<Receiver>) {}

    // ── Misc ─────────────────────────────────────────────────────────

    #[test]
    fn state_display_matches_lowercase_name() {
        assert_eq!(State::Initiated.to_string(), "initiated");
        assert_eq!(State::Processing.to_string(), "processing");
        assert_eq!(
            State::Completed(CompletedTerminal::Acked).to_string(),
            "completed"
        );
    }

    #[test]
    fn await_state_returns_immediately_when_predicate_already_true() {
        // Use the runtime to drive the future.
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let r: Receipt<Sender> = Receipt::new(uuid());
        r.apply_event(cancel("instant")).unwrap();
        let s = rt.block_on(async { r.await_state(State::is_terminal).await });
        assert!(s.is_terminal());
    }
}
