//! WAN rendezvous and relay support — rc1 placeholder; full WAN
//! landing targeted for **v0.4** per RFC-004.
//!
//! See `docs/rfcs/RFC-004-wan-rendezvous.md`. The standalone control-plane
//! binary and schema scaffolding live in the **`aerosync-rendezvous`**
//! workspace crate (`cargo run -p aerosync-rendezvous`); this module stays
//! a lightweight hook so feature flags `wan-rendezvous` and `wan-relay`
//! resolve in downstream `Cargo.toml` files today without a breaking
//! module-path reshuffle when client-side integration lands.

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
