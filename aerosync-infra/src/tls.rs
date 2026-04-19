//! Shared TLS infrastructure used by both the HTTPS receiver
//! (`aerosync::core::server::start_https_server`) and the QUIC stack
//! (`aerosync::protocols::quic`).
//!
//! Living in the `aerosync-infra` crate (rather than nested under
//! `protocols::quic`) means the `rustls` crypto provider can be
//! registered by the HTTPS server even when the `quic` Cargo feature
//! is disabled. Before this module existed, `server.rs` reached into
//! `protocols::quic::ensure_crypto_provider_installed`, which made
//! gating QUIC behind a feature awkward.
//!
//! ## Migration note (v0.3.0)
//!
//! This module previously lived at `aerosync::core::tls`. As part of
//! the v0.3.0 DDD refactor (`docs/v0.3.0-refactor-plan.md` Phase 1)
//! its source moved here verbatim — no behavior change. The original
//! path `aerosync::core::tls::ensure_rustls_provider_installed` keeps
//! resolving via a `pub(crate) use` re-export in `src/core/mod.rs`,
//! so callers inside the root `aerosync` crate did not need to
//! update their imports.

/// Install the `ring`-backed rustls crypto provider as the process
/// default if no provider has been registered yet.
///
/// Idempotent across calls (a [`std::sync::Once`] gates the install
/// attempt) and tolerant of an already-registered provider — the
/// `install_default()` `Result` is intentionally swallowed, since
/// "another provider is already in place" is exactly the post-
/// condition we want.
///
/// ## Visibility
///
/// `pub` (rather than the original `pub(crate)`): the function now
/// lives in a separate crate from its callers in the root `aerosync`
/// crate, so it must be reachable across the crate boundary. The
/// re-export in `aerosync::core::mod` is `pub(crate) use`, so the
/// effective external visibility from the root `aerosync` crate's
/// perspective is unchanged — only library embedders that opt-in to
/// `aerosync-infra` directly can reach the function under its new
/// canonical path.
pub fn ensure_rustls_provider_installed() {
    use std::sync::Once;
    static INIT: Once = Once::new();
    INIT.call_once(|| {
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}
