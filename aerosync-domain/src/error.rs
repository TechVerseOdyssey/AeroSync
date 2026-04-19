//! Top-level error type for the AeroSync domain layer.
//!
//! [`AeroSyncError`] is the canonical error returned by every fallible
//! operation in `aerosync-domain` and is re-exported through the root
//! `aerosync` crate as `aerosync::AeroSyncError`. The Python binding
//! maps each variant onto a corresponding subclass of
//! `aerosync.AeroSyncError` (RFC-001 §5.8 — see
//! `aerosync-py/src/errors.rs`).
//!
//! ## Migration note (v0.3.0 Phase 1c)
//!
//! Source moved verbatim from `aerosync::core::error` to
//! `aerosync_domain::error`. The original path keeps resolving via a
//! `pub use aerosync_domain::error` re-export in `src/core/mod.rs`,
//! and the variant set is byte-identical to v0.2.1 — no caller had
//! to change.
//!
//! Two `From<toml::de::Error>` / `From<toml::ser::Error>` impls that
//! existed in v0.2.x were dropped during the move because every call
//! site in the workspace already used explicit
//! `.map_err(AeroSyncError::System(...))` / `.map_err(...TomlParse...)`
//! conversions — the auto-`?` propagation surface they enabled was
//! never exercised. Dropping them keeps `aerosync-domain` free of any
//! TOML dependency, which is an infrastructure concern.
//! If a future caller needs `?` ergonomics again, add
//! `aerosync-infra::config::TomlError` as an extension point and keep
//! the From impl over there — orphan rules permit it because the
//! infra crate owns its own wrapper type, while AeroSyncError is
//! visible there too.

use thiserror::Error;

/// Convenience `Result` alias for `aerosync-domain` operations.
///
/// Equivalent to `std::result::Result<T, AeroSyncError>`. Re-exported
/// from the root `aerosync` crate as `aerosync::Result<T>`.
pub type Result<T> = std::result::Result<T, AeroSyncError>;

/// Domain-level error variants for AeroSync.
///
/// Each variant maps to a structured Python exception class via the
/// PyO3 binding (`aerosync-py/src/errors.rs`); the variant name doubles
/// as the Python `code` attribute (snake_case) per RFC-001 §5.8. New
/// variants land at the end so older error matches stay exhaustive
/// against `_` patterns.
#[derive(Error, Debug)]
pub enum AeroSyncError {
    /// File or directory I/O failure surfaced from `std::fs` / `tokio::fs`.
    #[error("File I/O error: {0}")]
    FileIo(#[from] std::io::Error),

    /// Network-level failure (DNS, connect, TLS handshake, HTTP/QUIC
    /// transport error). The String carries the human-readable detail.
    #[error("Network error: {0}")]
    Network(String),

    /// Persistent storage failure (JSON / JSONL append, SQLite future).
    #[error("Storage error: {message}")]
    Storage {
        /// Human-readable detail forwarded to logs and the Python
        /// `.detail` attribute.
        message: String,
    },

    /// User-initiated cancel via `Receipt::cancel()` or CLI `Ctrl-C`.
    /// Distinguished from a remote cancel (which surfaces as a
    /// `Receipt` terminal state with `Outcome::Cancelled`).
    #[error("Transfer cancelled by user")]
    Cancelled,

    /// Construction-time configuration validation error
    /// (e.g. negative chunk size, unknown log level).
    #[error("Invalid configuration: {0}")]
    InvalidConfig(String),

    /// Wire-protocol violation — handshake mismatch, frame parse
    /// error, unexpected stream prefix, etc.
    #[error("Protocol error: {0}")]
    Protocol(String),

    /// Unspecified system-level failure that does not fit the more
    /// specific variants above.
    #[error("System error: {0}")]
    System(String),

    /// Reserved for future categories that have not yet earned their
    /// own variant. Avoid for new code in v0.3+.
    #[error("Unknown error: {0}")]
    Unknown(String),

    /// Authentication / authorization failure — bad token, missing
    /// `Authorization` header, ACL deny.
    #[error("Authentication error: {0}")]
    Auth(String),

    /// Generic configuration error not specific to one of the more
    /// targeted variants (`InvalidConfig`, `TomlParse`).
    #[error("Configuration error: {0}")]
    Config(String),

    /// TOML parsing failure (malformed config file, bad token store,
    /// …). Constructed explicitly via
    /// `.map_err(|e| AeroSyncError::TomlParse(e.to_string()))` at the
    /// call site since `aerosync-domain` does not depend on the
    /// `toml` crate.
    #[error("TOML parsing error: {0}")]
    TomlParse(String),
}
