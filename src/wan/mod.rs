//! WAN rendezvous and relay support — rc1 placeholder; full WAN
//! landing targeted for **v0.4** per RFC-004.
//!
//! See `docs/rfcs/RFC-004-wan-rendezvous.md` (in progress) for the
//! full design. This module is intentionally empty in v0.3.0-rc1 —
//! it exists so feature flags `wan-rendezvous` and `wan-relay` are
//! addressable in downstream Cargo.toml files now, and so v0.4 can
//! land the implementation without a breaking module-path reshuffle.

#[cfg(feature = "wan-rendezvous")]
pub mod rendezvous {
    //! rc1 placeholder; full WAN landing targeted for v0.4 per
    //! RFC-004 (R1, R5).
}

#[cfg(feature = "wan-relay")]
pub mod relay {
    //! rc1 placeholder; full WAN landing targeted for v0.4 per
    //! RFC-004 (R3).
}
