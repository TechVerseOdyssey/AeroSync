pub mod audit;
pub mod auth;
pub mod capabilities;
#[cfg(feature = "mdns")]
pub mod discovery;
pub mod error;
pub mod error_advice;
pub mod file_manager;
pub mod history;
pub mod incoming_file;
pub mod metadata;
pub mod metrics;
pub mod preflight;
pub mod progress;
pub mod receipt;
pub mod receipt_registry;
pub mod receipts_http;
pub mod resume;
pub mod routing;
pub mod server;
pub mod sniff;
pub(crate) mod tls;
pub mod transfer;

pub use audit::{
    AuditEntry, AuditEvent, AuditLogger, AuditRecord, AuditResult, Direction as AuditDirection,
};
pub use auth::{AuthConfig, AuthManager, AuthMiddleware, StoredToken, TokenManager, TokenStore};
pub use capabilities::{
    Capabilities, Flag as CapabilityFlag, BYTES_RECEIVED_CHUNK_THRESHOLD, SUPPORTS_BYTES_RECEIVED,
    SUPPORTS_RECEIPTS,
};
#[cfg(feature = "mdns")]
pub use discovery::{AeroSyncMdns, AeroSyncPeer, MdnsHandle, MDNS_SERVICE_TYPE};
pub use error::{AeroSyncError, Result};
pub use file_manager::{FileInfo, FileManager};
pub use history::{HistoryEntry, HistoryFilter, HistoryQuery, HistoryStore, ReceiptStateLabel};
pub use incoming_file::IncomingFile;
pub use metadata::{
    empty_metadata, label_to_lifecycle, lifecycle_to_label, validate_sealed as validate_metadata,
    MetadataBuilder, MetadataError, MetadataJson, MAX_FILE_NAME_BYTES, MAX_METADATA_BYTES,
    MAX_PARENT_FILE_IDS, MAX_USER_METADATA_ENTRIES, MAX_USER_METADATA_KEY_BYTES,
    MAX_USER_METADATA_VALUE_BYTES, SYSTEM_FIELD_NAMES,
};
pub use metrics::Metrics;
pub use preflight::{preflight_check, probe_receiver, PreflightError, PreflightResult};
pub use progress::{ProgressMonitor, TransferProgress, TransferStats};
pub use receipt::{
    CompletedTerminal, Event as ReceiptEvent, FailedTerminal, Outcome as ReceiptOutcome, Receipt,
    Receiver as ReceiptReceiver, Sender as ReceiptSender, State as ReceiptState,
    StateError as ReceiptStateError,
};
pub use receipt_registry::ReceiptRegistry;
pub use receipts_http::{
    router as receipts_http_router, AckBody, CancelBody, IdempotencyCache, NackBody,
    ReceiptHttpState, StateEventBody, StateView, IDEMPOTENCY_TTL,
};
pub use resume::{ResumeState, ResumeStore, DEFAULT_CHUNK_SIZE};
pub use routing::{Router, RouterConfig, RoutingRule};
pub use server::{FileReceiver, ReceivedFile, ServerConfig, ServerStatus, TlsConfig};
pub use sniff::{sniff_content_type, DEFAULT_CONTENT_TYPE, SNIFF_PEEK_BYTES};
pub use transfer::{TransferConfig, TransferEngine, TransferTask};
