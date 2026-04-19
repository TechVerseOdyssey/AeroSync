//! Metadata envelope (RFC-003) — application-facing builder, validation,
//! and serde adapter on top of the protobuf [`aerosync_proto::Metadata`].
//!
//! # Field classes
//!
//! Per RFC-003 §4.1 the envelope is split into three classes:
//!
//! - **System fields (1-19)** — `id`, `from_node`, `to_node`,
//!   `created_at`, `content_type`, `size_bytes`, `sha256`,
//!   `file_name`, `protocol`. The application **cannot** set these
//!   via [`MetadataBuilder`]. They are populated by
//!   [`crate::core::transfer::TransferEngine`] just before
//!   `TransferStart` is sent.
//!
//! - **Well-known fields (20-99)** — `trace_id`, `conversation_id`,
//!   `parent_file_ids`, `expires_at`, `lifecycle`, `correlation_id`.
//!   Set via the typed builder methods.
//!
//! - **`user_metadata` (99)** — free-form `string → string` map for
//!   application use. Bounded by RFC-003 §6 size limits.
//!
//! # Size limits (enforced by [`MetadataBuilder::build`])
//!
//! | Constraint                          | Limit       |
//! | ----------------------------------- | ----------- |
//! | Total serialized `Metadata`         | 64 KiB      |
//! | `user_metadata` entry count         | 256         |
//! | `user_metadata` key length          | 128 bytes   |
//! | `user_metadata` value length        | 16 KiB      |
//! | `parent_file_ids` count             | 64          |
//! | `file_name` length                  | 1024 bytes  |
//!
//! All `user_metadata` values are **UTF-8 strings only** (RFC-003 §10
//! Q2). Binary payloads must be base64-encoded by the caller. The
//! protobuf `map<string, string>` type already enforces UTF-8 on the
//! wire, but the builder also validates it eagerly so callers see a
//! `MetadataError` rather than a wire-time failure.
//!
//! # System-field shadowing
//!
//! If the caller writes a system field name (e.g. `"sha256"`) into
//! `user_metadata`, the builder **logs a warning** and keeps the
//! entry (RFC-003 §4.1 "we do not strip the user-supplied value to
//! preserve round-trip fidelity"). The system value always wins on
//! read because the typed `Metadata.sha256` field is checked first.

use std::collections::HashMap;

use aerosync_proto::{Lifecycle, Metadata};

/// Maximum total wire-encoded size of the `Metadata` message. RFC-003 §6.
pub const MAX_METADATA_BYTES: usize = 64 * 1024;
/// Maximum number of entries in `user_metadata`. RFC-003 §6.
pub const MAX_USER_METADATA_ENTRIES: usize = 256;
/// Maximum byte length of any `user_metadata` key. RFC-003 §6.
pub const MAX_USER_METADATA_KEY_BYTES: usize = 128;
/// Maximum byte length of any `user_metadata` value. RFC-003 §6.
pub const MAX_USER_METADATA_VALUE_BYTES: usize = 16 * 1024;
/// Maximum number of `parent_file_ids`. RFC-003 §6.
pub const MAX_PARENT_FILE_IDS: usize = 64;
/// Maximum `file_name` length in bytes. RFC-003 §6.
pub const MAX_FILE_NAME_BYTES: usize = 1024;

/// System field names. If any of these appear as a key in
/// `user_metadata`, [`MetadataBuilder::build`] emits a warning. They
/// are not stripped — see module docs.
pub const SYSTEM_FIELD_NAMES: &[&str] = &[
    "id",
    "from_node",
    "to_node",
    "created_at",
    "content_type",
    "size_bytes",
    "sha256",
    "file_name",
    "protocol",
];

/// Validation failure raised by [`MetadataBuilder::build`].
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum MetadataError {
    /// `user_metadata` exceeded [`MAX_USER_METADATA_ENTRIES`].
    #[error("user_metadata has {count} entries, max {max}")]
    TooManyUserEntries { count: usize, max: usize },

    /// A `user_metadata` key exceeded [`MAX_USER_METADATA_KEY_BYTES`].
    #[error("user_metadata key '{key}' is {len} bytes, max {max}")]
    UserKeyTooLong { key: String, len: usize, max: usize },

    /// A `user_metadata` value exceeded [`MAX_USER_METADATA_VALUE_BYTES`].
    #[error("user_metadata['{key}'] value is {len} bytes, max {max}")]
    UserValueTooLong { key: String, len: usize, max: usize },

    /// `parent_file_ids` exceeded [`MAX_PARENT_FILE_IDS`].
    #[error("parent_file_ids has {count} entries, max {max}")]
    TooManyParents { count: usize, max: usize },

    /// `file_name` exceeded [`MAX_FILE_NAME_BYTES`].
    #[error("file_name is {len} bytes, max {max}")]
    FileNameTooLong { len: usize, max: usize },

    /// Total serialized envelope exceeded [`MAX_METADATA_BYTES`].
    #[error("serialized metadata is {actual} bytes, max {max}")]
    OversizeEnvelope { actual: usize, max: usize },
}

/// Application-facing builder for the [`Metadata`] envelope.
///
/// The builder accepts only **well-known** and **`user_metadata`**
/// fields. System fields (`id`, `sha256`, `created_at`, …) are
/// populated by [`crate::core::transfer::TransferEngine`] before
/// [`Metadata`] is sealed onto `TransferStart`.
///
/// ```no_run
/// // v0.3.0+ canonical import path. The legacy
/// // `aerosync::core::metadata::MetadataBuilder` path also resolves
/// // via the root crate's `pub use aerosync_domain::metadata` re-export.
/// use aerosync_domain::metadata::MetadataBuilder;
/// use aerosync_proto::Lifecycle;
///
/// let meta = MetadataBuilder::new()
///     .trace_id("run-123")
///     .lifecycle(Lifecycle::Transient)
///     .user("tenant", "acme")
///     .build()
///     .expect("metadata fits the size limits");
/// # let _ = meta;
/// ```
#[derive(Debug, Default, Clone)]
pub struct MetadataBuilder {
    trace_id: Option<String>,
    conversation_id: Option<String>,
    parent_file_ids: Vec<String>,
    expires_at: Option<chrono::DateTime<chrono::Utc>>,
    lifecycle: Option<Lifecycle>,
    correlation_id: Option<String>,
    content_type_override: Option<String>,
    user_metadata: HashMap<String, String>,
}

impl MetadataBuilder {
    /// Create an empty builder. Equivalent to `Default::default()`.
    pub fn new() -> Self {
        Self::default()
    }

    /// Set the well-known `trace_id`. Last write wins.
    pub fn trace_id(mut self, v: impl Into<String>) -> Self {
        self.trace_id = Some(v.into());
        self
    }

    /// Set the well-known `conversation_id`. Last write wins.
    pub fn conversation_id(mut self, v: impl Into<String>) -> Self {
        self.conversation_id = Some(v.into());
        self
    }

    /// Append a parent file id (lineage). Order is preserved.
    pub fn parent(mut self, file_id: impl Into<String>) -> Self {
        self.parent_file_ids.push(file_id.into());
        self
    }

    /// Set the well-known `lifecycle` hint.
    pub fn lifecycle(mut self, l: Lifecycle) -> Self {
        self.lifecycle = Some(l);
        self
    }

    /// Set the well-known `expires_at` hint. Hint only — not enforced
    /// by AeroSync in v0.2 (RFC-003 §10 Q1).
    pub fn expires_at(mut self, ts: chrono::DateTime<chrono::Utc>) -> Self {
        self.expires_at = Some(ts);
        self
    }

    /// Set the well-known `correlation_id`. Free-form.
    pub fn correlation_id(mut self, v: impl Into<String>) -> Self {
        self.correlation_id = Some(v.into());
        self
    }

    /// Override the sniffed content type. When set, the engine will
    /// **not** run the magic-number sniffer for this transfer and
    /// will use the provided value verbatim.
    pub fn content_type(mut self, ct: impl Into<String>) -> Self {
        self.content_type_override = Some(ct.into());
        self
    }

    /// Insert (or overwrite) a `user_metadata` entry. Last write wins.
    pub fn user(mut self, k: impl Into<String>, v: impl Into<String>) -> Self {
        self.user_metadata.insert(k.into(), v.into());
        self
    }

    /// Bulk-insert multiple `user_metadata` entries. Last write wins
    /// per key, both relative to existing entries and within the
    /// iterator (later keys shadow earlier ones).
    pub fn user_many<I, K, V>(mut self, entries: I) -> Self
    where
        I: IntoIterator<Item = (K, V)>,
        K: Into<String>,
        V: Into<String>,
    {
        for (k, v) in entries {
            self.user_metadata.insert(k.into(), v.into());
        }
        self
    }

    /// Borrow the user-metadata override (if the caller passed
    /// `content_type(...)`). Used by the engine to decide whether
    /// to skip the sniffer.
    pub fn content_type_override(&self) -> Option<&str> {
        self.content_type_override.as_deref()
    }

    /// Validate and produce a [`Metadata`] with **only well-known and
    /// user fields populated**. System fields are left at their
    /// protobuf defaults; the engine fills them in
    /// (`TransferEngine::seal_system_fields`) just before send.
    ///
    /// # Errors
    ///
    /// Returns [`MetadataError`] when any of the RFC-003 §6 size
    /// limits is violated.
    pub fn build(self) -> Result<Metadata, MetadataError> {
        if self.user_metadata.len() > MAX_USER_METADATA_ENTRIES {
            return Err(MetadataError::TooManyUserEntries {
                count: self.user_metadata.len(),
                max: MAX_USER_METADATA_ENTRIES,
            });
        }
        for (k, v) in &self.user_metadata {
            if k.len() > MAX_USER_METADATA_KEY_BYTES {
                return Err(MetadataError::UserKeyTooLong {
                    key: k.clone(),
                    len: k.len(),
                    max: MAX_USER_METADATA_KEY_BYTES,
                });
            }
            if v.len() > MAX_USER_METADATA_VALUE_BYTES {
                return Err(MetadataError::UserValueTooLong {
                    key: k.clone(),
                    len: v.len(),
                    max: MAX_USER_METADATA_VALUE_BYTES,
                });
            }
            if SYSTEM_FIELD_NAMES.contains(&k.as_str()) {
                tracing::warn!(
                    user_key = %k,
                    "user_metadata key shadows a system field; the system value will win on read (RFC-003 §4.1)"
                );
            }
        }
        if self.parent_file_ids.len() > MAX_PARENT_FILE_IDS {
            return Err(MetadataError::TooManyParents {
                count: self.parent_file_ids.len(),
                max: MAX_PARENT_FILE_IDS,
            });
        }

        let meta = Metadata {
            id: String::new(),
            from_node: String::new(),
            to_node: String::new(),
            created_at: None,
            content_type: self.content_type_override.clone().unwrap_or_default(),
            size_bytes: 0,
            sha256: String::new(),
            file_name: String::new(),
            protocol: String::new(),
            trace_id: self.trace_id,
            conversation_id: self.conversation_id,
            parent_file_ids: self.parent_file_ids,
            expires_at: self.expires_at.map(datetime_to_proto_ts),
            lifecycle: self.lifecycle.map(|l| l as i32),
            correlation_id: self.correlation_id,
            user_metadata: self.user_metadata,
        };

        // Wire-size check happens against the *application-shaped*
        // envelope. The engine will later add system fields which
        // will grow the wire size by ~200-400 bytes; we leave that
        // headroom intact by keeping the cap at exactly 64 KiB.
        let serialized = prost::Message::encoded_len(&meta);
        if serialized > MAX_METADATA_BYTES {
            return Err(MetadataError::OversizeEnvelope {
                actual: serialized,
                max: MAX_METADATA_BYTES,
            });
        }

        Ok(meta)
    }
}

/// Validate a fully-populated [`Metadata`] (system fields included)
/// against the RFC-003 §6 wire-size cap and the `file_name` /
/// `parent_file_ids` complexity limits. Used by
/// [`crate::core::transfer::TransferEngine`] after it has stamped
/// the system fields in.
pub fn validate_sealed(meta: &Metadata) -> Result<(), MetadataError> {
    if meta.file_name.len() > MAX_FILE_NAME_BYTES {
        return Err(MetadataError::FileNameTooLong {
            len: meta.file_name.len(),
            max: MAX_FILE_NAME_BYTES,
        });
    }
    if meta.parent_file_ids.len() > MAX_PARENT_FILE_IDS {
        return Err(MetadataError::TooManyParents {
            count: meta.parent_file_ids.len(),
            max: MAX_PARENT_FILE_IDS,
        });
    }
    if meta.user_metadata.len() > MAX_USER_METADATA_ENTRIES {
        return Err(MetadataError::TooManyUserEntries {
            count: meta.user_metadata.len(),
            max: MAX_USER_METADATA_ENTRIES,
        });
    }
    let serialized = prost::Message::encoded_len(meta);
    if serialized > MAX_METADATA_BYTES {
        return Err(MetadataError::OversizeEnvelope {
            actual: serialized,
            max: MAX_METADATA_BYTES,
        });
    }
    Ok(())
}

/// Construct a default empty [`Metadata`] envelope for callers that
/// want only system fields populated. Equivalent to
/// `MetadataBuilder::new().build().unwrap()`.
pub fn empty_metadata() -> Metadata {
    Metadata::default()
}

// ─────────────────────────────────────────────────────────────────────
// Time conversion helpers (chrono ↔ google.protobuf.Timestamp)
// ─────────────────────────────────────────────────────────────────────

/// Convert a `chrono` UTC datetime into a `prost_types::Timestamp`.
/// Lossless within the protobuf range (year 1-9999).
pub fn datetime_to_proto_ts(dt: chrono::DateTime<chrono::Utc>) -> prost_types::Timestamp {
    prost_types::Timestamp {
        seconds: dt.timestamp(),
        nanos: dt.timestamp_subsec_nanos() as i32,
    }
}

/// Convert a `prost_types::Timestamp` into a `chrono` UTC datetime.
/// Returns `None` when the timestamp is out of `chrono`'s range.
pub fn proto_ts_to_datetime(ts: &prost_types::Timestamp) -> Option<chrono::DateTime<chrono::Utc>> {
    chrono::DateTime::<chrono::Utc>::from_timestamp(ts.seconds, ts.nanos.max(0) as u32)
}

// ─────────────────────────────────────────────────────────────────────
// JSON adapter — JSONL HistoryStore persistence (Group B)
// ─────────────────────────────────────────────────────────────────────

/// Mirror of [`Metadata`] with serde derives, used as the on-disk
/// representation in the JSONL [`crate::core::history::HistoryStore`].
/// We hand-roll this rather than `#[derive(Serialize)]` on the
/// generated proto type because `prost` does not emit serde derives
/// and we want a stable JSON shape that survives proto-field
/// reordering in future revisions.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize, Default)]
pub struct MetadataJson {
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub id: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub from_node: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub to_node: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_at: Option<chrono::DateTime<chrono::Utc>>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub content_type: String,
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    pub size_bytes: u64,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub sha256: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub file_name: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub protocol: String,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trace_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conversation_id: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub parent_file_ids: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<chrono::DateTime<chrono::Utc>>,
    /// Stored as the lower-case enum name (`"transient"`,
    /// `"durable"`, `"ephemeral"`) for forward compatibility — adding
    /// a new lifecycle variant later does not invalidate existing
    /// JSONL records.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lifecycle: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub correlation_id: Option<String>,

    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub user_metadata: HashMap<String, String>,
}

#[allow(clippy::trivially_copy_pass_by_ref)]
fn is_zero_u64(v: &u64) -> bool {
    *v == 0
}

impl MetadataJson {
    /// Project a wire [`Metadata`] into its JSONL persistence shape.
    pub fn from_proto(m: &Metadata) -> Self {
        Self {
            id: m.id.clone(),
            from_node: m.from_node.clone(),
            to_node: m.to_node.clone(),
            created_at: m.created_at.as_ref().and_then(proto_ts_to_datetime),
            content_type: m.content_type.clone(),
            size_bytes: m.size_bytes,
            sha256: m.sha256.clone(),
            file_name: m.file_name.clone(),
            protocol: m.protocol.clone(),
            trace_id: m.trace_id.clone(),
            conversation_id: m.conversation_id.clone(),
            parent_file_ids: m.parent_file_ids.clone(),
            expires_at: m.expires_at.as_ref().and_then(proto_ts_to_datetime),
            lifecycle: m.lifecycle.and_then(lifecycle_to_label),
            correlation_id: m.correlation_id.clone(),
            user_metadata: m.user_metadata.clone(),
        }
    }

    /// Hydrate back into a wire [`Metadata`].
    pub fn to_proto(&self) -> Metadata {
        Metadata {
            id: self.id.clone(),
            from_node: self.from_node.clone(),
            to_node: self.to_node.clone(),
            created_at: self.created_at.map(datetime_to_proto_ts),
            content_type: self.content_type.clone(),
            size_bytes: self.size_bytes,
            sha256: self.sha256.clone(),
            file_name: self.file_name.clone(),
            protocol: self.protocol.clone(),
            trace_id: self.trace_id.clone(),
            conversation_id: self.conversation_id.clone(),
            parent_file_ids: self.parent_file_ids.clone(),
            expires_at: self.expires_at.map(datetime_to_proto_ts),
            lifecycle: self
                .lifecycle
                .as_deref()
                .and_then(label_to_lifecycle)
                .map(|l| l as i32),
            correlation_id: self.correlation_id.clone(),
            user_metadata: self.user_metadata.clone(),
        }
    }
}

/// Human-readable lifecycle label, used in CLI flags, JSON
/// persistence, and MCP tool args. Round-trips losslessly with
/// [`label_to_lifecycle`].
pub fn lifecycle_to_label(l: i32) -> Option<String> {
    match Lifecycle::try_from(l).ok()? {
        Lifecycle::Unspecified => None,
        Lifecycle::Transient => Some("transient".to_string()),
        Lifecycle::Durable => Some("durable".to_string()),
        Lifecycle::Ephemeral => Some("ephemeral".to_string()),
    }
}

/// Inverse of [`lifecycle_to_label`]. Case-insensitive on the input.
pub fn label_to_lifecycle(s: &str) -> Option<Lifecycle> {
    match s.to_ascii_lowercase().as_str() {
        "transient" => Some(Lifecycle::Transient),
        "durable" => Some(Lifecycle::Durable),
        "ephemeral" => Some(Lifecycle::Ephemeral),
        _ => None,
    }
}

// ─────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_builder_produces_default_metadata() {
        let m = MetadataBuilder::new().build().expect("build");
        assert!(m.id.is_empty());
        assert!(m.user_metadata.is_empty());
        assert!(m.trace_id.is_none());
        assert!(m.lifecycle.is_none());
    }

    #[test]
    fn well_known_fields_round_trip() {
        let ts = chrono::DateTime::<chrono::Utc>::from_timestamp(1_700_000_000, 0).unwrap();
        let m = MetadataBuilder::new()
            .trace_id("run-7")
            .conversation_id("chat-1")
            .correlation_id("corr-1")
            .lifecycle(Lifecycle::Durable)
            .expires_at(ts)
            .parent("blob-a")
            .parent("blob-b")
            .build()
            .unwrap();
        assert_eq!(m.trace_id.as_deref(), Some("run-7"));
        assert_eq!(m.conversation_id.as_deref(), Some("chat-1"));
        assert_eq!(m.correlation_id.as_deref(), Some("corr-1"));
        assert_eq!(m.lifecycle, Some(Lifecycle::Durable as i32));
        assert_eq!(m.parent_file_ids, vec!["blob-a", "blob-b"]);
        assert_eq!(m.expires_at.unwrap().seconds, 1_700_000_000);
    }

    #[test]
    fn user_entries_last_write_wins() {
        let m = MetadataBuilder::new()
            .user("k", "v1")
            .user("k", "v2")
            .build()
            .unwrap();
        assert_eq!(m.user_metadata["k"], "v2");
    }

    #[test]
    fn user_many_inserts_in_order() {
        let m = MetadataBuilder::new()
            .user_many([("a", "1"), ("b", "2"), ("a", "3")])
            .build()
            .unwrap();
        assert_eq!(m.user_metadata["a"], "3");
        assert_eq!(m.user_metadata["b"], "2");
    }

    #[test]
    fn rejects_too_many_user_entries() {
        let mut b = MetadataBuilder::new();
        for i in 0..(MAX_USER_METADATA_ENTRIES + 1) {
            b = b.user(format!("k{i}"), "v");
        }
        let err = b.build().unwrap_err();
        assert!(matches!(err, MetadataError::TooManyUserEntries { .. }));
    }

    #[test]
    fn rejects_oversize_user_key() {
        let key = "a".repeat(MAX_USER_METADATA_KEY_BYTES + 1);
        let err = MetadataBuilder::new().user(key, "v").build().unwrap_err();
        assert!(matches!(err, MetadataError::UserKeyTooLong { .. }));
    }

    #[test]
    fn rejects_oversize_user_value() {
        let value = "x".repeat(MAX_USER_METADATA_VALUE_BYTES + 1);
        let err = MetadataBuilder::new().user("k", value).build().unwrap_err();
        assert!(matches!(err, MetadataError::UserValueTooLong { .. }));
    }

    #[test]
    fn rejects_too_many_parents() {
        let mut b = MetadataBuilder::new();
        for i in 0..(MAX_PARENT_FILE_IDS + 1) {
            b = b.parent(format!("p{i}"));
        }
        let err = b.build().unwrap_err();
        assert!(matches!(err, MetadataError::TooManyParents { .. }));
    }

    #[test]
    fn rejects_oversize_envelope() {
        // Each entry: ~16 KiB value × 5 entries → ~80 KiB > 64 KiB cap,
        // while staying well under MAX_USER_METADATA_ENTRIES.
        let big = "x".repeat(MAX_USER_METADATA_VALUE_BYTES);
        let mut b = MetadataBuilder::new();
        for i in 0..5 {
            b = b.user(format!("k{i}"), big.clone());
        }
        let err = b.build().unwrap_err();
        assert!(matches!(err, MetadataError::OversizeEnvelope { .. }));
    }

    #[test]
    fn validate_sealed_rejects_long_file_name() {
        let m = Metadata {
            file_name: "x".repeat(MAX_FILE_NAME_BYTES + 1),
            ..Metadata::default()
        };
        let err = validate_sealed(&m).unwrap_err();
        assert!(matches!(err, MetadataError::FileNameTooLong { .. }));
    }

    #[test]
    fn system_field_shadowing_logs_but_succeeds() {
        // "sha256" is a system field; the builder MUST NOT reject it,
        // only emit a warn. We rely on the build() returning Ok.
        let m = MetadataBuilder::new()
            .user("sha256", "deadbeef")
            .build()
            .unwrap();
        assert_eq!(m.user_metadata["sha256"], "deadbeef");
    }

    #[test]
    fn lifecycle_label_round_trip() {
        for lc in [
            Lifecycle::Transient,
            Lifecycle::Durable,
            Lifecycle::Ephemeral,
        ] {
            let label = lifecycle_to_label(lc as i32).unwrap();
            assert_eq!(label_to_lifecycle(&label), Some(lc));
        }
        assert!(lifecycle_to_label(Lifecycle::Unspecified as i32).is_none());
        assert!(label_to_lifecycle("nonsense").is_none());
        assert_eq!(label_to_lifecycle("TRANSIENT"), Some(Lifecycle::Transient));
    }

    #[test]
    fn metadata_json_round_trip_via_serde() {
        let m = MetadataBuilder::new()
            .trace_id("run-1")
            .lifecycle(Lifecycle::Transient)
            .user("k", "v")
            .build()
            .unwrap();
        let mut sealed = m;
        sealed.id = "id-1".into();
        sealed.from_node = "alice".into();
        sealed.to_node = "bob".into();
        sealed.content_type = "text/plain".into();
        sealed.size_bytes = 7;
        sealed.sha256 = "ab".repeat(32);
        sealed.file_name = "f.txt".into();
        sealed.protocol = "quic".into();
        sealed.created_at = Some(prost_types::Timestamp {
            seconds: 1_700_000_000,
            nanos: 500_000_000,
        });

        let json_form = MetadataJson::from_proto(&sealed);
        let s = serde_json::to_string(&json_form).unwrap();
        let back: MetadataJson = serde_json::from_str(&s).unwrap();
        assert_eq!(back, json_form);

        let proto_back = back.to_proto();
        assert_eq!(proto_back.id, sealed.id);
        assert_eq!(proto_back.lifecycle, sealed.lifecycle);
        assert_eq!(proto_back.user_metadata, sealed.user_metadata);
        // created_at survives the chrono round-trip including subsec
        // nanos.
        assert_eq!(proto_back.created_at, sealed.created_at);
    }

    #[test]
    fn metadata_json_skips_empty_fields() {
        // A bare empty MetadataJson serializes as `{}` so JSONL
        // history records for legacy transfers don't bloat with
        // null fields.
        let s = serde_json::to_string(&MetadataJson::default()).unwrap();
        assert_eq!(s, "{}");
    }

    #[test]
    fn metadata_json_backfill_safe_old_records() {
        // Old JSONL records that pre-date Group B do NOT contain a
        // `metadata` key. Any future read into a structure that
        // wraps Option<MetadataJson> with serde(default) MUST yield
        // None. We exercise the parse here against a bare {} so
        // serde wiring above is provably correct.
        #[derive(serde::Deserialize)]
        struct W {
            #[serde(default)]
            metadata: Option<MetadataJson>,
        }
        let w: W = serde_json::from_str("{}").unwrap();
        assert!(w.metadata.is_none());
    }

    #[test]
    fn empty_metadata_helper_returns_default() {
        let m = empty_metadata();
        assert_eq!(m, Metadata::default());
    }

    #[test]
    fn datetime_proto_round_trip_preserves_nanos() {
        let dt =
            chrono::DateTime::<chrono::Utc>::from_timestamp(1_700_000_000, 123_456_789).unwrap();
        let ts = datetime_to_proto_ts(dt);
        assert_eq!(ts.seconds, 1_700_000_000);
        assert_eq!(ts.nanos, 123_456_789);
        assert_eq!(proto_ts_to_datetime(&ts), Some(dt));
    }

    #[test]
    fn content_type_override_is_observable_pre_build() {
        let b = MetadataBuilder::new().content_type("image/png");
        assert_eq!(b.content_type_override(), Some("image/png"));
        let m = b.build().unwrap();
        assert_eq!(m.content_type, "image/png");
    }
}
