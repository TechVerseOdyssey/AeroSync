//! WAN rendezvous and relay support — see `docs/rfcs/RFC-004-wan-rendezvous.md`.
//!
//! The standalone control-plane server lives in the **`aerosync-rendezvous`**
//! workspace crate. This module holds the main-crate client hooks.

#[cfg(all(feature = "wan-rendezvous", feature = "quic"))]
pub mod hole_punch;
#[cfg(feature = "wan-rendezvous")]
pub mod punch_signaling;
#[cfg(feature = "wan-rendezvous")]
pub mod rendezvous;

#[cfg(feature = "wan-relay")]
pub mod relay {
    //! Relay placeholder (RFC-004 R3); targeted for a later milestone.
}
