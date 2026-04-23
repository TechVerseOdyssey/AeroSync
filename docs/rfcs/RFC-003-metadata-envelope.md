# RFC-003: Metadata Envelope

| Field          | Value                                            |
| -------------- | ------------------------------------------------ |
| Status         | Draft                                            |
| Author         | TechVerseOdyssey                                 |
| Created        | 2026-04-18                                       |
| Target version | v0.2.0                                           |
| Wire version   | `aerosync-wire/1`                                |
| Estimated work | ~9.5 engineer-days                               |
| Depends on     | RFC-002 (`aerosync/1` ALPN, protobuf wire)       |

## Implementation status (2026-04-22)

This RFC intentionally remains **Draft**, but the metadata envelope is
already implemented as shipped product behavior. The schema, builder,
validation rules, CLI/MCP entry points, and HTTP/QUIC propagation are
live in the codebase, and `docs/v0.3.0-frozen-api.md` +
`docs/protocol/metadata-v1.md` now describe the effective contract more
accurately than some older examples in this RFC.

### Implemented in the codebase

- Protobuf `Metadata` schema and well-known fields.
- Builder/validation logic with size and key/value limits.
- HTTP and QUIC metadata propagation, receiver exposure, and history
  persistence/query support.
- CLI and MCP support for sending metadata and filtering history.

### Drift from the original RFC text

- Some user-facing names evolved since this draft (for example,
  history-query surface naming and some example flows).
- The stable behavior should be read from `docs/v0.3.0-frozen-api.md`
  and `docs/protocol/metadata-v1.md` first.

### Still open or deferred

- SQLite/JSON1 indexing and a richer Python-side typed metadata/history
  surface are still incomplete relative to the original RFC ambition.
- The approval checklist at the end of this RFC has not been updated to
  reflect the shipped state.

## 1. Summary

Every transfer in v0.2 carries a structured **metadata envelope** that
travels alongside the file bytes: a system half (filled in by
AeroSync — id, sender, sha256, timestamps, content-type) and a user
half (free-form key/value provided by the application). Receivers
can route, filter, and correlate on metadata; the envelope is
persisted in receiver history for the file's lifetime.

After this RFC ships, an AI agent can do:

```python
await client.send(
    "report.parquet",
    to="archiver",
    metadata={
        "trace_id":      "run-123",
        "agent_id":      "data-cleaner",
        "parent_files":  "blob-aaa,blob-bbb",
        "lifecycle":     "transient",
    },
)

# On the receiver:
async for f in receiver:
    if f.metadata.get("lifecycle") == "transient":
        archive_to_cold_storage(f.path)
        await f.ack()
```

## 2. Goals

- One envelope schema (protobuf) shared by Rust, Python SDK, MCP, CLI.
- Cleanly separated **system** vs **user** fields; system fields
  cannot be spoofed by application code.
- Automatic content-type sniffing (no mandatory user input).
- Bounded size: hard cap **64 KiB** on the wire; reject larger.
- Persisted in receiver `HistoryStore`; queryable.

## 3. Non-goals

- **Schema validation of `user_metadata` values.** Free-form strings.
  Apps that want JSON should `json.dumps` themselves.
- **Encryption / signing of metadata.** Channel security is mTLS over
  QUIC (existing); per-message signing waits for RFC-005 (Identity).
- **Server-side metadata indexing / search.** Receivers can query
  their own history; we do not build a metadata search service.
- **Metadata mutation after send.** Once on the wire, metadata is
  immutable. Ack-time `metadata` (RFC-002 `Acked.metadata`) is a
  separate piece of data attached to the receipt, not the file.

## 4. Schema

```protobuf
// File: proto/aerosync/wire/v1.proto  (extends RFC-002)
syntax = "proto3";
package aerosync.wire.v1;

import "google/protobuf/timestamp.proto";

message Metadata {
  // ── System fields (set by AeroSync; senders cannot override) ──
  string                       id           = 1;   // UUID v7, == receipt_id
  string                       from_node    = 2;   // sender node identifier
  string                       to_node      = 3;   // receiver name (mDNS / rendezvous)
  google.protobuf.Timestamp    created_at   = 4;
  string                       content_type = 5;   // sniffed MIME (RFC-6838)
  uint64                       size_bytes   = 6;
  string                       sha256       = 7;   // hex lowercase
  string                       file_name    = 8;   // basename, may be safe-renamed by receiver
  string                       protocol     = 9;   // "quic" | "http" | "s3" | "ftp"

  // ── Well-known optional fields (recognized but not enforced) ──
  optional string              trace_id           = 20;
  optional string              conversation_id    = 21;
  repeated string              parent_file_ids    = 22;   // for lineage
  optional google.protobuf.Timestamp expires_at   = 23;   // hint, not enforced in v0.2
  optional Lifecycle           lifecycle          = 24;   // hint
  optional string              correlation_id     = 25;   // free-form correlation

  // ── User free-form fields ──
  // Hard cap: 64 KiB serialized; max 256 entries; key ≤ 128 B; value ≤ 16 KiB.
  map<string, string>          user_metadata      = 99;
}

enum Lifecycle {
  LIFECYCLE_UNSPECIFIED = 0;
  LIFECYCLE_TRANSIENT   = 1;   // can be deleted after ack
  LIFECYCLE_DURABLE     = 2;   // archive
  LIFECYCLE_EPHEMERAL   = 3;   // delete on transfer regardless of ack
}
```

`Metadata` rides inside `TransferStart` (RFC-002 §6.3). Field IDs ≥ 100
are reserved for future system extensions; ≥ 200 reserved for SDK
use; ≥ 1000 forever for user proto extensions.

### 4.1 Field rules

| Class              | Who sets it                | Override by sender?  | Override by receiver? |
| ------------------ | -------------------------- | -------------------- | --------------------- |
| System (1-19)      | AeroSync engine            | ❌                   | ❌                    |
| Well-known (20-99) | App via SDK / explicit API | ✅                   | ❌                    |
| `user_metadata`    | App                        | ✅                   | ❌                    |

If a sender attempts to set a system field via `user_metadata` (e.g.
`user_metadata["sha256"] = "fake"`), it is allowed but ignored on
read — the **system** `sha256` field always wins. We do not strip
the user-supplied value, to preserve round-trip fidelity.

## 5. Content-type sniffing

We use the [`infer`](https://crates.io/crates/infer) crate (≈ 30 KiB,
zero-dep, magic-number table for ~100 formats).

Algorithm:

1. If the application explicitly passes `content_type=`, trust it.
2. Otherwise, read the first 8192 bytes and run `infer::get`.
3. If `infer` returns nothing, fall back to:
   - extension-based guess via `mime_guess` (already a dep through reqwest);
   - then `application/octet-stream`.

Sniffing happens **once on the sender** before the first chunk is
sent. The result is included in `Metadata.content_type` and never
re-sniffed by the receiver.

## 6. Size & complexity limits

These limits are **enforced on the wire** by both endpoints; violators
produce `Failed { code: ERROR_INTERNAL, detail: "metadata oversize" }`.

| Constraint                                   | Limit       |
| -------------------------------------------- | ----------- |
| Total serialized `Metadata` message          | 64 KiB      |
| `user_metadata` entry count                  | 256         |
| `user_metadata` key length                   | 128 bytes   |
| `user_metadata` value length                 | 16 KiB      |
| `parent_file_ids` count                      | 64          |
| `file_name` length                           | 1024 bytes  |

64 KiB is generous for AI workflows (fits a moderate JSON blob) but
prevents abuse where someone tries to ship the actual payload through
metadata.

If a user truly needs more, the answer is: **put it in the file.**

## 7. Persistence

Receiver-side `HistoryStore` gains:

```sql
ALTER TABLE history ADD COLUMN metadata_json TEXT;  -- canonical JSON, lossless re-encoding
CREATE INDEX history_trace_id     ON history(json_extract(metadata_json, '$.trace_id'));
CREATE INDEX history_conv_id      ON history(json_extract(metadata_json, '$.conversation_id'));
```

We store metadata as JSON (not binary protobuf) because:
- SQLite has first-class JSON1 query support.
- It is human-readable when debugging via `sqlite3 ~/.aerosync/state/history.db`.
- Wire format and at-rest format being different is acceptable; both
  are deterministic (canonical key ordering for JSON).

Sender-side: no separate persistence beyond the metadata being
embedded in the transfer record on `ResumeStore` (existing).

## 8. CLI / SDK / MCP surface

### 8.1 CLI

```
aerosync send <file> --to <peer> \
  --meta trace_id=run-123 \
  --meta agent_id=cleaner \
  --content-type application/octet-stream
```

`--meta` is repeatable. `--meta key=value` writes into
`user_metadata`. Well-known fields have dedicated flags
(`--trace-id`, `--conversation-id`, `--lifecycle transient|durable|ephemeral`,
`--parent <file_id>`, `--expires-in 7d`).

### 8.2 Rust API

```rust
// crate aerosync, module core::metadata

pub struct MetadataBuilder { /* … */ }

impl MetadataBuilder {
    pub fn new() -> Self;
    pub fn trace_id(mut self, v: impl Into<String>) -> Self;
    pub fn conversation_id(mut self, v: impl Into<String>) -> Self;
    pub fn parent(mut self, file_id: impl Into<String>) -> Self;
    pub fn lifecycle(mut self, l: Lifecycle) -> Self;
    pub fn expires_at(mut self, t: SystemTime) -> Self;
    pub fn user(mut self, k: impl Into<String>, v: impl Into<String>) -> Self;
    pub fn content_type(mut self, ct: impl Into<String>) -> Self;
    pub fn build(self) -> Metadata;
}

// On TransferEngine::send:
pub async fn send_with_metadata(
    &self,
    path: impl AsRef<Path>,
    to: impl Into<String>,
    metadata: Metadata,
) -> Result<Receipt, AeroSyncError>;
```

### 8.3 Python SDK (RFC-001 §5.2 already includes `metadata=` arg)

Python users pass `metadata={"trace_id": "...", "user.foo": "bar"}`.
The SDK auto-routes well-known keys (`trace_id`, `conversation_id`,
`parent_files`, `lifecycle`, `expires_at`, `correlation_id`) to the
typed fields and the rest into `user_metadata`. Reserved well-known
keys cannot appear in `user_metadata` — passing both raises
`aerosync.ConfigError`.

```python
@dataclass(frozen=True, slots=True)
class FileMetadata:
    id: str
    from_node: str
    to_node: str
    created_at: datetime
    content_type: str
    size_bytes: int
    sha256: str
    file_name: str
    protocol: str

    trace_id: str | None
    conversation_id: str | None
    parent_file_ids: list[str]
    expires_at: datetime | None
    lifecycle: Literal["transient", "durable", "ephemeral"] | None
    correlation_id: str | None

    user_metadata: Mapping[str, str]   # immutable view
```

### 8.4 MCP

Existing tool `send_file` gains an optional `metadata` argument:

```json
{
  "name": "send_file",
  "arguments": {
    "path": "/tmp/report.csv",
    "to":   "archiver",
    "metadata": {
      "trace_id": "run-123",
      "lifecycle": "transient",
      "user_metadata": { "row_count": "12345" }
    }
  }
}
```

New tool `query_history` gains a `metadata_filter` argument:

```json
{
  "name": "query_history",
  "arguments": {
    "metadata_filter": { "trace_id": "run-123" },
    "limit": 50
  }
}
```

Implementation: SQLite JSON1 `json_extract` with parameterized
queries; safe against injection.

## 9. Examples

### 9.1 AI workflow lineage

```python
# Step 1: scrape
res = await client.send(raw, to="cleaner", metadata={
    "trace_id": trace, "agent_id": "scraper", "lifecycle": "transient",
})
# Step 2: cleaner — references upstream file
async for f in cleaner_inbox:
    cleaned = clean(f.path)
    await client.send(cleaned, to="enricher", metadata={
        "trace_id": f.metadata.trace_id,
        "agent_id": "cleaner",
        "parent_files": f.metadata.id,
        "lifecycle": "transient",
    })
    await f.ack()
```

Auditor later queries:

```python
runs = await client.history(metadata_filter={"trace_id": trace})
graph = build_lineage(runs)   # parent_files → DAG
```

### 9.2 Routing in a multi-tenant receiver

```python
async for f in receiver:
    tenant = f.metadata.user_metadata.get("tenant")
    if tenant not in TENANTS:
        await f.nack(reason="unknown tenant")
        continue
    await dispatch_to[tenant](f)
```

## 10. Open questions

| # | Question                                                               | Default                                              |
| - | ---------------------------------------------------------------------- | ---------------------------------------------------- |
| 1 | Should `expires_at` actually delete files, or just be a hint?           | Hint only in v0.2; enforcement deferred to v0.5      |
| 2 | Should we allow binary values in `user_metadata`?                       | No; UTF-8 strings only. Base64 if you must.          |
| 3 | Should the receiver be able to **add** metadata on save (annotations)?  | No; ack-time `Acked.metadata` is the channel for that |
| 4 | Should we provide a `aerosync history search` CLI?                      | Yes, basic — `aerosync history --meta trace_id=…`    |
| 5 | Do we expose `metadata_json` raw via Python?                            | Yes, as `FileMetadata.raw_json: str` for advanced use |

## 11. Implementation tasks

| #  | Task                                                                  | Estimate |
| -- | --------------------------------------------------------------------- | -------- |
| 1  | Add `Metadata` message to `wire/v1.proto`                             | 0.5 d    |
| 2  | `core::metadata` module: builder, validation, size enforcement        | 1 d      |
| 3  | Wire integration: include in `TransferStart`, parse on receive        | 0.5 d    |
| 4  | Content-type sniffing pipeline (`infer` + `mime_guess` fallback)      | 1 d      |
| 5  | `HistoryStore` schema migration + JSON1 indices                       | 1 d      |
| 6  | CLI flags (`--meta`, well-known shortcuts)                            | 0.5 d    |
| 7  | Rust `MetadataBuilder` + `send_with_metadata`                         | 0.5 d    |
| 8  | Python SDK `metadata=` argument + `FileMetadata` dataclass            | 1 d      |
| 9  | MCP `send_file` arg + `query_history` filter                          | 1 d      |
| 10 | Tests: unit (validation, sniffing), integration (lineage demo)        | 1.5 d    |
| 11 | Docs: `docs/protocol/metadata-v1.md` + cookbook recipe                | 1 d      |

**Total: ~9.5 engineer-days.**

## 12. Approval

- [ ] Author on size limits & well-known fields
- [ ] One reviewer on schema (anything missing for AI workflows?)
