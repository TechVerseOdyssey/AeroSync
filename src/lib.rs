//! AeroSync — fast, agent-friendly file transfer with auto protocol
//! negotiation (HTTP/QUIC), resumable chunked uploads and an MCP server
//! for AI agents.
//!
//! The library is organized in two sub-modules:
//!
//! - [`core`] — transfer engine, mDNS discovery, resume store, auth and
//!   the file receiver server.
//! - [`protocols`] — pluggable transports (HTTP, QUIC, S3, FTP) plus the
//!   `AutoAdapter` that picks the right one for a given task.
//!
//! Most users will only need the high-level types re-exported below.

pub mod core;
pub mod protocols;
pub mod wan;

// Convenience re-exports for the most common types so callers can write
// `use aerosync::TransferEngine` without remembering the sub-module path.
#[cfg(feature = "mdns")]
pub use crate::core::AeroSyncMdns;
pub use crate::core::{
    AeroSyncError, AuditLogger, AuthConfig, AuthManager, FileManager, FileReceiver, HistoryStore,
    ProgressMonitor, Result, ResumeStore, ServerConfig, SessionId, SqliteHistoryStore, TlsConfig,
    TransferConfig, TransferEngine, TransferTask,
};
pub use crate::protocols::{AutoAdapter, HttpConfig, HttpTransfer};
#[cfg(feature = "quic")]
pub use crate::protocols::{QuicConfig, QuicTransfer};
#[cfg(all(feature = "wan-rendezvous", feature = "quic"))]
pub use crate::wan::hole_punch::udp_punch_warmup;
#[cfg(feature = "wan-rendezvous")]
pub use crate::wan::punch_signaling::{
    exchange_candidates_and_wait_punch, rendezvous_signaling_websocket_url, PunchAt,
    RemoteCandidates,
};
#[cfg(feature = "wan-rendezvous")]
pub use crate::wan::rendezvous::{
    parse_peer_at_rendezvous, InitiateSessionResponse, InitiateSessionSignaling, RendezvousClient,
};
