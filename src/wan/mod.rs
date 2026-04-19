//! WAN rendezvous and relay support (placeholder for v0.3.0).
//!
//! See `docs/rfcs/RFC-004-wan-rendezvous.md` (in progress) for the
//! full design. This module is intentionally empty in v0.2.1 — it
//! exists so feature flags `wan-rendezvous` and `wan-relay` are
//! addressable in downstream Cargo.toml files now, and so v0.3.0
//! can land the implementation without a breaking module-path
//! reshuffle.

#[cfg(feature = "wan-rendezvous")]
pub mod rendezvous {
    //! Placeholder. Implementation lands in v0.3.0 (RFC-004 R1, R5).
}

#[cfg(feature = "wan-relay")]
pub mod relay {
    //! Placeholder. Implementation lands in v0.3.0 (RFC-004 R3).
}
