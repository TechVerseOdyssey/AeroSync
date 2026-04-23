// `audit` migrated to `aerosync-infra` in v0.3.0 Phase 1d. The
// `pub use` re-export keeps `crate::core::audit::*` and
// `aerosync::core::audit::*` resolving for every existing caller
// (mcp/server.rs, src/main.rs, src/core/transfer.rs, src/core/server.rs)
// without needing import-site updates. The downstream
// `pub use audit::{AuditEntry, AuditEvent, ...}` line below still
// works because `audit` is now in scope as an alias.
pub use aerosync_infra::audit;
pub mod auth;
pub mod capabilities;
#[cfg(feature = "mdns")]
pub mod discovery;
// `error` migrated to `aerosync-domain` in v0.3.0 Phase 1c. The
// `pub use` re-export keeps every existing path
// (`crate::core::error::AeroSyncError`, `crate::core::error::Result`,
// `aerosync::core::error::*`) resolving without forcing any caller
// to update. The downstream `pub use error::{AeroSyncError, Result};`
// line below still works because `error` is now in local scope as an
// alias for `aerosync_domain::error`.
pub use aerosync_domain::error;
pub mod error_advice;
pub mod file_manager;
// `history` migrated to `aerosync-infra` in v0.3.0 Phase 3.4b after
// Phase 3.4a promoted `Receipt` to `aerosync-domain` (which broke
// the `aerosync-infra → aerosync` cycle that blocked the move
// during Phase 2.3). The `pub use` re-export keeps every existing
// path (`crate::core::history::HistoryStore`,
// `aerosync::core::history::HistoryEntry`, etc.) resolving for the
// 8 in-workspace callers (transfer.rs, incoming_file.rs, py
// records, py client, 2 cross-rfc tests, frozen-api docs, metadata
// cross-ref) without forcing any import-site update. The downstream
// `pub use history::{...}` block below still works because
// `history` is now in scope as an alias for
// `aerosync_infra::history`.
pub use aerosync_infra::history;
pub mod incoming_file;
// `metadata` migrated to `aerosync-domain` in v0.3.0 Phase 1e. The
// `pub use` re-export keeps `crate::core::metadata::*` and
// `aerosync::core::metadata::*` resolving for every existing caller
// (transfer.rs, server.rs, history.rs, mcp/server.rs, py bindings)
// without forcing import-site updates. The downstream
// `pub use metadata::{...}` line below still works because
// `metadata` is now in scope as an alias.
pub use aerosync_domain::metadata;
pub mod metrics;
pub mod preflight;
pub mod progress;
// `receipt` migrated to `aerosync-domain` in v0.3.0 Phase 3.4a (the
// move was deferred from Phase 1f to land alongside the Phase 3
// aggregate-root work — see refactor-plan §3 and the
// `aerosync_domain::receipt` module docs). The `pub use` re-export
// keeps every existing path (`crate::core::receipt::Receipt`,
// `aerosync::core::receipt::Sender`, etc.) resolving for the 17
// in-workspace callers (history.rs, transfer.rs, server.rs,
// receipts_http.rs, receipt_registry.rs, incoming_file.rs,
// quic_receipt.rs, mcp/server.rs, py bindings, 5 integration
// tests) without forcing any import-site update. The downstream
// `pub use receipt::{...}` block below still works because
// `receipt` is now in scope as an alias for
// `aerosync_domain::receipt`.
pub use aerosync_domain::receipt;
pub mod receipt_registry;
// `receipt_journal` is a brand-new module in v0.3.0 Phase 2 (RFC-002
// §8 follow-up): a SQLite-backed durable log of every observed
// receipt state transition. Lives in `aerosync-infra` because it
// owns IO; re-exported here so the legacy
// `aerosync::core::receipt_journal::*` import path resolves alongside
// every other infra module.
pub use aerosync_infra::receipt_journal;
pub mod receipts_http;
// `resume` migrated to `aerosync-infra` in v0.3.0 Phase 2.2 with one
// behavioural upgrade — `ResumeStore::save` is now crash-safe via
// tmp+rename. The `pub use` re-export keeps every existing path
// (`crate::core::resume::ResumeState`, `crate::core::ResumeState`,
// `aerosync::core::resume::ResumeStore`, etc.) resolving for every
// caller (transfer.rs, mcp/server.rs, py bindings, CLI) without any
// import-site updates. The downstream `pub use resume::{ResumeState,
// ResumeStore, DEFAULT_CHUNK_SIZE};` line below still works because
// `resume` is now in scope as an alias for `aerosync_infra::resume`.
pub use aerosync_infra::resume;
pub mod routing;
pub mod server;
pub mod sniff;
// `tls` migrated to `aerosync-infra` in v0.3.0 Phase 1b. The
// `pub(crate) use` keeps the canonical in-crate path
// `crate::core::tls::ensure_rustls_provider_installed` working
// without forcing every caller (server.rs, quic_receipt.rs) to
// update its imports. External visibility unchanged — the original
// module was `pub(crate)`.
pub(crate) use aerosync_infra::tls;
pub mod transfer;

pub use aerosync_domain::storage::{
    HistoryStorage, ReceiptJournalRecord, ReceiptJournalStorage, ReceiptSide, RecoverableReceipt,
    ResumeStorage,
};
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
pub use receipt_journal::{spawn_journal_bridge, SqliteReceiptJournal, CURRENT_SCHEMA_VERSION};
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
