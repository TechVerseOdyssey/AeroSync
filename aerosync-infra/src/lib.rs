//! # `aerosync-infra` — infrastructure layer for AeroSync
//!
//! This crate is the v0.3.0 home for the AeroSync **infrastructure
//! layer** as prescribed by `docs/ARCHITECTURE_AND_DESIGN.md` §4.4
//! and the refactor plan `docs/v0.3.0-refactor-plan.md`. It owns the
//! concrete IO-bound impls of the trait abstractions defined in
//! [`aerosync_domain`] — JSON / JSONL persistence, the rustls crypto
//! provider bootstrap, and the audit logger.
//!
//! ## Status (v0.3.0-rc1 baseline)
//!
//! Phase 1 of the refactor lands the **crate skeleton only** —
//! everything below is a stub. Phase 1 follow-ups will `git mv` files
//! into here from `src/core/` (preserving `git blame`) without
//! changing any behavior. Phase 2 wires the impls behind the
//! [`aerosync_domain::storage`] traits.
//!
//! ## What lives here
//!
//! | Module     | Phase | Contents                                                  |
//! |------------|-------|-----------------------------------------------------------|
//! | `tls`      | 1     | `ensure_rustls_provider_installed` (moved from `core::tls`)|
//! | `audit`    | 1     | `AuditLogger`, JSONL append writer (moved from `core::audit`)|
//! | `history`  | 2     | `JsonlHistoryStore impl HistoryStorage` (rename from `HistoryStore`)|
//! | `resume`   | 2     | `JsonResumeStore impl ResumeStorage` + atomic-write fix   |
//! | `config`   | 4     | `AeroSyncConfig` TOML loader (split from `src/config.rs`) |
//!
//! ## What does NOT live here
//!
//! - **No domain logic.** The `Receipt` state machine, `TransferSession`,
//!   and `Storage` trait *shapes* live in [`aerosync_domain`]; this
//!   crate only provides their concrete impls.
//! - **No transport / wire code.** HTTP / QUIC / S3 / FTP transports
//!   stay in the root `aerosync` crate under `src/protocols/`.
//! - **No CLI / MCP / Python binding code.** Those are presentation
//!   layers and live in their own crates.
//!
//! ## Stability contract
//!
//! Per `docs/v0.3.0-frozen-api.md` §1.6, every public symbol the root
//! `aerosync` crate currently exposes (e.g. `aerosync::core::tls::*`,
//! `aerosync::core::audit::*`) MUST stay resolvable after the file
//! moves complete — the root crate re-exports symbols from this crate
//! via `pub use aerosync_infra::tls::*` etc. Renaming or removing a
//! re-exported symbol is a v0.4.0 break.

#![forbid(unsafe_code)]
#![warn(missing_docs)]
#![warn(rust_2018_idioms)]

// ── Phase 1 modules ───────────────────────────────────────────────────

/// JSONL audit-trail logger ([`audit::AuditLogger`]) — append-only
/// transfer/auth/MCP-call records keyed by a UTC timestamp. Migrated
/// verbatim from `src/core/audit.rs` in Phase 1d; the temporary
/// `#[allow(missing_docs)]` is retired here (Phase 4b) after
/// backfilling field-level docs on the public enums and struct.
pub mod audit;
pub mod tls;

// ── Phase 2 modules ───────────────────────────────────────────────────

/// File-backed JSON [`aerosync_domain::storage::ResumeStorage`] impl
/// ([`resume::ResumeStore`]). Migrated from `src/core/resume.rs` in
/// Phase 2.2 with one behavioural upgrade: [`resume::ResumeStore::save`]
/// is now crash-safe via tmp+rename (was vulnerable to torn writes
/// pre-v0.3.0). Re-exports the `ResumeState` / `ChunkState` /
/// `ResumeStorage` / `DEFAULT_CHUNK_SIZE` symbols from
/// [`aerosync_domain::storage`] so the legacy
/// `aerosync::core::resume::*` import path keeps resolving via the
/// `pub use aerosync_infra::resume;` shim in `src/core/mod.rs`. Phase
/// 4d (this commit) retired the previous `#[allow(missing_docs)]` by
/// adding a single block doc on the re-export — the field-level docs
/// live with the canonical definitions in `aerosync_domain::storage`.
pub mod resume;
// pub mod history;     // Phase 2.3 (deferred — see src/core/history.rs)

// ── Phase 4 modules ───────────────────────────────────────────────────
//
// pub mod config;

/// Crate version string, exposed for diagnostic output. Matches the
/// `Cargo.toml` `version.workspace` value at build time.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_string_is_non_empty() {
        // Same smoke check as `aerosync-domain` — guards against
        // workspace version drift that would otherwise only surface
        // at publish time.
        assert!(!VERSION.is_empty());
        assert!(VERSION.starts_with("0."));
    }

    #[test]
    fn domain_dep_resolves() {
        // Cheap integration check: confirm the `aerosync-domain`
        // workspace dep is correctly wired in `Cargo.toml`. If
        // `aerosync_domain::VERSION` ever fails to resolve, the
        // whole storage-trait layering is broken.
        assert!(!aerosync_domain::VERSION.is_empty());
    }
}
