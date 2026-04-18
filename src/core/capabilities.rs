//! Negotiable peer capabilities for the `aerosync/1` wire protocol.
//!
//! Per RFC-002 §6.5, every QUIC handshake carries a 4-byte capability
//! bitmask so v0.3+ peers can opt into new behaviour without bumping
//! the ALPN string. This module owns the canonical bit layout and the
//! encode/decode/intersect helpers; it is intentionally **transport-
//! agnostic**.
//!
//! # Wired into transport in Week 2
//!
//! Today this type is pure — nothing on the QUIC handshake path
//! actually exchanges it yet. That wiring happens in Week 2 alongside
//! the bidirectional receipt stream (RFC-002 task #4). Until then this
//! module exists so:
//!
//!   1. The bit layout is reviewable in isolation.
//!   2. Tests can pin the encode / decode contract today rather than
//!      retrofitting them under the time pressure of stream wiring.
//!   3. The Python SDK (RFC-001) and any future TS / Go binding can
//!      already share the same bit assignments.
//!
//! # Bit layout
//!
//! | Bit  | Constant                  | Meaning                                                  |
//! | :--- | :------------------------ | :------------------------------------------------------- |
//! |  0   | [`Flag::Receipts`]        | Receipt protocol enabled (always set in v0.2).           |
//! |  1   | [`Flag::BytesReceived`]   | Receiver emits `BytesReceived` frames (chunk_size ≤ 1 MiB). |
//! | 2-31 | reserved                  | MUST be zero on send; MUST be ignored on decode.         |
//!
//! Reserved bits land in future RFCs (rendezvous identity, signed
//! receipts, …). On decode we mask them off so a peer running a newer
//! version cannot trick us into honouring bits we don't yet know.

use std::fmt;

/// Bit 0 — receipt protocol support. Always set in v0.2.
///
/// Exposed at module level for callers that prefer named constants
/// over [`Flag::Receipts`]`.mask()`.
pub const SUPPORTS_RECEIPTS: u32 = 1 << 0;

/// Bit 1 — receiver advertises it will emit `BytesReceived` progress
/// frames. RFC-002 §13 Q1 (resolved): set iff the negotiated
/// `chunk_size` is at most [`BYTES_RECEIVED_CHUNK_THRESHOLD`].
pub const SUPPORTS_BYTES_RECEIVED: u32 = 1 << 1;

/// Largest chunk size (bytes) for which `BytesReceived` progress
/// frames are still cheap enough to be worth sending. Above this the
/// receipt stream becomes mostly empty noise and the bit is dropped.
///
/// Per RFC-002 §13 Q1.
pub const BYTES_RECEIVED_CHUNK_THRESHOLD: u32 = 1024 * 1024;

/// Mask of bits defined by this version of AeroSync. Anything outside
/// this mask is reserved and MUST be ignored on decode / cleared on
/// encode. Adding a new flag means widening this constant in lockstep
/// with [`Flag::all`].
const KNOWN_BITS: u32 = SUPPORTS_RECEIPTS | SUPPORTS_BYTES_RECEIVED;

/// Individual capability flags. The numeric value of each variant is
/// the bit position (0-indexed); use [`Flag::mask`] to get the bit
/// itself.
#[derive(Debug, Clone, Copy, Eq, PartialEq, Hash)]
#[repr(u8)]
pub enum Flag {
    /// Bit 0 — receipt protocol (RFC-002). Mandatory in v0.2.
    Receipts = 0,
    /// Bit 1 — receiver emits `BytesReceived` progress frames. Set
    /// when `chunk_size <= 1 MiB` (RFC-002 §13 Q1, accepted default).
    BytesReceived = 1,
}

impl Flag {
    /// Returns the bit position (0..32) for this flag.
    pub fn bit(self) -> u8 {
        self as u8
    }

    /// Returns the single-bit mask for this flag.
    pub fn mask(self) -> u32 {
        1u32 << (self as u8)
    }

    /// Every flag known to this version of the wire protocol. Useful
    /// for iterating in tests and for sanity-checking that
    /// `KNOWN_BITS` stays in sync with the enum.
    pub fn all() -> &'static [Flag] {
        &[Flag::Receipts, Flag::BytesReceived]
    }
}

/// 4-byte capability bitmask carried in [`aerosync_proto::Handshake`].
///
/// Construct via [`Capabilities::v0_2_default`] for the standard v0.2
/// peer profile, or via [`Capabilities::with_flags`] for tests and
/// custom negotiation experiments.
#[derive(Clone, Copy, Eq, PartialEq, Hash, Default)]
pub struct Capabilities(u32);

impl Capabilities {
    /// Empty capability set — nothing supported. Useful for tests and
    /// as the identity element of [`Capabilities::intersect`].
    pub const EMPTY: Capabilities = Capabilities(0);

    /// Construct from a raw bitmask. Reserved bits are stripped on
    /// construction so callers cannot accidentally smuggle in unknown
    /// flags via this entry point.
    pub fn from_bits(bits: u32) -> Self {
        Self(bits & KNOWN_BITS)
    }

    /// Construct a capability set from the listed flags. Each flag is
    /// OR'd into the bitmask.
    pub fn with_flags<I: IntoIterator<Item = Flag>>(flags: I) -> Self {
        let mut bits = 0u32;
        for f in flags {
            bits |= f.mask();
        }
        Self(bits)
    }

    /// The default capability set advertised by every v0.2 peer for a
    /// transfer with the given `chunk_size`.
    ///
    /// Per RFC-002 §13 Q1 (resolved): the receiver emits
    /// `BytesReceived` progress frames iff `chunk_size <= 1 MiB`, so
    /// the corresponding bit is set conditionally. The receipt bit is
    /// unconditional in v0.2.
    pub fn v0_2_default(chunk_size: u32) -> Self {
        let mut bits = Flag::Receipts.mask();
        if chunk_size <= BYTES_RECEIVED_CHUNK_THRESHOLD {
            bits |= Flag::BytesReceived.mask();
        }
        Self(bits)
    }

    /// Big-endian wire encoding (4 bytes). Reserved bits are forced
    /// to zero before encoding so a buggy caller cannot leak unknown
    /// flags onto the wire.
    pub fn encode(&self) -> [u8; 4] {
        (self.0 & KNOWN_BITS).to_be_bytes()
    }

    /// Inverse of [`Capabilities::encode`]. Reserved bits in the input
    /// are silently discarded.
    pub fn decode(bytes: [u8; 4]) -> Self {
        Self(u32::from_be_bytes(bytes) & KNOWN_BITS)
    }

    /// Raw bitmask, after stripping reserved bits.
    pub fn bits(&self) -> u32 {
        self.0 & KNOWN_BITS
    }

    /// True if [`Flag::Receipts`] is set.
    pub fn supports_receipts(&self) -> bool {
        self.has(Flag::Receipts)
    }

    /// True if [`Flag::BytesReceived`] is set.
    pub fn supports_bytes_received(&self) -> bool {
        self.has(Flag::BytesReceived)
    }

    /// Generic flag accessor.
    pub fn has(&self, flag: Flag) -> bool {
        (self.0 & flag.mask()) != 0
    }

    /// Bitwise AND of two capability sets — the negotiated common
    /// subset. Use this to compute "what we and our peer can both do".
    pub fn intersect(&self, other: Capabilities) -> Capabilities {
        Capabilities((self.0 & other.0) & KNOWN_BITS)
    }
}

impl fmt::Debug for Capabilities {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Capabilities")
            .field("bits", &format_args!("{:#010b}", self.0 & KNOWN_BITS))
            .field("receipts", &self.supports_receipts())
            .field("bytes_received", &self.supports_bytes_received())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_bits_matches_flag_all() {
        // Defence in depth: if a contributor adds a new variant to
        // `Flag` but forgets to widen `KNOWN_BITS`, this test fires.
        let union: u32 = Flag::all()
            .iter()
            .map(|f| f.mask())
            .fold(0u32, |a, b| a | b);
        assert_eq!(
            union, KNOWN_BITS,
            "Flag::all() must enumerate exactly the bits in KNOWN_BITS"
        );
    }

    #[test]
    fn encode_decode_roundtrip_preserves_known_bits() {
        let caps = Capabilities::with_flags([Flag::Receipts, Flag::BytesReceived]);
        let bytes = caps.encode();
        let back = Capabilities::decode(bytes);
        assert_eq!(caps, back);
        assert!(back.supports_receipts());
        assert!(back.supports_bytes_received());
    }

    #[test]
    fn encode_is_big_endian_four_bytes() {
        let caps = Capabilities::with_flags([Flag::Receipts, Flag::BytesReceived]);
        let bytes = caps.encode();
        // bits = 0b11 → big-endian u32 → [0, 0, 0, 0b11]
        assert_eq!(bytes, [0, 0, 0, 0b11]);
    }

    #[test]
    fn flag_accessors_independent() {
        let only_receipts = Capabilities::with_flags([Flag::Receipts]);
        assert!(only_receipts.supports_receipts());
        assert!(!only_receipts.supports_bytes_received());

        let only_bytes = Capabilities::with_flags([Flag::BytesReceived]);
        assert!(!only_bytes.supports_receipts());
        assert!(only_bytes.supports_bytes_received());
    }

    #[test]
    fn reserved_bits_are_ignored_on_decode_and_cleared_on_encode() {
        // High bits + every reserved bit set in the input.
        let dirty: u32 = 0xFFFF_FFFC; // everything except bits 0 and 1
        let caps = Capabilities::decode(dirty.to_be_bytes());
        // No known bit was actually set in `dirty`, so the resulting
        // bitmask is empty.
        assert_eq!(caps.bits(), 0);
        assert!(!caps.supports_receipts());
        assert!(!caps.supports_bytes_received());

        // Now encode it back: reserved bits stripped.
        let bytes = caps.encode();
        assert_eq!(bytes, [0, 0, 0, 0]);

        // And a mixed dirty-and-known input keeps only the known bits.
        let mixed: u32 = 0xFFFF_FFFF; // every bit set
        let mixed_caps = Capabilities::decode(mixed.to_be_bytes());
        assert_eq!(mixed_caps.bits(), KNOWN_BITS);
        assert!(mixed_caps.supports_receipts());
        assert!(mixed_caps.supports_bytes_received());
        // And encoding masks reserved bits off.
        assert_eq!(mixed_caps.encode(), [0, 0, 0, 0b11]);
    }

    #[test]
    fn intersect_is_commutative_and_associative() {
        let a = Capabilities::with_flags([Flag::Receipts, Flag::BytesReceived]);
        let b = Capabilities::with_flags([Flag::Receipts]);
        let c = Capabilities::with_flags([Flag::BytesReceived]);

        assert_eq!(a.intersect(b), b.intersect(a), "commutative");

        let left = a.intersect(b).intersect(c);
        let right = a.intersect(b.intersect(c));
        assert_eq!(left, right, "associative");
    }

    #[test]
    fn intersect_with_empty_yields_empty() {
        let full = Capabilities::with_flags([Flag::Receipts, Flag::BytesReceived]);
        assert_eq!(full.intersect(Capabilities::EMPTY), Capabilities::EMPTY);
        assert_eq!(Capabilities::EMPTY.intersect(full), Capabilities::EMPTY);
    }

    #[test]
    fn v0_2_default_for_small_chunks_sets_both_bits() {
        let caps = Capabilities::v0_2_default(BYTES_RECEIVED_CHUNK_THRESHOLD);
        assert!(caps.supports_receipts());
        assert!(caps.supports_bytes_received());
        assert_eq!(caps.bits(), KNOWN_BITS);
    }

    #[test]
    fn v0_2_default_for_large_chunks_drops_bytes_received() {
        let caps = Capabilities::v0_2_default(BYTES_RECEIVED_CHUNK_THRESHOLD + 1);
        assert!(caps.supports_receipts());
        assert!(!caps.supports_bytes_received());
        assert_eq!(caps.bits(), SUPPORTS_RECEIPTS);
    }

    #[test]
    fn module_constants_match_flag_masks() {
        assert_eq!(SUPPORTS_RECEIPTS, Flag::Receipts.mask());
        assert_eq!(SUPPORTS_BYTES_RECEIVED, Flag::BytesReceived.mask());
        assert_eq!(BYTES_RECEIVED_CHUNK_THRESHOLD, 1024 * 1024);
    }

    #[test]
    fn from_bits_strips_reserved() {
        // Pass in everything; expect only KNOWN_BITS to survive.
        let caps = Capabilities::from_bits(0xFFFF_FFFF);
        assert_eq!(caps.bits(), KNOWN_BITS);
    }

    #[test]
    fn flag_mask_matches_bit_position() {
        assert_eq!(Flag::Receipts.bit(), 0);
        assert_eq!(Flag::Receipts.mask(), 1);
        assert_eq!(Flag::BytesReceived.bit(), 1);
        assert_eq!(Flag::BytesReceived.mask(), 2);
    }
}
