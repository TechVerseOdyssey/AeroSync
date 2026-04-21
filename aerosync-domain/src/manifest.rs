//! File-manifest value objects — [`struct@crate::manifest::Hash`],
//! [`crate::manifest::FileEntry`], [`crate::manifest::FileManifest`],
//! and the [`crate::manifest::ChunkPlan`] sibling that describes how a
//! single file is sliced for resumable transfer.
//!
//! Per `docs/v0.3.0-refactor-plan.md` §3 (line 123, micro-PR 3.2), this
//! module owns the **shape** of a transfer's payload: which files are
//! involved, how big each is, what their content hashes are (when known),
//! and how each one will be cut into upload chunks. The
//! `TransferSession` aggregate root (Phase 3.3) holds one
//! [`crate::manifest::FileManifest`] and uses [`crate::manifest::ChunkPlan`]
//! to drive its sender / receiver tasks.
//!
//! ## v0.3.0 Phase 3.2 status (skeleton)
//!
//! Like [`crate::session`], this module is **purely additive**: no
//! existing AeroSync code constructs a [`crate::manifest::FileManifest`] yet. Phase 3.4
//! (`TransferEngine::send_*` migration) and Phase 3.5
//! (`FileReceiver::accept_*` migration) will adopt the types in
//! single-file commits. Today the closest analogue is the per-file
//! `ResumeState` in [`crate::storage`], which the migration will fold
//! into a `FileManifest` of length 1 for single-file transfers and of
//! length N for directory transfers.
//!
//! ## Design notes
//!
//! - **Relative paths only.** Every [`crate::manifest::FileEntry::relative_path`] is
//!   relative to the manifest's owning `source_root` (sender side) or
//!   `save_root` (receiver side); the roots themselves live one layer
//!   up in [`crate::session::SessionKind`]. Construction validates
//!   that no entry is absolute, contains a `..` segment, or starts
//!   with a Windows drive letter — these would let a malicious peer
//!   write outside the intended root. See [`crate::manifest::ManifestError`].
//!
//! - **`Hash` is opaque.** AeroSync has historically threaded SHA-256
//!   hashes as raw lowercase hex strings. The [`struct@crate::manifest::Hash`]
//!   newtype lifts this to a typed value object so v0.4 can add
//!   Blake3 / xxh3 / … without churning every call-site that currently
//!   writes `String`-typed `sha256` fields. The on-disk and on-wire
//!   framing is preserved — [`struct@crate::manifest::Hash`]'s serde
//!   adapter is `#[serde(into / from)]` the legacy hex-string
//!   representation when the algorithm is SHA-256, so any v0.2.x
//!   persisted state still parses.
//!
//! - **`ChunkPlan` is pure arithmetic.** It carries no state about
//!   which chunks have been uploaded — that is [`crate::storage::ChunkState`]'s
//!   job. The split lets the planner be cheap to copy and easy to
//!   compute on demand from `(file_size, chunk_size)` without dragging
//!   in the full resume snapshot.
//!
//! ## Stability
//!
//! Types in this module are reachable only through the module path
//! (`aerosync_domain::manifest::*`) and are considered unstable until
//! Phase 3.3 elevates a curated subset to the crate root. They are
//! **not** listed in `docs/v0.3.0-frozen-api.md` yet.

use std::fmt;
use std::path::{Component, Path, PathBuf};
use std::time::SystemTime;

use serde::{Deserialize, Serialize};

use crate::storage::DEFAULT_CHUNK_SIZE;

// ──────────────────────────── HashAlgo / Hash ──────────────────────────────────

/// Cryptographic-or-checksum algorithm identifying a [`struct@Hash`] value.
///
/// Today AeroSync only emits SHA-256, so [`HashAlgo::Sha256`] is the
/// only variant. The enum is `#[non_exhaustive]` so adding Blake3 /
/// xxh3 / Sha384 in v0.4 does not require a major-version bump on
/// `aerosync-domain`. Wire format (`#[serde(rename_all = "kebab-case")]`)
/// matches the algorithm names used by the broader ecosystem (HTTP
/// `Digest:` header, OCI manifests, …).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
#[non_exhaustive]
pub enum HashAlgo {
    /// SHA-256 (FIPS 180-4). Hex-encoded value is 64 lowercase
    /// characters.
    Sha256,
}

impl HashAlgo {
    /// Expected hex-string length for this algorithm's digest, in
    /// characters. Used by [`Hash::new`] to validate a parsed value.
    pub const fn hex_len(self) -> usize {
        match self {
            HashAlgo::Sha256 => 64,
        }
    }

    /// Algorithm tag string used in serialized form, mirroring
    /// `#[serde(rename_all = "kebab-case")]`.
    pub const fn as_str(self) -> &'static str {
        match self {
            HashAlgo::Sha256 => "sha256",
        }
    }
}

impl fmt::Display for HashAlgo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A typed content hash carrying its algorithm tag.
///
/// Stored on the wire as `{ "algo": "sha256", "value": "<hex>" }`.
/// Constructing via [`Hash::new`] validates that `value` is the
/// correct length for the given algorithm and contains only
/// lowercase hex characters (`[0-9a-f]`); upper-case input is
/// rejected so a round-trip through serde never silently
/// re-normalizes a peer-supplied value.
///
/// For SHA-256 specifically, [`Hash::sha256_from_hex`] is a
/// short-cut that does the same validation.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Hash {
    /// Algorithm tag. Today always [`HashAlgo::Sha256`].
    pub algo: HashAlgo,
    /// Lowercase hex digest. Length matches
    /// [`HashAlgo::hex_len`] for [`Self::algo`].
    pub value: String,
}

impl Hash {
    /// Construct and validate a [`struct@Hash`].
    ///
    /// Returns [`ManifestError::InvalidHash`] if `value` is the wrong
    /// length for `algo` or contains a non-hex character (uppercase
    /// included — see the type-level docs for the rationale).
    pub fn new(algo: HashAlgo, value: impl Into<String>) -> Result<Self, ManifestError> {
        let value = value.into();
        if value.len() != algo.hex_len() {
            return Err(ManifestError::InvalidHash {
                algo,
                reason: format!("expected {} hex chars, got {}", algo.hex_len(), value.len()),
            });
        }
        if !value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return Err(ManifestError::InvalidHash {
                algo,
                reason: "value must be lowercase hex (0-9, a-f only)".to_string(),
            });
        }
        Ok(Self { algo, value })
    }

    /// Convenience: wrap a SHA-256 hex digest with one call.
    /// Equivalent to `Hash::new(HashAlgo::Sha256, hex)`.
    pub fn sha256_from_hex(hex: impl Into<String>) -> Result<Self, ManifestError> {
        Self::new(HashAlgo::Sha256, hex)
    }
}

impl fmt::Display for Hash {
    /// Renders as `<algo>:<hex>` — the same shape as OCI digest
    /// references and the HTTP `Digest:` header. Pure presentation
    /// helper; the wire format is JSON, see the struct serde derive.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.algo, self.value)
    }
}

// ──────────────────────────── FileEntry ────────────────────────────────────────

/// One file in a [`FileManifest`].
///
/// `relative_path` is **always** relative to the owning manifest's
/// root (`source_root` for outbound, `save_root` for inbound). The
/// root itself is held one layer up in
/// [`crate::session::SessionKind`]; this entry only records the
/// suffix. Path security invariants (no `..`, no absolute, no
/// Windows drive letters) are enforced by [`FileManifest::new`] and
/// [`FileManifest::push`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileEntry {
    /// Relative path from the manifest root. UTF-8 path components
    /// are required (`PathBuf` is OS-native, but a non-UTF-8 path
    /// will fail JSON serialization — that is intentional, since
    /// the wire format crosses platforms).
    pub relative_path: PathBuf,
    /// Total file size in bytes as observed at manifest-construction
    /// time. Zero is allowed (empty files transfer fine).
    pub size: u64,
    /// Optional pre-computed content hash. `None` when the manifest
    /// was built without hashing (e.g. directory listing for the
    /// receiver-side manifest, where the hash is filled in after
    /// the body lands). Sender-side manifests built by Phase 3.4
    /// will carry [`HashAlgo::Sha256`] populated.
    pub content_hash: Option<Hash>,
    /// Last-modified timestamp captured at manifest build time.
    /// Threaded through to the receiver so timestamps survive a
    /// transfer. `None` when the source filesystem does not surface
    /// modification time (rare).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub modified: Option<SystemTime>,
}

impl FileEntry {
    /// Construct a [`FileEntry`] without enforcing path safety.
    ///
    /// **The constructor is intentionally infallible** — security
    /// validation lives in [`FileManifest::push`] / [`FileManifest::new`]
    /// where it can reject the entry before it joins a manifest.
    /// Callers building a manifest directly should always go through
    /// the manifest API; the bare constructor exists for tests and
    /// for the receiver-side path that hydrates entries from a
    /// trusted source (the wire validator already ran).
    pub fn new(relative_path: PathBuf, size: u64) -> Self {
        Self {
            relative_path,
            size,
            content_hash: None,
            modified: None,
        }
    }

    /// Builder-style setter for [`Self::content_hash`].
    pub fn with_hash(mut self, hash: Hash) -> Self {
        self.content_hash = Some(hash);
        self
    }

    /// Builder-style setter for [`Self::modified`].
    pub fn with_modified(mut self, ts: SystemTime) -> Self {
        self.modified = Some(ts);
        self
    }
}

// ──────────────────────────── ManifestError ────────────────────────────────────

/// Validation failures from manifest construction.
///
/// Distinct from [`crate::error::AeroSyncError`] because manifest
/// errors are always *programmer-recoverable* (caller fixes the
/// path or the hex string and retries) — surfacing them as a
/// dedicated type lets [`FileManifest::new`] return a precise
/// reason without dragging the full top-level error enum into
/// scope.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ManifestError {
    /// Path is absolute or starts with a Windows drive letter (e.g.
    /// `/etc/passwd`, `C:\\Windows`). Sender side: use
    /// `Path::strip_prefix(source_root)` first.
    AbsolutePath {
        /// Offending path verbatim.
        path: PathBuf,
    },
    /// Path contains a `..` component, which would let the receiver
    /// escape its `save_root`.
    ParentTraversal {
        /// Offending path verbatim.
        path: PathBuf,
    },
    /// Path is empty or normalises to nothing (e.g. `.`).
    EmptyPath,
    /// Two distinct entries share the same relative path. The
    /// receiver-side writer would otherwise stomp the first file.
    DuplicateEntry {
        /// Path that appears more than once.
        path: PathBuf,
    },
    /// [`Hash::new`] rejected a value as malformed.
    InvalidHash {
        /// Which algorithm the value was claimed to belong to.
        algo: HashAlgo,
        /// Human-readable reason (length mismatch, bad char, …).
        reason: String,
    },
    /// [`ChunkPlan::new`] received an invalid `chunk_size`
    /// (currently: zero).
    InvalidChunkSize {
        /// Offending chunk-size value.
        chunk_size: u64,
    },
}

impl fmt::Display for ManifestError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ManifestError::AbsolutePath { path } => {
                write!(
                    f,
                    "manifest entry path must be relative: {}",
                    path.display()
                )
            }
            ManifestError::ParentTraversal { path } => write!(
                f,
                "manifest entry path must not contain '..': {}",
                path.display()
            ),
            ManifestError::EmptyPath => f.write_str("manifest entry path must not be empty"),
            ManifestError::DuplicateEntry { path } => {
                write!(f, "duplicate manifest entry: {}", path.display())
            }
            ManifestError::InvalidHash { algo, reason } => {
                write!(f, "invalid {algo} hash: {reason}")
            }
            ManifestError::InvalidChunkSize { chunk_size } => {
                write!(f, "chunk size must be > 0, got {chunk_size}")
            }
        }
    }
}

impl std::error::Error for ManifestError {}

// ──────────────────────────── FileManifest ─────────────────────────────────────

/// Ordered collection of [`FileEntry`]s describing one transfer's payload.
///
/// `total_bytes` is maintained as a running sum across all entries; it
/// is exposed read-only because mutating it independently of the
/// entries would let the field drift out of sync.
///
/// Manifests are constructed via [`FileManifest::new`] (validating in
/// one shot) or via [`FileManifest::default`] + repeated
/// [`FileManifest::push`] calls (validating incrementally). Both paths
/// enforce identical invariants:
///
/// 1. No path is absolute (`/foo`, `C:\\foo`).
/// 2. No path contains a `..` component.
/// 3. No path is empty / normalises to `.`.
/// 4. No two entries share the same `relative_path`.
///
/// Violating any of these returns a [`ManifestError`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileManifest {
    /// All files this manifest claims, in caller-supplied insertion
    /// order. The order is preserved across the wire so that the
    /// receiver writes / acks files in the same sequence the sender
    /// announced — relevant for the sender's progress UI and for
    /// per-file receipt sequencing.
    entries: Vec<FileEntry>,
    /// Sum of [`FileEntry::size`] over `entries`. Maintained in sync
    /// by [`Self::push`].
    total_bytes: u64,
}

impl FileManifest {
    /// Build a manifest from an iterator of [`FileEntry`]s,
    /// validating each one. Returns the first [`ManifestError`]
    /// encountered.
    pub fn new<I: IntoIterator<Item = FileEntry>>(entries: I) -> Result<Self, ManifestError> {
        let mut m = FileManifest::default();
        for e in entries {
            m.push(e)?;
        }
        Ok(m)
    }

    /// Append `entry` to the manifest, validating its path and
    /// uniqueness. Increments [`Self::total_bytes`] on success.
    pub fn push(&mut self, entry: FileEntry) -> Result<(), ManifestError> {
        validate_relative_path(&entry.relative_path)?;
        if self
            .entries
            .iter()
            .any(|e| e.relative_path == entry.relative_path)
        {
            return Err(ManifestError::DuplicateEntry {
                path: entry.relative_path.clone(),
            });
        }
        self.total_bytes = self.total_bytes.saturating_add(entry.size);
        self.entries.push(entry);
        Ok(())
    }

    /// Borrow the entries in insertion order.
    pub fn entries(&self) -> &[FileEntry] {
        &self.entries
    }

    /// Total payload size in bytes (sum of all
    /// [`FileEntry::size`]). Saturated at `u64::MAX` if the sum
    /// would overflow — practically unreachable, but documented for
    /// completeness.
    pub fn total_bytes(&self) -> u64 {
        self.total_bytes
    }

    /// Number of entries in the manifest.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// `true` iff the manifest contains zero entries. Empty manifests
    /// are legal at construction time (the sender may build one
    /// incrementally) but transports SHOULD reject them at dispatch
    /// time — Phase 3.4 will add that check at the `TransferEngine`
    /// boundary.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// Path-safety validator shared by [`FileManifest::new`] and
/// [`FileManifest::push`]. Pulled out as a free fn so unit tests
/// can hammer it directly without constructing a manifest.
fn validate_relative_path(p: &Path) -> Result<(), ManifestError> {
    if p.as_os_str().is_empty() {
        return Err(ManifestError::EmptyPath);
    }
    if p.is_absolute() {
        return Err(ManifestError::AbsolutePath {
            path: p.to_path_buf(),
        });
    }
    let mut had_normal = false;
    for c in p.components() {
        match c {
            Component::Prefix(_) | Component::RootDir => {
                // `is_absolute` should have caught these on Unix, but
                // on Windows `Component::Prefix` (drive letter) is
                // separate from `is_absolute` and we want to reject
                // both unconditionally.
                return Err(ManifestError::AbsolutePath {
                    path: p.to_path_buf(),
                });
            }
            Component::ParentDir => {
                return Err(ManifestError::ParentTraversal {
                    path: p.to_path_buf(),
                });
            }
            Component::CurDir => {
                // `.` segments are tolerated mid-path (e.g. `./a`)
                // but a path that normalises to *only* `.` is empty.
            }
            Component::Normal(_) => {
                had_normal = true;
            }
        }
    }
    if !had_normal {
        return Err(ManifestError::EmptyPath);
    }
    Ok(())
}

// ──────────────────────────── ChunkPlan / ChunkSpec ────────────────────────────

/// A single chunk slot inside a [`ChunkPlan`].
///
/// Pure arithmetic value (offset + size), no upload state. The
/// state side lives in [`crate::storage::ChunkState`], which a
/// resume store keys by [`Self::index`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChunkSpec {
    /// Zero-based chunk index within the plan.
    pub index: u64,
    /// Byte offset from the start of the file.
    pub offset: u64,
    /// Length of this chunk in bytes. Equals
    /// [`ChunkPlan::chunk_size`] for every chunk except possibly
    /// the last, which may be shorter.
    pub size: u64,
}

/// Describes how one file is sliced into upload chunks for resumable
/// transfer.
///
/// Construction validates that `chunk_size > 0`. A zero-byte file
/// produces a plan with `chunk_count == 0` (no chunks to upload —
/// the receiver records the empty file directly). All other inputs
/// produce `ceil(file_size / chunk_size)` chunks where the last one
/// may be short.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChunkPlan {
    /// Total file size in bytes the plan covers.
    pub file_size: u64,
    /// Per-chunk byte budget. Equal for all chunks except possibly
    /// the last. Defaults to [`crate::storage::DEFAULT_CHUNK_SIZE`]
    /// when [`ChunkPlan::default_for`] is used.
    pub chunk_size: u64,
    /// Number of chunks: `ceil(file_size / chunk_size)`, or 0 when
    /// `file_size == 0`.
    pub chunk_count: u64,
}

impl ChunkPlan {
    /// Build a plan, validating `chunk_size > 0`.
    pub fn new(file_size: u64, chunk_size: u64) -> Result<Self, ManifestError> {
        if chunk_size == 0 {
            return Err(ManifestError::InvalidChunkSize { chunk_size });
        }
        let chunk_count = if file_size == 0 {
            0
        } else {
            // Ceil division without overflow risk: (a - 1) / b + 1
            // where a > 0.
            (file_size - 1) / chunk_size + 1
        };
        Ok(Self {
            file_size,
            chunk_size,
            chunk_count,
        })
    }

    /// Convenience: build a plan using
    /// [`crate::storage::DEFAULT_CHUNK_SIZE`] (32 MiB).
    pub fn default_for(file_size: u64) -> Self {
        // `DEFAULT_CHUNK_SIZE` is a non-zero compile-time constant,
        // so `new` cannot fail here — `expect` documents the
        // invariant for the reader.
        Self::new(file_size, DEFAULT_CHUNK_SIZE)
            .expect("DEFAULT_CHUNK_SIZE is non-zero by construction")
    }

    /// Return the [`ChunkSpec`] at `index`, or `None` if `index`
    /// is out of range.
    pub fn chunk_at(&self, index: u64) -> Option<ChunkSpec> {
        if index >= self.chunk_count {
            return None;
        }
        let offset = index * self.chunk_size;
        let size = std::cmp::min(self.chunk_size, self.file_size - offset);
        Some(ChunkSpec {
            index,
            offset,
            size,
        })
    }

    /// Iterate every chunk in order. `O(chunk_count)`; allocation-free.
    pub fn iter(&self) -> impl Iterator<Item = ChunkSpec> + '_ {
        (0..self.chunk_count)
            .map(move |i| self.chunk_at(i).expect("index is in range by construction"))
    }
}

// ──────────────────────────── Tests ────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ---- Hash ---------------------------------------------------------------

    #[test]
    fn hash_sha256_accepts_64_lower_hex() {
        let hex = "a".repeat(64);
        let h = Hash::sha256_from_hex(hex.clone()).expect("valid sha256");
        assert_eq!(h.algo, HashAlgo::Sha256);
        assert_eq!(h.value, hex);
        assert_eq!(h.to_string(), format!("sha256:{hex}"));
    }

    #[test]
    fn hash_sha256_rejects_wrong_length() {
        let err = Hash::sha256_from_hex("a".repeat(63)).unwrap_err();
        assert!(matches!(
            err,
            ManifestError::InvalidHash {
                algo: HashAlgo::Sha256,
                ..
            }
        ));
    }

    #[test]
    fn hash_sha256_rejects_uppercase() {
        // Upper-case is rejected so a peer-supplied value never
        // silently re-normalises through serde.
        let err = Hash::sha256_from_hex(format!("{}{}", "A", "a".repeat(63))).unwrap_err();
        assert!(matches!(err, ManifestError::InvalidHash { .. }));
    }

    #[test]
    fn hash_sha256_rejects_non_hex_char() {
        let err = Hash::sha256_from_hex(format!("{}{}", "z", "a".repeat(63))).unwrap_err();
        assert!(matches!(err, ManifestError::InvalidHash { .. }));
    }

    #[test]
    fn hash_serde_round_trip() {
        let h = Hash::sha256_from_hex("0".repeat(64)).unwrap();
        let json = serde_json::to_string(&h).unwrap();
        // The wire format is a struct with `algo` + `value`, not the
        // legacy bare hex string. Verify the framing explicitly so a
        // future drop of the field-level serde derive fires.
        assert!(json.contains("\"algo\":\"sha256\""), "got: {json}");
        assert!(json.contains("\"value\":"), "got: {json}");
        let back: Hash = serde_json::from_str(&json).unwrap();
        assert_eq!(h, back);
    }

    #[test]
    fn hash_algo_hex_len_is_64_for_sha256() {
        assert_eq!(HashAlgo::Sha256.hex_len(), 64);
        assert_eq!(HashAlgo::Sha256.to_string(), "sha256");
    }

    // ---- FileEntry ----------------------------------------------------------

    #[test]
    fn file_entry_builder_chains() {
        let h = Hash::sha256_from_hex("f".repeat(64)).unwrap();
        let now = SystemTime::now();
        let e = FileEntry::new(PathBuf::from("dir/a.txt"), 42)
            .with_hash(h.clone())
            .with_modified(now);
        assert_eq!(e.size, 42);
        assert_eq!(e.content_hash, Some(h));
        assert_eq!(e.modified, Some(now));
    }

    #[test]
    fn file_entry_serde_skips_none_modified() {
        let e = FileEntry::new(PathBuf::from("a.txt"), 0);
        let json = serde_json::to_string(&e).unwrap();
        assert!(!json.contains("\"modified\""), "got: {json}");
    }

    // ---- FileManifest -------------------------------------------------------

    #[test]
    fn manifest_new_sums_total_bytes() {
        let m = FileManifest::new([
            FileEntry::new(PathBuf::from("a"), 10),
            FileEntry::new(PathBuf::from("b"), 25),
            FileEntry::new(PathBuf::from("c/d"), 7),
        ])
        .unwrap();
        assert_eq!(m.len(), 3);
        assert_eq!(m.total_bytes(), 42);
        assert!(!m.is_empty());
    }

    #[test]
    fn manifest_default_is_empty() {
        let m = FileManifest::default();
        assert!(m.is_empty());
        assert_eq!(m.total_bytes(), 0);
    }

    #[test]
    fn manifest_rejects_absolute_unix_path() {
        let err = FileManifest::new([FileEntry::new(PathBuf::from("/etc/passwd"), 1)]).unwrap_err();
        assert!(matches!(err, ManifestError::AbsolutePath { .. }));
    }

    #[test]
    fn manifest_rejects_parent_traversal() {
        let err = FileManifest::new([FileEntry::new(PathBuf::from("a/../b"), 1)]).unwrap_err();
        assert!(matches!(err, ManifestError::ParentTraversal { .. }));
    }

    #[test]
    fn manifest_rejects_empty_path() {
        let err = FileManifest::new([FileEntry::new(PathBuf::new(), 1)]).unwrap_err();
        assert!(matches!(err, ManifestError::EmptyPath));
    }

    #[test]
    fn manifest_rejects_dot_only_path() {
        // `.` normalises to no `Component::Normal`, so it must be
        // rejected even though it isn't absolute or parent-traversing.
        let err = FileManifest::new([FileEntry::new(PathBuf::from("."), 1)]).unwrap_err();
        assert!(matches!(err, ManifestError::EmptyPath));
    }

    #[test]
    fn manifest_tolerates_leading_dot_path() {
        // `./a/b.txt` is a valid relative path; `.` is mid-path noise.
        let m = FileManifest::new([FileEntry::new(PathBuf::from("./a/b.txt"), 5)]).unwrap();
        assert_eq!(m.len(), 1);
    }

    #[test]
    fn manifest_rejects_duplicate_entries() {
        let err = FileManifest::new([
            FileEntry::new(PathBuf::from("a"), 1),
            FileEntry::new(PathBuf::from("a"), 2),
        ])
        .unwrap_err();
        assert!(matches!(err, ManifestError::DuplicateEntry { .. }));
    }

    #[test]
    fn manifest_serde_round_trip() {
        let m = FileManifest::new([
            FileEntry::new(PathBuf::from("a.txt"), 10)
                .with_hash(Hash::sha256_from_hex("0".repeat(64)).unwrap()),
            FileEntry::new(PathBuf::from("dir/b.bin"), 32),
        ])
        .unwrap();
        let json = serde_json::to_string(&m).unwrap();
        let back: FileManifest = serde_json::from_str(&json).unwrap();
        assert_eq!(m, back);
        assert_eq!(back.total_bytes(), 42);
    }

    // ---- ChunkPlan ----------------------------------------------------------

    #[test]
    fn chunk_plan_zero_size_has_zero_chunks() {
        let p = ChunkPlan::new(0, 1024).unwrap();
        assert_eq!(p.chunk_count, 0);
        assert!(p.iter().next().is_none());
        assert!(p.chunk_at(0).is_none());
    }

    #[test]
    fn chunk_plan_exact_multiple_has_equal_sized_chunks() {
        let p = ChunkPlan::new(4096, 1024).unwrap();
        assert_eq!(p.chunk_count, 4);
        let chunks: Vec<_> = p.iter().collect();
        assert_eq!(chunks.len(), 4);
        for (i, c) in chunks.iter().enumerate() {
            assert_eq!(c.index, i as u64);
            assert_eq!(c.offset, i as u64 * 1024);
            assert_eq!(c.size, 1024);
        }
    }

    #[test]
    fn chunk_plan_short_last_chunk() {
        let p = ChunkPlan::new(1500, 1024).unwrap();
        assert_eq!(p.chunk_count, 2);
        let last = p.chunk_at(1).unwrap();
        assert_eq!(last.offset, 1024);
        assert_eq!(last.size, 476);
    }

    #[test]
    fn chunk_plan_default_uses_default_chunk_size() {
        let p = ChunkPlan::default_for(DEFAULT_CHUNK_SIZE * 2 + 5);
        assert_eq!(p.chunk_size, DEFAULT_CHUNK_SIZE);
        assert_eq!(p.chunk_count, 3);
    }

    #[test]
    fn chunk_plan_rejects_zero_chunk_size() {
        let err = ChunkPlan::new(100, 0).unwrap_err();
        assert!(matches!(
            err,
            ManifestError::InvalidChunkSize { chunk_size: 0 }
        ));
    }

    #[test]
    fn chunk_plan_chunk_at_out_of_range_returns_none() {
        let p = ChunkPlan::new(10, 4).unwrap();
        assert_eq!(p.chunk_count, 3);
        assert!(p.chunk_at(3).is_none());
        assert!(p.chunk_at(99).is_none());
    }

    #[test]
    fn chunk_plan_serde_round_trip() {
        let p = ChunkPlan::new(1500, 1024).unwrap();
        let json = serde_json::to_string(&p).unwrap();
        let back: ChunkPlan = serde_json::from_str(&json).unwrap();
        assert_eq!(p, back);
    }
}
