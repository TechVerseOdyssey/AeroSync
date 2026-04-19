//! Shared TLS infrastructure used by both the HTTPS receiver
//! (`core::server::start_https_server`) and the QUIC stack
//! (`protocols::quic`).
//!
//! Living at the `core` layer (rather than nested under
//! `protocols::quic`) means the `rustls` crypto provider can be
//! registered by the HTTPS server even when the `quic` Cargo
//! feature is disabled. Before this module existed, server.rs
//! reached into `protocols::quic::ensure_crypto_provider_installed`
//! which made gating QUIC behind a feature awkward.

/// Install the `ring`-backed rustls crypto provider as the process
/// default if no provider has been registered yet.
///
/// Idempotent across calls (a [`std::sync::Once`] gates the install
/// attempt) and tolerant of an already-registered provider — the
/// `install_default()` `Result` is intentionally swallowed, since
/// "another provider is already in place" is exactly the post-
/// condition we want.
pub(crate) fn ensure_rustls_provider_installed() {
    use std::sync::Once;
    static INIT: Once = Once::new();
    INIT.call_once(|| {
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}
