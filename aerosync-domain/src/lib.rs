//! # `aerosync-domain` — pure domain logic for AeroSync
//!
//! This crate is the v0.3.0 home for the AeroSync **domain layer** as
//! prescribed by `docs/ARCHITECTURE_AND_DESIGN.md` §4.4 and the
//! refactor plan `docs/v0.3.0-refactor-plan.md`. It exists so that
//! when RFC-004 (WAN rendezvous, ~3 K LoC of new code) lands in v0.4.0
//! the boundary between *what AeroSync knows* (this crate) and *how it
//! does IO* (`aerosync-infra` + the root `aerosync` crate) is already
//! clean and load-bearing.
//!
//! ## Status (v0.3.0 baseline)
//!
//! Phase 1 of the refactor lands the **crate skeleton only** —
//! everything below is a stub. Phase 1 follow-ups will `git mv` files
//! into here from `src/core/` (preserving `git blame`) without
//! changing any behavior. Phase 2 introduces the [`storage`] traits.
//! Phase 3 introduces the [`session`] aggregate root.
//!
//! ## What lives here
//!
//! | Module      | Phase | Contents                                                  |
//! |-------------|-------|-----------------------------------------------------------|
//! | `error`     | 1     | `AeroSyncError`, `Result<T>` (moved from `core::error`)   |
//! | `metadata`  | 1     | `MetadataBuilder`, system field constants (RFC-003 §4)    |
//! | `receipt`   | 1     | `Receipt`, `Sender`, `RxSide`, `State`, `Event`, `Outcome`|
//! | `storage`   | 2     | `ResumeStorage`, `HistoryStorage` async traits            |
//! | `session`   | 3     | `TransferSession`, `SessionId`, `SessionKind`, `EventLog` |
//! | `manifest`  | 3     | `FileManifest`, `FileEntry`, `Hash`, `ChunkPlan`          |
//!
//! ## What does NOT live here
//!
//! - **No tokio runtime types** (`TcpListener`, `tokio::fs::*`). Domain
//!   types are runtime-agnostic — only `tokio::sync::{watch, mpsc}` for
//!   receipt change notifications is allowed (see refactor plan §4 D1).
//! - **No transport implementations** (HTTP / QUIC / S3 / FTP). Those
//!   live in the root `aerosync` crate under `src/protocols/`.
//! - **No filesystem persistence**. JSON / JSONL impls of the storage
//!   traits live in `aerosync-infra`; this crate only owns the trait
//!   shapes.
//! - **No mDNS / WAN / receiver server**. Those are pure infra.
//!
//! ## Stability contract
//!
//! Every type re-exported from this crate's root MUST also be reachable
//! through the root `aerosync::` namespace via `pub use` in
//! `src/lib.rs`. The list of frozen symbols is documented in
//! `docs/v0.3.0-frozen-api.md` §1; any change here that breaks an entry
//! in that list requires a v0.4.0 release.

#![forbid(unsafe_code)]
#![warn(missing_docs)]
#![warn(rust_2018_idioms)]

// ── Phase 1 modules ───────────────────────────────────────────────────

pub mod error;

/// RFC-003 metadata envelope: builder, validator, lifecycle helpers,
/// and the JSON-serializable [`metadata::MetadataJson`] adapter.
/// Migrated verbatim from `src/core/metadata.rs` in Phase 1e; the
/// temporary `#[allow(missing_docs)]` is retired here (Phase 4b)
/// after backfilling field-level docs on the few public structs that
/// shipped without them.
pub mod metadata;

/// Per-receipt state machine ([`receipt::Receipt`] + side markers
/// [`receipt::Sender`] / [`receipt::Receiver`], the [`receipt::State`]
/// machine, [`receipt::Event`] inputs, terminal payloads
/// [`receipt::CompletedTerminal`] / [`receipt::FailedTerminal`] /
/// [`receipt::Outcome`], and the [`receipt::StateError`]
/// invalid-transition error). Migrated verbatim from
/// `src/core/receipt.rs` in v0.3.0 Phase 3.4a (the move was deferred
/// from Phase 1f to land alongside the Phase 3 aggregate-root work
/// per refactor-plan §3). The module's only deps are
/// `tokio::sync::watch` (allowed per refactor-plan §4 D1), `uuid`,
/// and `std::{fmt, marker::PhantomData}` — no transport, no
/// filesystem, no protobuf coupling. The legacy
/// `aerosync::core::receipt::*` import path keeps resolving via the
/// `pub use aerosync_domain::receipt;` shim in `src/core/mod.rs`.
pub mod receipt;

// ── Phase 2 modules ───────────────────────────────────────────────────

/// Storage abstractions ([`crate::storage::ResumeStorage`], [`crate::storage::HistoryStorage`]) and
/// the pure-data value objects they transit. See `storage.rs` for the
/// rationale of splitting data ↔ trait ↔ impl across three crates.
///
/// All field-level docs are now in place — the temporary
/// `#[allow(missing_docs)]` introduced in Phase 2.1b was retired by
/// the v0.3.0 Phase 4 doc-closure follow-up that backfilled
/// `HistoryEntry::{id, filename, size, sha256}`. The audit + metadata
/// modules in `aerosync-infra` still carry the lint suppression and
/// will be retired by the same Phase 4 task once their fields are
/// likewise documented.
pub mod storage;

// ── Phase 3 modules ───────────────────────────────────────────────────

/// `TransferSession` aggregate-root companions:
/// [`session::SessionId`] newtype, [`session::SessionKind`]
/// (Send / Receive discriminator), [`session::SessionStatus`]
/// lifecycle enum. The aggregate root itself (`TransferSession`)
/// lands in Phase 3.3; the file-manifest / event-log helpers in
/// 3.2 / 3.3. This module exists in v0.3.0 Phase 3.1 as the stable
/// type-signature foundation that later phases (sender-path
/// migration in 3.4, receiver-path migration in 3.5) can reference
/// without churn. No re-export at the crate root yet — Phase 3.3
/// will decide what to surface.
pub mod session;

/// File-payload value objects: [`manifest::Hash`] (typed content
/// digest), [`manifest::FileEntry`] (one file in a manifest),
/// [`manifest::FileManifest`] (ordered collection of entries with
/// path-safety invariants), and [`manifest::ChunkPlan`] /
/// [`manifest::ChunkSpec`] (pure-arithmetic slicing of a single file
/// for resumable upload). Phase 3.3 composes these into
/// [`transfer_session::TransferSession`]. No re-export at the crate
/// root yet — Phase 3.4 will decide what to surface.
pub mod manifest;

/// `TransferSession` aggregate root + supporting types
/// ([`transfer_session::ProtocolKind`], [`transfer_session::EventLog`],
/// [`transfer_session::SessionEvent`],
/// [`transfer_session::SessionStateError`]). Composes the Phase 3.1
/// session companions and the Phase 3.2 manifest into a single
/// state-machine-validated aggregate. `ReceiptLedger` and the
/// `task_ids` field are deferred to Phase 3.4 because they need
/// `Receipt` (still in the root crate) and a domain-level `TaskId`
/// design decision. No consumer wiring yet — sender path is Phase
/// 3.4, receiver path is Phase 3.5.
pub mod transfer_session;

// ── Crate-root re-exports ─────────────────────────────────────────────
//
// Mirrors what the root `aerosync` crate already exposes via
// `pub use crate::core::error::{AeroSyncError, Result}`. Lifting the
// most-used names to the domain crate root lets infra-layer callers
// write `use aerosync_domain::{AeroSyncError, Result}` instead of
// the longer `use aerosync_domain::error::{AeroSyncError, Result}`.

pub use error::{AeroSyncError, Result};

/// Crate version string, exposed for diagnostic output. Matches the
/// `Cargo.toml` `version.workspace` value at build time.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_string_is_non_empty() {
        // Smoke check that `env!` resolved against the workspace
        // version. If this ever returns "" the cargo workspace
        // configuration has drifted and the published crate would
        // ship without a version banner.
        assert!(!VERSION.is_empty());
        assert!(VERSION.starts_with("0."));
    }
}
