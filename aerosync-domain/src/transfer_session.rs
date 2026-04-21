//! [`TransferSession`] aggregate root + supporting value objects:
//! [`ProtocolKind`], [`SessionEvent`], [`EventLog`], [`ReceiptLedger`],
//! [`ReceiptEntry`], [`SessionStateError`].
//!
//! Per `docs/v0.3.0-refactor-plan.md` §3 (line 124, micro-PR 3.3),
//! this module composes the Phase 3.1 / 3.2 companions into the
//! actual `TransferSession` struct that the sender path (Phase 3.4)
//! and the receiver path (Phase 3.5) will adopt.
//!
//! ## v0.3.0 Phase 3.3 + 3.4c status (skeleton, no consumer wiring)
//!
//! Like the rest of Phase 3 to date, this module is **purely
//! additive** — no existing AeroSync code constructs a
//! [`TransferSession`] yet. The state machine is fully validated
//! (illegal transitions return [`SessionStateError`]) and the event
//! log is fully usable, so Phase 3.4 / 3.5 can drive it from the
//! engine and receiver respectively without further changes here.
//!
//! Phase 3.4c (this commit) adds [`ReceiptLedger`] — a per-session
//! map from `receipt_id` → ([`crate::receipt::State`] snapshot +
//! [`crate::receipt::Outcome`] when terminal) — alongside
//! [`EventLog`]. The ledger holds plain snapshots rather than
//! phantom-typed `Arc<Receipt<Side>>`s so a single session can
//! mix sender-side and receiver-side receipts in one collection
//! without an existential type. Phase 3.4e/f wiring will feed
//! it from `tokio::sync::watch::Receiver` observers attached to
//! the per-task `Receipt`s.
//!
//! ## What's deferred to Phase 3.4d/e/f/g
//!
//! - **`receipts: ReceiptLedger` field on `TransferSession`.** The
//!   refactor sketch shows the ledger embedded in the aggregate
//!   root, but adding a non-default-constructible field today
//!   would change the serde-format of `TransferSession` and force
//!   every test/mock that already constructs one to update.
//!   Phase 3.4e (engine integration) lands the field together
//!   with the `task_ids` field below — both as a single
//!   serde-format change.
//!
//! - **`task_ids: Vec<TaskId>` field.** The refactor sketch shows
//!   this on `TransferSession`, but `TaskId` lives in the root
//!   crate. Phase 3.4 will either (a) add a domain-level `TaskId`
//!   newtype and convert at the boundary, or (b) hold raw `Uuid`s
//!   in a `Vec<Uuid>` here and let the engine maintain a side-table.
//!   The choice is part of the larger Phase 3.4 sender-path design
//!   conversation; deferring keeps this skeleton uncoupled.
//!
//! ## State machine
//!
//! Legal transitions enforced by [`TransferSession::transition_to`]:
//!
//! ```text
//!   Pending ─► Active ─┬─► Completed
//!         │            ├─► Failed { reason }
//!         │            └─► Cancelled
//!         └────────────► Cancelled        (cancel before start)
//! ```
//!
//! Any other move returns [`SessionStateError`] without touching
//! `self.status`. Entering a terminal state (Completed / Failed /
//! Cancelled) also stamps [`TransferSession::completed_at`].
//!
//! ## Stability
//!
//! Types in this module are reachable only through the module path
//! (`aerosync_domain::transfer_session::*`) and are considered
//! unstable until Phase 3.3-end elevates a curated subset to the
//! crate root. They are **not** listed in
//! `docs/v0.3.0-frozen-api.md` yet.

use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::time::SystemTime;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::manifest::FileManifest;
use crate::receipt::{Outcome, State};
use crate::session::{SessionId, SessionKind, SessionStatus};

// ──────────────────────────── ProtocolKind ─────────────────────────────────────

/// Wire protocol carrying a [`TransferSession`].
///
/// `#[non_exhaustive]` so v0.4 can add `WanRendezvous` (RFC-004) and
/// future transports without a major bump on `aerosync-domain`.
/// Wire format is `#[serde(rename_all = "kebab-case")]` matching
/// the values the CLI accepts via `--protocol` (per
/// `docs/v0.3.0-frozen-api.md` §5.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
#[non_exhaustive]
pub enum ProtocolKind {
    /// HTTP/1.1 chunked upload + JSON receipt fallback. Default for
    /// LAN transfers when QUIC is unavailable.
    Http,
    /// QUIC + custom binary framing. Primary fast-path for LAN.
    Quic,
    /// AWS S3 multipart upload (or any S3-compatible endpoint).
    /// Behind the `s3` cargo feature on the root crate.
    S3,
    /// Plain FTP. Behind the `ftp` cargo feature on the root crate.
    Ftp,
}

impl ProtocolKind {
    /// Lower-case `kebab-case` tag string used in serialized form,
    /// CLI flags, and config files. Matches the serde rename rule.
    pub const fn as_str(self) -> &'static str {
        match self {
            ProtocolKind::Http => "http",
            ProtocolKind::Quic => "quic",
            ProtocolKind::S3 => "s3",
            ProtocolKind::Ftp => "ftp",
        }
    }
}

impl fmt::Display for ProtocolKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

// ──────────────────────────── SessionEvent / EventLog ──────────────────────────

/// One entry in a [`TransferSession`]'s [`EventLog`].
///
/// The event log is a **debug aid**, not a full audit trail —
/// `aerosync-infra::audit::AuditLogger` owns the durable side. The
/// log holds the most recent N events (default 256, see
/// [`EventLog::DEFAULT_CAPACITY`]) so a debugger inspecting a stuck
/// session can see what happened locally without paging in the audit
/// store.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum SessionEvent {
    /// The session was constructed. Recorded by
    /// [`TransferSession::new`] with the initial `Pending` status.
    Created {
        /// Wall-clock time at construction.
        at: SystemTime,
        /// Status the session was constructed in (always `Pending`
        /// today, kept as a field so future builders that start in
        /// `Active` — e.g. session rehydration on receiver restart —
        /// can record their actual seed state).
        status: SessionStatus,
    },
    /// A successful [`TransferSession::transition_to`] call.
    StatusChanged {
        /// Wall-clock time the transition was applied.
        at: SystemTime,
        /// Previous status.
        from: SessionStatus,
        /// New status.
        to: SessionStatus,
    },
    /// Caller-supplied diagnostic. Phases 3.4 / 3.5 will record
    /// per-task events here (`"task chunk-12 acked"`, etc.) — kept
    /// free-form to avoid coupling this enum to concrete task
    /// shapes ahead of time.
    Custom {
        /// Wall-clock time the event was recorded.
        at: SystemTime,
        /// Free-form message. Convention: short imperative phrase
        /// such as `"task X started"` so log readers can scan.
        msg: String,
    },
}

impl SessionEvent {
    /// Wall-clock timestamp the event was recorded.
    pub fn at(&self) -> SystemTime {
        match self {
            SessionEvent::Created { at, .. }
            | SessionEvent::StatusChanged { at, .. }
            | SessionEvent::Custom { at, .. } => *at,
        }
    }
}

/// Bounded ring buffer of [`SessionEvent`]s.
///
/// Backed by a [`VecDeque`] for `O(1)` push-and-evict. When the
/// buffer reaches capacity, the **oldest** event is dropped to make
/// room for the newest. Capacity is set at construction time and
/// fixed for the lifetime of the log.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EventLog {
    capacity: usize,
    events: VecDeque<SessionEvent>,
}

impl EventLog {
    /// Default capacity used by [`EventLog::new`] and
    /// [`Default::default`]. Sized so a verbose session
    /// (`StatusChanged` per chunk × ~hundreds of chunks) still fits
    /// without dropping foundational events like `Created`.
    pub const DEFAULT_CAPACITY: usize = 256;

    /// Construct an empty log with [`Self::DEFAULT_CAPACITY`].
    pub fn new() -> Self {
        Self::with_capacity(Self::DEFAULT_CAPACITY)
    }

    /// Construct an empty log with the given `capacity`. A capacity
    /// of 0 is degenerate but legal (every push is dropped); the
    /// debug-aid use-case warrants tolerating it without an error.
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            capacity,
            events: VecDeque::with_capacity(capacity),
        }
    }

    /// Append `event` to the log, evicting the oldest entry if the
    /// log is at capacity. No-op when [`Self::capacity`] is 0.
    pub fn push(&mut self, event: SessionEvent) {
        if self.capacity == 0 {
            return;
        }
        if self.events.len() == self.capacity {
            self.events.pop_front();
        }
        self.events.push_back(event);
    }

    /// Borrow the events oldest-first. The returned slice is
    /// guaranteed contiguous because `VecDeque::make_contiguous`
    /// would change the buffer; instead, [`Self::iter`] is the
    /// preferred read API.
    pub fn iter(&self) -> impl Iterator<Item = &SessionEvent> {
        self.events.iter()
    }

    /// Number of events currently stored.
    pub fn len(&self) -> usize {
        self.events.len()
    }

    /// `true` iff no events have been recorded.
    pub fn is_empty(&self) -> bool {
        self.events.is_empty()
    }

    /// Configured maximum number of events.
    pub fn capacity(&self) -> usize {
        self.capacity
    }
}

impl Default for EventLog {
    fn default() -> Self {
        Self::new()
    }
}

// ──────────────────────────── ReceiptLedger / ReceiptEntry ────────────────────

/// Snapshot of one receipt's lifecycle as recorded in a
/// [`ReceiptLedger`].
///
/// Holds the most-recent [`crate::receipt::State`] for the receipt
/// plus the wall-clock time at which it was recorded. When the
/// receipt has reached a terminal state, [`ReceiptEntry::outcome`]
/// is also populated for fast `terminal/pending` queries; for
/// non-terminal entries it is `None`.
///
/// `ReceiptEntry` is intentionally a plain snapshot rather than an
/// `Arc<Receipt<Side>>` — receipts come in two phantom-typed
/// flavours (`Receipt<Sender>` / `Receipt<Receiver>`) that cannot
/// share a single homogeneous container without an existential type.
/// The session-level ledger only needs the observable lifecycle,
/// which is identical between the two sides, so the snapshot
/// representation is both simpler and lossless.
///
/// **Not yet serializable.** [`crate::receipt::State`] and
/// [`crate::receipt::Outcome`] do not derive `Serialize` /
/// `Deserialize` today (the receipt types are tokio-watch-driven,
/// not persisted). If Phase 3.4e decides to embed
/// [`ReceiptLedger`] in [`TransferSession`]'s on-disk
/// representation, serde derives will be added here and on the
/// receipt enums together. Today the ledger is a process-lifetime
/// in-memory aggregator only.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReceiptEntry {
    /// Stable receipt identifier (matches `Receipt::id()`).
    pub receipt_id: Uuid,
    /// Most-recently observed lifecycle state for this receipt.
    pub state: State,
    /// Cached terminal outcome — `Some(_)` iff
    /// [`Self::state`] is one of [`State::Completed`] /
    /// [`State::Failed`]. Avoids re-pattern-matching the state
    /// every time a caller asks "did this terminal ack or fail?".
    pub outcome: Option<Outcome>,
    /// Wall-clock time the snapshot was last written by
    /// [`ReceiptLedger::record`].
    pub recorded_at: SystemTime,
}

/// Per-session map from `receipt_id` to the latest
/// [`ReceiptEntry`].
///
/// One [`TransferSession`] hosts N parallel per-file transfers,
/// each owning a `Receipt`. The ledger aggregates their snapshots
/// so the session aggregate root can answer "are we done yet?"
/// (`pending_count() == 0`), "any failures?"
/// (`iter().any(|e| matches!(e.outcome, Some(Outcome::Failed(_))))`),
/// or "where did receipt X end up?" (`get(id)`) without
/// touching every per-task `Receipt` directly.
///
/// The ledger does NOT subscribe to receipt change notifications
/// itself — Phase 3.4e wiring will spawn one observer task per
/// receipt that calls [`ReceiptLedger::record`] on every
/// `tokio::sync::watch::Receiver::changed()`. Keeping the ledger
/// passive keeps it usable in test mocks and in the Phase 3.5
/// receiver path where the watch loop sits one layer up.
/// **Not yet serializable** — see [`ReceiptEntry`] for the
/// rationale and the Phase 3.4e migration plan.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReceiptLedger {
    /// Snapshot map keyed by `receipt_id`. Stored as a plain
    /// `HashMap` because lookups dominate over iteration on this
    /// type's call-sites.
    entries: HashMap<Uuid, ReceiptEntry>,
}

impl ReceiptLedger {
    /// Construct an empty ledger.
    pub fn new() -> Self {
        Self::default()
    }

    /// Record (or overwrite) the snapshot for `receipt_id`. The
    /// `recorded_at` field is stamped to `SystemTime::now()`; tests
    /// that need determinism can use [`Self::record_at`].
    pub fn record(&mut self, receipt_id: Uuid, state: State) {
        self.record_at(receipt_id, state, SystemTime::now());
    }

    /// Variant of [`Self::record`] with an explicit timestamp.
    /// Auto-populates [`ReceiptEntry::outcome`] when the state is
    /// terminal, mirroring [`State::outcome`].
    pub fn record_at(&mut self, receipt_id: Uuid, state: State, at: SystemTime) {
        let outcome = state.outcome();
        self.entries.insert(
            receipt_id,
            ReceiptEntry {
                receipt_id,
                state,
                outcome,
                recorded_at: at,
            },
        );
    }

    /// Borrow the snapshot for `receipt_id`, or `None` if the
    /// ledger has never seen it.
    pub fn get(&self, receipt_id: Uuid) -> Option<&ReceiptEntry> {
        self.entries.get(&receipt_id)
    }

    /// Iterate every recorded entry. Iteration order is
    /// unspecified (backed by `HashMap`); callers that need a
    /// stable order should `collect` then `sort_by_key(|e|
    /// e.recorded_at)`.
    pub fn iter(&self) -> impl Iterator<Item = &ReceiptEntry> {
        self.entries.values()
    }

    /// Number of recorded receipts (terminal + pending).
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// `true` iff no receipts have been recorded.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Number of recorded receipts that have reached a terminal
    /// state ([`State::Completed`] / [`State::Failed`]).
    pub fn terminal_count(&self) -> usize {
        self.entries
            .values()
            .filter(|e| e.state.is_terminal())
            .count()
    }

    /// Number of recorded receipts still in-flight (not terminal).
    /// Equal to `len() - terminal_count()`.
    pub fn pending_count(&self) -> usize {
        self.entries
            .values()
            .filter(|e| !e.state.is_terminal())
            .count()
    }
}

// ──────────────────────────── SessionStateError ────────────────────────────────

/// An illegal state-machine transition was attempted on a
/// [`TransferSession`].
///
/// Returned by [`TransferSession::transition_to`] when the requested
/// move is not permitted by the table in this module's docs.
/// Distinct from [`crate::error::AeroSyncError`] because state-machine
/// violations are programmer bugs (the caller should have known the
/// current status), not propagated I/O failures.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionStateError {
    /// Status the session was in when the transition was attempted.
    pub from: SessionStatus,
    /// Status the caller tried to move to.
    pub to: SessionStatus,
}

impl fmt::Display for SessionStateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "illegal session transition {:?} → {:?}",
            self.from, self.to
        )
    }
}

impl std::error::Error for SessionStateError {}

// ──────────────────────────── TransferSession ──────────────────────────────────

/// The aggregate root of a single AeroSync transfer session.
///
/// Owns its identity ([`SessionId`]), direction
/// ([`SessionKind`]), payload ([`FileManifest`]), wire choice
/// ([`ProtocolKind`]), lifecycle ([`SessionStatus`]), and a bounded
/// debug log ([`EventLog`]). Constructing a session does NOT start
/// any I/O — the session begins in [`SessionStatus::Pending`] and
/// the engine (Phase 3.4) calls
/// [`TransferSession::transition_to`] with `Active` to mark
/// dispatch.
///
/// **Not yet wired** into the engine / receiver — Phases 3.4 / 3.5
/// own that. Today the type exists so test code, mocks, and design
/// docs can reference a stable signature.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TransferSession {
    /// Stable identity. Generated by the constructor; survives
    /// persistence + rehydration.
    pub id: SessionId,
    /// Send vs. Receive plus the corresponding root path /
    /// destination handle.
    pub kind: SessionKind,
    /// Files this session covers.
    pub manifest: FileManifest,
    /// Which transport carries the session's bytes.
    pub protocol: ProtocolKind,
    /// Lifecycle state. Mutate only via
    /// [`TransferSession::transition_to`].
    pub status: SessionStatus,
    /// Bounded log of state-changes and caller-supplied custom
    /// events. See [`EventLog`].
    pub events: EventLog,
    /// Wall-clock time the session was constructed.
    pub started_at: SystemTime,
    /// Wall-clock time the session reached one of the three
    /// terminal states. `None` while non-terminal.
    pub completed_at: Option<SystemTime>,
}

impl TransferSession {
    /// Build a new session in [`SessionStatus::Pending`].
    ///
    /// Auto-generates a fresh [`SessionId`] and stamps `started_at`
    /// to `SystemTime::now()`. Records a [`SessionEvent::Created`]
    /// in the event log.
    pub fn new(kind: SessionKind, manifest: FileManifest, protocol: ProtocolKind) -> Self {
        Self::new_at(kind, manifest, protocol, SystemTime::now())
    }

    /// Like [`Self::new`] but with the construction timestamp
    /// supplied explicitly. Used in tests so `started_at` and the
    /// `Created` event are deterministic.
    pub fn new_at(
        kind: SessionKind,
        manifest: FileManifest,
        protocol: ProtocolKind,
        at: SystemTime,
    ) -> Self {
        let id = SessionId::new();
        let status = SessionStatus::Pending;
        let mut events = EventLog::new();
        events.push(SessionEvent::Created {
            at,
            status: status.clone(),
        });
        Self {
            id,
            kind,
            manifest,
            protocol,
            status,
            events,
            started_at: at,
            completed_at: None,
        }
    }

    /// Attempt the transition `self.status → new`.
    ///
    /// Records a [`SessionEvent::StatusChanged`] on success and
    /// stamps [`Self::completed_at`] when entering a terminal
    /// state. Returns [`SessionStateError`] without touching state
    /// when the move is illegal.
    pub fn transition_to(&mut self, new: SessionStatus) -> Result<(), SessionStateError> {
        self.transition_to_at(new, SystemTime::now())
    }

    /// Variant of [`Self::transition_to`] that accepts an explicit
    /// timestamp. Used in tests to keep the event log deterministic.
    pub fn transition_to_at(
        &mut self,
        new: SessionStatus,
        at: SystemTime,
    ) -> Result<(), SessionStateError> {
        if !is_legal_transition(&self.status, &new) {
            return Err(SessionStateError {
                from: self.status.clone(),
                to: new,
            });
        }
        let from = std::mem::replace(&mut self.status, new.clone());
        self.events.push(SessionEvent::StatusChanged {
            at,
            from,
            to: new.clone(),
        });
        if new.is_terminal() {
            self.completed_at = Some(at);
        }
        Ok(())
    }

    /// Append a free-form caller-supplied event to the log. Does
    /// NOT change `self.status`; pure logging. Phases 3.4 / 3.5
    /// will use this to record per-task progress.
    pub fn record_custom(&mut self, msg: impl Into<String>) {
        self.events.push(SessionEvent::Custom {
            at: SystemTime::now(),
            msg: msg.into(),
        });
    }
}

/// State-machine table. Pulled out as a free fn so unit tests can
/// hammer it without constructing a whole [`TransferSession`].
///
/// See module docs for the legal-move diagram.
fn is_legal_transition(from: &SessionStatus, to: &SessionStatus) -> bool {
    use SessionStatus::*;
    match (from, to) {
        // No-op transitions are forbidden — they would write
        // misleading `StatusChanged { from: X, to: X }` events.
        (a, b) if std::mem::discriminant(a) == std::mem::discriminant(b) => false,
        // Forward edges from Pending.
        (Pending, Active) => true,
        (Pending, Cancelled) => true,
        // Forward edges from Active to terminal states.
        (Active, Completed) => true,
        (Active, Failed { .. }) => true,
        (Active, Cancelled) => true,
        // Everything else (incl. Pending → Completed / Failed and
        // any move out of a terminal state) is illegal.
        _ => false,
    }
}

// ──────────────────────────── Tests ────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::{FileEntry, FileManifest};
    use crate::session::SessionKind;
    use std::path::PathBuf;
    use std::time::Duration;

    fn sample_kind() -> SessionKind {
        SessionKind::Send {
            destination: "https://example.invalid/upload".to_string(),
            source_root: PathBuf::from("/tmp/src"),
        }
    }

    fn sample_manifest() -> FileManifest {
        FileManifest::new([FileEntry::new(PathBuf::from("a.txt"), 10)]).unwrap()
    }

    // ---- ProtocolKind -------------------------------------------------------

    #[test]
    fn protocol_kind_serde_uses_kebab_case() {
        assert_eq!(
            serde_json::to_string(&ProtocolKind::Http).unwrap(),
            "\"http\""
        );
        assert_eq!(
            serde_json::to_string(&ProtocolKind::S3).unwrap(),
            "\"s3\""
        );
        let back: ProtocolKind = serde_json::from_str("\"quic\"").unwrap();
        assert_eq!(back, ProtocolKind::Quic);
    }

    #[test]
    fn protocol_kind_display_matches_as_str() {
        assert_eq!(ProtocolKind::Ftp.to_string(), "ftp");
        assert_eq!(ProtocolKind::Quic.as_str(), "quic");
    }

    // ---- EventLog -----------------------------------------------------------

    #[test]
    fn event_log_evicts_oldest_when_full() {
        let mut log = EventLog::with_capacity(2);
        let now = SystemTime::now();
        log.push(SessionEvent::Custom {
            at: now,
            msg: "a".into(),
        });
        log.push(SessionEvent::Custom {
            at: now,
            msg: "b".into(),
        });
        log.push(SessionEvent::Custom {
            at: now,
            msg: "c".into(),
        });
        assert_eq!(log.len(), 2);
        let msgs: Vec<&str> = log
            .iter()
            .map(|e| match e {
                SessionEvent::Custom { msg, .. } => msg.as_str(),
                _ => panic!("expected Custom"),
            })
            .collect();
        assert_eq!(msgs, vec!["b", "c"]);
    }

    #[test]
    fn event_log_zero_capacity_drops_pushes() {
        let mut log = EventLog::with_capacity(0);
        log.push(SessionEvent::Custom {
            at: SystemTime::now(),
            msg: "x".into(),
        });
        assert!(log.is_empty());
    }

    #[test]
    fn event_log_default_capacity() {
        assert_eq!(EventLog::default().capacity(), EventLog::DEFAULT_CAPACITY);
    }

    #[test]
    fn session_event_at_is_uniform_across_variants() {
        let t = SystemTime::now();
        let created = SessionEvent::Created {
            at: t,
            status: SessionStatus::Pending,
        };
        let changed = SessionEvent::StatusChanged {
            at: t,
            from: SessionStatus::Pending,
            to: SessionStatus::Active,
        };
        let custom = SessionEvent::Custom {
            at: t,
            msg: "x".into(),
        };
        assert_eq!(created.at(), t);
        assert_eq!(changed.at(), t);
        assert_eq!(custom.at(), t);
    }

    // ---- ReceiptLedger ------------------------------------------------------

    use crate::receipt::{CompletedTerminal, FailedTerminal, Outcome as ReceiptOutcome};

    #[test]
    fn receipt_ledger_default_is_empty() {
        let l = ReceiptLedger::new();
        assert_eq!(l.len(), 0);
        assert!(l.is_empty());
        assert_eq!(l.terminal_count(), 0);
        assert_eq!(l.pending_count(), 0);
    }

    #[test]
    fn receipt_ledger_record_inserts_then_overwrites_same_id() {
        let mut l = ReceiptLedger::new();
        let id = Uuid::new_v4();
        l.record(id, State::Initiated);
        assert_eq!(l.len(), 1);
        // Re-record same id with a later state — overwrite, not append.
        l.record(id, State::StreamOpened);
        assert_eq!(l.len(), 1);
        let entry = l.get(id).unwrap();
        assert_eq!(entry.state, State::StreamOpened);
    }

    #[test]
    fn receipt_ledger_outcome_is_none_for_non_terminal() {
        let mut l = ReceiptLedger::new();
        let id = Uuid::new_v4();
        l.record(id, State::Processing);
        assert!(l.get(id).unwrap().outcome.is_none());
    }

    #[test]
    fn receipt_ledger_outcome_acked_for_completed_acked() {
        let mut l = ReceiptLedger::new();
        let id = Uuid::new_v4();
        l.record(id, State::Completed(CompletedTerminal::Acked));
        let entry = l.get(id).unwrap();
        assert!(entry.state.is_terminal());
        assert!(matches!(entry.outcome, Some(ReceiptOutcome::Acked)));
    }

    #[test]
    fn receipt_ledger_outcome_failed_variants() {
        let mut l = ReceiptLedger::new();
        let nack_id = Uuid::new_v4();
        l.record(
            nack_id,
            State::Failed(FailedTerminal::Nacked {
                reason: "checksum mismatch".to_string(),
            }),
        );
        let cancel_id = Uuid::new_v4();
        l.record(
            cancel_id,
            State::Failed(FailedTerminal::Cancelled {
                reason: "user cancelled".to_string(),
            }),
        );
        let err_id = Uuid::new_v4();
        l.record(
            err_id,
            State::Failed(FailedTerminal::Errored {
                code: 500,
                detail: "io".to_string(),
            }),
        );
        assert!(matches!(
            l.get(nack_id).unwrap().outcome,
            Some(ReceiptOutcome::Nacked(_))
        ));
        assert!(matches!(
            l.get(cancel_id).unwrap().outcome,
            Some(ReceiptOutcome::Cancelled(_))
        ));
        assert!(matches!(
            l.get(err_id).unwrap().outcome,
            Some(ReceiptOutcome::Failed { code: 500, .. })
        ));
    }

    #[test]
    fn receipt_ledger_terminal_and_pending_counts() {
        let mut l = ReceiptLedger::new();
        l.record(Uuid::new_v4(), State::Initiated);
        l.record(Uuid::new_v4(), State::StreamOpened);
        l.record(Uuid::new_v4(), State::Completed(CompletedTerminal::Acked));
        l.record(
            Uuid::new_v4(),
            State::Failed(FailedTerminal::Cancelled {
                reason: "x".to_string(),
            }),
        );
        assert_eq!(l.len(), 4);
        assert_eq!(l.pending_count(), 2);
        assert_eq!(l.terminal_count(), 2);
    }

    #[test]
    fn receipt_ledger_record_at_pins_timestamp() {
        let mut l = ReceiptLedger::new();
        let id = Uuid::new_v4();
        let t = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000);
        l.record_at(id, State::Processing, t);
        assert_eq!(l.get(id).unwrap().recorded_at, t);
    }

    #[test]
    fn receipt_ledger_get_returns_none_for_unknown_id() {
        let l = ReceiptLedger::new();
        assert!(l.get(Uuid::new_v4()).is_none());
    }

    #[test]
    fn receipt_ledger_iter_yields_every_entry() {
        let mut l = ReceiptLedger::new();
        let ids: Vec<_> = (0..5).map(|_| Uuid::new_v4()).collect();
        for &id in &ids {
            l.record(id, State::Initiated);
        }
        let seen: std::collections::HashSet<Uuid> =
            l.iter().map(|e| e.receipt_id).collect();
        for id in ids {
            assert!(seen.contains(&id));
        }
    }

    // ---- State::outcome() ---------------------------------------------------

    #[test]
    fn state_outcome_none_for_non_terminal_states() {
        assert!(State::Initiated.outcome().is_none());
        assert!(State::StreamOpened.outcome().is_none());
        assert!(State::DataTransferred.outcome().is_none());
        assert!(State::StreamClosed.outcome().is_none());
        assert!(State::Processing.outcome().is_none());
    }

    #[test]
    fn state_outcome_clones_terminal_payloads() {
        let s = State::Failed(FailedTerminal::Nacked {
            reason: "abc".to_string(),
        });
        match s.outcome() {
            Some(ReceiptOutcome::Nacked(reason)) => assert_eq!(reason, "abc"),
            other => panic!("expected Nacked, got {other:?}"),
        }
    }

    // ---- TransferSession constructor ----------------------------------------

    #[test]
    fn new_session_is_pending_with_created_event() {
        let s = TransferSession::new(sample_kind(), sample_manifest(), ProtocolKind::Http);
        assert_eq!(s.status, SessionStatus::Pending);
        assert!(s.completed_at.is_none());
        assert_eq!(s.events.len(), 1);
        let first = s.events.iter().next().cloned().unwrap();
        match first {
            SessionEvent::Created { status, .. } => {
                assert_eq!(status, SessionStatus::Pending);
            }
            other => panic!("expected Created, got {other:?}"),
        }
    }

    #[test]
    fn new_at_pins_started_at_and_event_timestamp() {
        let t = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        let s = TransferSession::new_at(
            sample_kind(),
            sample_manifest(),
            ProtocolKind::Quic,
            t,
        );
        assert_eq!(s.started_at, t);
        assert_eq!(s.events.iter().next().unwrap().at(), t);
    }

    // ---- TransferSession transitions ---------------------------------------

    #[test]
    fn pending_to_active_then_completed_is_legal() {
        let mut s =
            TransferSession::new(sample_kind(), sample_manifest(), ProtocolKind::Http);
        s.transition_to(SessionStatus::Active).unwrap();
        s.transition_to(SessionStatus::Completed).unwrap();
        assert_eq!(s.status, SessionStatus::Completed);
        assert!(s.completed_at.is_some());
        // Created + Active + Completed = 3 events
        assert_eq!(s.events.len(), 3);
    }

    #[test]
    fn pending_to_cancelled_is_legal() {
        let mut s =
            TransferSession::new(sample_kind(), sample_manifest(), ProtocolKind::Http);
        s.transition_to(SessionStatus::Cancelled).unwrap();
        assert_eq!(s.status, SessionStatus::Cancelled);
        assert!(s.completed_at.is_some());
    }

    #[test]
    fn active_to_failed_records_reason_and_terminal_stamp() {
        let mut s =
            TransferSession::new(sample_kind(), sample_manifest(), ProtocolKind::Http);
        s.transition_to(SessionStatus::Active).unwrap();
        s.transition_to(SessionStatus::Failed {
            reason: "disk full".into(),
        })
        .unwrap();
        assert!(matches!(s.status, SessionStatus::Failed { .. }));
        assert!(s.completed_at.is_some());
    }

    #[test]
    fn pending_to_completed_is_illegal() {
        let mut s =
            TransferSession::new(sample_kind(), sample_manifest(), ProtocolKind::Http);
        let err = s.transition_to(SessionStatus::Completed).unwrap_err();
        assert_eq!(err.from, SessionStatus::Pending);
        assert!(matches!(err.to, SessionStatus::Completed));
        // status untouched on illegal transition
        assert_eq!(s.status, SessionStatus::Pending);
        assert!(s.completed_at.is_none());
        // only the Created event recorded
        assert_eq!(s.events.len(), 1);
    }

    #[test]
    fn pending_to_failed_is_illegal() {
        let mut s =
            TransferSession::new(sample_kind(), sample_manifest(), ProtocolKind::Http);
        let err = s
            .transition_to(SessionStatus::Failed {
                reason: "x".into(),
            })
            .unwrap_err();
        assert_eq!(err.from, SessionStatus::Pending);
    }

    #[test]
    fn no_op_transition_is_illegal() {
        let mut s =
            TransferSession::new(sample_kind(), sample_manifest(), ProtocolKind::Http);
        let err = s.transition_to(SessionStatus::Pending).unwrap_err();
        assert_eq!(err.from, SessionStatus::Pending);
    }

    #[test]
    fn cannot_transition_out_of_completed() {
        let mut s =
            TransferSession::new(sample_kind(), sample_manifest(), ProtocolKind::Http);
        s.transition_to(SessionStatus::Active).unwrap();
        s.transition_to(SessionStatus::Completed).unwrap();
        let err = s.transition_to(SessionStatus::Active).unwrap_err();
        assert_eq!(err.from, SessionStatus::Completed);
    }

    #[test]
    fn cannot_transition_out_of_cancelled() {
        let mut s =
            TransferSession::new(sample_kind(), sample_manifest(), ProtocolKind::Http);
        s.transition_to(SessionStatus::Cancelled).unwrap();
        let err = s.transition_to(SessionStatus::Active).unwrap_err();
        assert_eq!(err.from, SessionStatus::Cancelled);
    }

    #[test]
    fn record_custom_appends_event_without_status_change() {
        let mut s =
            TransferSession::new(sample_kind(), sample_manifest(), ProtocolKind::Http);
        s.record_custom("task chunk-12 acked");
        assert_eq!(s.status, SessionStatus::Pending);
        assert_eq!(s.events.len(), 2);
        let last = s.events.iter().last().cloned().unwrap();
        match last {
            SessionEvent::Custom { msg, .. } => assert_eq!(msg, "task chunk-12 acked"),
            other => panic!("expected Custom, got {other:?}"),
        }
    }

    #[test]
    fn transfer_session_serde_round_trip() {
        let mut s = TransferSession::new_at(
            sample_kind(),
            sample_manifest(),
            ProtocolKind::Http,
            SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000),
        );
        s.transition_to_at(
            SessionStatus::Active,
            SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_001),
        )
        .unwrap();
        let json = serde_json::to_string(&s).unwrap();
        let back: TransferSession = serde_json::from_str(&json).unwrap();
        assert_eq!(s, back);
    }
}
