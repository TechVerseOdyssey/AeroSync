//! `TransferSession` companion types — [`crate::session::SessionId`],
//! [`crate::session::SessionKind`], [`crate::session::SessionStatus`].
//!
//! Per `docs/v0.3.0-refactor-plan.md` §3 Phase 3, this module owns the
//! pure-data foundations the upcoming `TransferSession` aggregate root
//! will reference. The aggregate root itself, plus its `FileManifest`
//! / `EventLog` / `ReceiptLedger` collaborators, are deferred to
//! Phases 3.2–3.5; landing the companions in isolation lets every
//! later phase target a stable type signature for tests and mocks
//! without a single giant PR.
//!
//! ## v0.3.0 Phase 3.1 status (skeleton)
//!
//! No existing AeroSync code references [`crate::session::SessionId`] yet — this is
//! purely additive infrastructure. Phases 3.4 / 3.5 will wire it
//! through `TransferEngine::send_with_metadata` and
//! `FileReceiver::accept_*` once the aggregate root lands. The
//! protobuf field that carries `session_id` over the wire is part of
//! Phase 3.4 as well.
//!
//! ## Naming
//!
//! [`crate::session::SessionId`] is the **upper-level semantic id** — one logical
//! batch transfer of N files == one session. `TaskId` (lives in the
//! root `aerosync` crate, attached to `TransferTask`) remains the
//! per-file data-stream id. Per refactor-plan §4 D3, both ids
//! coexist: `Receipt.session_id` becomes a new optional Python-side
//! getter; `Receipt.transfer_id` (`= TaskId`) stays for back-compat.
//!
//! ## Stability
//!
//! The three types defined here are not yet listed in
//! `docs/v0.3.0-frozen-api.md`; they are reachable only through the
//! module path (`aerosync_domain::session::SessionId`) and are
//! considered unstable until Phase 3.3 elevates a curated subset to
//! the crate root.

use std::fmt;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

// ──────────────────────────── SessionId ────────────────────────────────────────

/// Stable identifier for a `TransferSession`.
///
/// Wraps a UUID so the type is distinct from `TaskId` (also a UUID,
/// but per-file) at the type-checker level. Constructing a
/// `SessionId` from a raw `Uuid` is free (`From<Uuid>` /
/// [`SessionId::from_uuid`]); going the other way is also free
/// (`From<SessionId>` for `Uuid` / [`SessionId::into_uuid`]).
///
/// On the wire, `SessionId` serializes as the canonical
/// hyphenated lower-case UUID string thanks to `#[serde(transparent)]`
/// — the value rides as a JSON string, not an object. On the protobuf
/// side it will be a single `string` field once the aggregate-root
/// proto extension lands in v0.3.0 Phase 3.4.
///
/// The newtype is `Copy` because the inner `Uuid` is 16 bytes and the
/// callers (event ring buffers, receipt registry) clone it freely.
#[derive(Debug, Clone, Copy, Eq, PartialEq, Hash, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SessionId(Uuid);

impl SessionId {
    /// Generate a fresh random `SessionId`.
    ///
    /// Equivalent to `SessionId::from_uuid(Uuid::new_v4())`. Used by
    /// the future `TransferSession::new_send` / `new_receive`
    /// builders (Phase 3.3) when the caller does not supply an
    /// explicit id.
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }

    /// Wrap a pre-existing [`Uuid`] (e.g. one decoded from the wire
    /// or rehydrated from on-disk resume state).
    pub const fn from_uuid(id: Uuid) -> Self {
        Self(id)
    }

    /// Borrow the inner [`Uuid`].
    ///
    /// Useful for protobuf encoding (which still threads a raw UUID
    /// string) and for indexing into a `HashMap<Uuid, _>` keyed by
    /// the raw value.
    pub const fn as_uuid(&self) -> &Uuid {
        &self.0
    }

    /// Consume `self` and return the inner [`Uuid`].
    pub fn into_uuid(self) -> Uuid {
        self.0
    }
}

// `Default` is implemented manually (rather than `#[derive]`) because
// `Uuid::default()` returns the nil UUID (`00000000-…`); a derived
// `Default for SessionId` would silently mint a non-unique id and
// break any code that assumes "no two sessions share an id". The
// manual impl forwards to [`SessionId::new`] which uses `Uuid::new_v4`.
impl Default for SessionId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for SessionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.0, f)
    }
}

impl From<Uuid> for SessionId {
    fn from(id: Uuid) -> Self {
        Self(id)
    }
}

impl From<SessionId> for Uuid {
    fn from(id: SessionId) -> Self {
        id.0
    }
}

// ──────────────────────────── SessionKind ──────────────────────────────────────

/// Discriminates between sender-side and receiver-side sessions.
///
/// The two arms carry the destination/source root because the
/// `TransferSession` aggregate root needs both pieces at construction
/// time, and threading them as separate fields would let an invalid
/// state — e.g. a [`SessionKind::Send`] paired with a `save_root` —
/// slip through the type checker. Phase 3.4 / 3.5 builders will
/// pattern-match on this enum to dispatch to the sender or receiver
/// path.
///
/// Strings (rather than typed transport / listener handles) are used
/// at the domain layer to keep `aerosync-domain` transport-agnostic;
/// the root `aerosync` crate parses these into the concrete
/// `Endpoint` enum and `ListenerId` value object respectively.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SessionKind {
    /// Outbound session: this node is sending one or more files to
    /// `destination`.
    Send {
        /// Caller-supplied transport endpoint (URL, peer name, …).
        /// Stored as a string at the domain layer — the root
        /// `aerosync` crate parses it into the concrete `Endpoint`
        /// enum before dispatching to a transport adapter.
        destination: String,
        /// File-system root that the source paths in `FileManifest`
        /// (Phase 3.2) will be resolved against.
        source_root: PathBuf,
    },
    /// Inbound session: this node accepted a connection on
    /// `listener_id` and will save received files under `save_root`.
    Receive {
        /// Stable id of the listener that accepted the inbound
        /// session. Stored as a string at this layer; the root crate
        /// maps it back to the actual `FileReceiver` instance.
        listener_id: String,
        /// File-system root the receiver writes to.
        save_root: PathBuf,
    },
}

// ──────────────────────────── SessionStatus ────────────────────────────────────

/// Lifecycle state of a `TransferSession`.
///
/// State transitions (legal moves):
///
/// ```text
/// Pending ─► Active ─┬─► Completed
///                    ├─► Failed { reason }
///                    └─► Cancelled
/// ```
///
/// `Pending` is the construction-time state before the aggregate root
/// has dispatched its first task. `Active` covers the entire window
/// during which at least one task is in flight. The three terminal
/// states ([`SessionStatus::Completed`], [`SessionStatus::Failed`],
/// [`SessionStatus::Cancelled`]) are mutually exclusive and cannot be
/// left.
///
/// The transition rules are NOT enforced by this enum directly —
/// Phase 3.3 will add a `TransferSession::transition_to` method that
/// returns an error on illegal moves. Until then the enum is purely a
/// data tag.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum SessionStatus {
    /// Constructed but no task started yet.
    Pending,
    /// At least one task in flight.
    Active,
    /// All tasks finished successfully (terminal).
    Completed,
    /// At least one task failed terminally (terminal).
    Failed {
        /// Human-readable reason for diagnostics. NOT a stable error
        /// code — that's [`crate::error::AeroSyncError`]'s job. Phase
        /// 3.3 will populate this from the failing `TransferTask`'s
        /// error via `AeroSyncError::to_string()`.
        reason: String,
    },
    /// User or remote initiated a cancel before all tasks completed
    /// (terminal).
    Cancelled,
}

impl SessionStatus {
    /// `true` iff the session has reached one of the three terminal
    /// states ([`SessionStatus::Completed`], [`SessionStatus::Failed`],
    /// [`SessionStatus::Cancelled`]).
    ///
    /// Useful for the startup-recovery iterator that Phase 3.3 will
    /// add: it walks every persisted session and re-queues only the
    /// non-terminal ones.
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            SessionStatus::Completed | SessionStatus::Failed { .. } | SessionStatus::Cancelled
        )
    }
}

// ──────────────────────────── Tests ────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn session_id_new_produces_unique_ids() {
        // 100 freshly-minted v4 UUIDs colliding is statistically
        // ~10⁻³⁴; a hit means our `new()` accidentally returned the
        // nil UUID (or a fixed seed) and the regression is loud.
        let mut seen: HashSet<SessionId> = HashSet::new();
        for _ in 0..100 {
            assert!(seen.insert(SessionId::new()));
        }
        assert_eq!(seen.len(), 100);
    }

    #[test]
    fn session_id_serde_round_trips_as_quoted_uuid_string() {
        let id = SessionId::new();
        let json = serde_json::to_string(&id).expect("serialize");
        // `#[serde(transparent)]` makes the value a JSON string,
        // not an object; verify the framing explicitly so a future
        // refactor that drops the attribute fails this test instead
        // of silently breaking the wire format.
        assert!(json.starts_with('"') && json.ends_with('"'));
        assert!(!json.contains('{'));
        let back: SessionId = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(id, back);
    }

    #[test]
    fn session_id_uuid_round_trip_via_from_into() {
        let raw = Uuid::new_v4();
        let id: SessionId = raw.into();
        assert_eq!(id.as_uuid(), &raw);
        let back: Uuid = id.into();
        assert_eq!(back, raw);
    }

    #[test]
    fn session_id_display_matches_uuid_canonical_form() {
        let raw = Uuid::new_v4();
        let id = SessionId::from_uuid(raw);
        assert_eq!(id.to_string(), raw.to_string());
        // Canonical hyphenated form is 36 chars — guards against an
        // accidental switch to `simple` / `urn` formats.
        assert_eq!(id.to_string().len(), 36);
    }

    #[test]
    fn session_id_default_is_random_not_nil() {
        // `Uuid::default()` is the nil UUID; if we ever switch to a
        // derived `Default` for `SessionId`, this test fires.
        let a = SessionId::default();
        let b = SessionId::default();
        assert_ne!(a, b);
        assert_ne!(*a.as_uuid(), Uuid::nil());
    }

    #[test]
    fn session_kind_send_json_round_trip() {
        let kind = SessionKind::Send {
            destination: "https://example.invalid/upload".to_string(),
            source_root: PathBuf::from("/tmp/source"),
        };
        let json = serde_json::to_string(&kind).expect("serialize");
        let back: SessionKind = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(kind, back);
    }

    #[test]
    fn session_kind_receive_json_round_trip_with_relative_path() {
        // Use a relative `PathBuf` to make sure serde does not coerce
        // the value through `Path::canonicalize` (it shouldn't — the
        // domain layer is filesystem-agnostic).
        let kind = SessionKind::Receive {
            listener_id: "listener-a".to_string(),
            save_root: PathBuf::from("./inbox/2026"),
        };
        let json = serde_json::to_string(&kind).expect("serialize");
        let back: SessionKind = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(kind, back);
        if let SessionKind::Receive { save_root, .. } = back {
            assert_eq!(save_root, PathBuf::from("./inbox/2026"));
        } else {
            panic!("expected Receive variant after round trip");
        }
    }

    #[test]
    fn session_kind_tag_discriminator_is_kind() {
        let send = SessionKind::Send {
            destination: "d".to_string(),
            source_root: PathBuf::from("/s"),
        };
        let recv = SessionKind::Receive {
            listener_id: "l".to_string(),
            save_root: PathBuf::from("/r"),
        };
        let send_json = serde_json::to_string(&send).expect("serialize send");
        let recv_json = serde_json::to_string(&recv).expect("serialize recv");
        assert!(
            send_json.contains("\"kind\":\"send\""),
            "expected snake_case tag, got: {send_json}"
        );
        assert!(
            recv_json.contains("\"kind\":\"receive\""),
            "expected snake_case tag, got: {recv_json}"
        );
    }

    #[test]
    fn session_status_is_terminal_truth_table() {
        assert!(!SessionStatus::Pending.is_terminal());
        assert!(!SessionStatus::Active.is_terminal());
        assert!(SessionStatus::Completed.is_terminal());
        assert!(SessionStatus::Failed {
            reason: "disk full".to_string(),
        }
        .is_terminal());
        assert!(SessionStatus::Cancelled.is_terminal());
    }

    #[test]
    fn session_status_failed_carries_reason_through_json() {
        let status = SessionStatus::Failed {
            reason: "remote closed connection".to_string(),
        };
        let json = serde_json::to_string(&status).expect("serialize");
        let back: SessionStatus = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(status, back);
        if let SessionStatus::Failed { reason } = back {
            assert_eq!(reason, "remote closed connection");
        } else {
            panic!("expected Failed variant after round trip");
        }
    }

    #[test]
    fn session_status_tag_discriminator_is_status() {
        let pending = serde_json::to_string(&SessionStatus::Pending).expect("serialize");
        let completed = serde_json::to_string(&SessionStatus::Completed).expect("serialize");
        let failed = serde_json::to_string(&SessionStatus::Failed {
            reason: "io".to_string(),
        })
        .expect("serialize");
        assert!(pending.contains("\"status\":\"pending\""), "got: {pending}");
        assert!(
            completed.contains("\"status\":\"completed\""),
            "got: {completed}"
        );
        assert!(failed.contains("\"status\":\"failed\""), "got: {failed}");
    }
}
