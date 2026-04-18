pub mod audit;
pub mod auth;
pub mod capabilities;
pub mod discovery;
pub mod error;
pub mod error_advice;
pub mod file_manager;
pub mod history;
pub mod metrics;
pub mod preflight;
pub mod progress;
pub mod receipt;
pub mod resume;
pub mod routing;
pub mod server;
pub mod transfer;

pub use audit::{
    AuditEntry, AuditEvent, AuditLogger, AuditRecord, AuditResult, Direction as AuditDirection,
};
pub use auth::{AuthConfig, AuthManager, AuthMiddleware, StoredToken, TokenManager, TokenStore};
pub use capabilities::{
    Capabilities, Flag as CapabilityFlag, BYTES_RECEIVED_CHUNK_THRESHOLD, SUPPORTS_BYTES_RECEIVED,
    SUPPORTS_RECEIPTS,
};
pub use discovery::{AeroSyncMdns, AeroSyncPeer, MdnsHandle, MDNS_SERVICE_TYPE};
pub use error::{AeroSyncError, Result};
pub use file_manager::{FileInfo, FileManager};
pub use history::{HistoryEntry, HistoryQuery, HistoryStore};
pub use metrics::Metrics;
pub use preflight::{preflight_check, probe_receiver, PreflightError, PreflightResult};
pub use progress::{ProgressMonitor, TransferProgress, TransferStats};
pub use receipt::{
    CompletedTerminal, Event as ReceiptEvent, FailedTerminal, Outcome as ReceiptOutcome, Receipt,
    Receiver as ReceiptReceiver, Sender as ReceiptSender, State as ReceiptState,
    StateError as ReceiptStateError,
};
pub use resume::{ResumeState, ResumeStore, DEFAULT_CHUNK_SIZE};
pub use routing::{Router, RouterConfig, RoutingRule};
pub use server::{FileReceiver, ReceivedFile, ServerConfig, ServerStatus, TlsConfig};
pub use transfer::{TransferConfig, TransferEngine, TransferTask};
