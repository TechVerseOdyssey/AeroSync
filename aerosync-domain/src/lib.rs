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
//! ## Status (v0.3.0-rc1 baseline)
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

// `metadata` migrated verbatim from `src/core/metadata.rs` in
// Phase 1e. Original file did not enforce `missing_docs`; rustdoc
// completeness deferred to Phase 4 per refactor plan §3 Phase 4.
#[allow(missing_docs)]
pub mod metadata;

// `receipt` (state machine extraction) — DEFERRED to Phase 3 because
// `TransferSession` (the Phase 3 aggregate root) will reorganize
// receipt logic anyway. Doing the split twice is wasteful.

// ── Phase 2 modules ───────────────────────────────────────────────────

/// Storage abstractions ([`ResumeStorage`], [`HistoryStorage`]) and
/// the pure-data value objects they transit. See `storage.rs` for the
/// rationale of splitting data ↔ trait ↔ impl across three crates.
///
/// `missing_docs` is locally suppressed: a few field doc comments
/// were never written for the original `HistoryEntry` (id, filename,
/// size, sha256) in `src/core/history.rs`, and Phase 2.1b is a pure
/// rename — adding fresh docs now would conflate moves with content
/// changes. Phase 4 (documentation closure) retires this attribute.
#[allow(missing_docs)]
pub mod storage;

// ── Crate-root re-exports ─────────────────────────────────────────────
//
// Mirrors what the root `aerosync` crate already exposes via
// `pub use crate::core::error::{AeroSyncError, Result}`. Lifting the
// most-used names to the domain crate root lets infra-layer callers
// write `use aerosync_domain::{AeroSyncError, Result}` instead of
// the longer `use aerosync_domain::error::{AeroSyncError, Result}`.

pub use error::{AeroSyncError, Result};

// ── Phase 2 modules ───────────────────────────────────────────────────
//
// pub mod storage;

// ── Phase 3 modules ───────────────────────────────────────────────────
//
// pub mod session;
// pub mod manifest;

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
