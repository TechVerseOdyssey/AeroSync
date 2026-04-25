//! NAT hole **warm-up** (UDP) before a QUIC handshake on the same 5-tuple
//! (RFC-004 §6.2–6.3). Reuse the same [`std::net::UdpSocket`] for this and for
//! [`quinn::Endpoint::new`](https://docs.rs/quinn) so the mapping matches.
//!
//! Full `Endpoint::new` + transfer wiring is not yet in `AutoAdapter`; this module
//! exposes the low-level primitive only.

use std::io;
use std::net::{SocketAddr, UdpSocket};

/// Send a few tiny datagrams to each `peer` address to open or refresh pinholes
/// for the local (`socket`’s) NAT mapping.
pub fn udp_punch_warmup(
    socket: &UdpSocket,
    peer_addrs: &[SocketAddr],
    dgrams_per_addr: u8,
) -> io::Result<()> {
    // One byte — some stacks reject 0-len sends.
    let buf = [0u8; 1];
    for a in peer_addrs {
        for _ in 0..dgrams_per_addr {
            let _n = socket.send_to(&buf, *a)?;
        }
    }
    Ok(())
}
