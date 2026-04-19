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
    ProgressMonitor, Result, ResumeStore, ServerConfig, TlsConfig, TransferConfig, TransferEngine,
    TransferTask,
};
pub use crate::protocols::{AutoAdapter, HttpConfig, HttpTransfer};
#[cfg(feature = "quic")]
pub use crate::protocols::{QuicConfig, QuicTransfer};
