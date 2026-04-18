# Metadata Envelope (v1)

> **Status**: implemented in v0.2.0. Wire version `aerosync-wire/1`.
> **Design source**: [RFC-003 Metadata Envelope](../rfcs/RFC-003-metadata-envelope.md).
> **Scope**: this document is the operator-/integrator-facing reference for
> the on-the-wire and on-disk shape of the metadata envelope, the size
> limits we enforce, and the public Rust / CLI / MCP entry points. For the
> design rationale and trade-offs, read RFC-003.

Every transfer in v0.2 carries a structured **metadata envelope** that
flows alongside the file bytes. It has three layers, separated by who is
allowed to set each field:

| Layer       | Field IDs | Set by             | Sender override? | Receiver override? |
| ----------- | --------- | ------------------ | ---------------- | ------------------ |
| System      | 1–19      | AeroSync engine    | no               | no                 |
| Well-known  | 20–99     | App via SDK / CLI  | yes              | no                 |
| `user_metadata` | 99    | App                | yes              | no                 |

System fields are stamped by `TransferEngine::send_with_metadata` right
before the first chunk is sent. Senders that try to spoof a system
field by writing the same key into `user_metadata` (e.g.
`user_metadata["sha256"] = "deadbeef"`) get a `tracing::warn!` from the
builder, but the entry is **kept verbatim** for round-trip fidelity —
the canonical system slot still wins on read.

## 1. Schema

The wire schema is defined in
[`aerosync-proto/proto/aerosync/wire/v1.proto`](../../aerosync-proto/proto/aerosync/wire/v1.proto):

```protobuf
message Metadata {
  // ── System fields (set by AeroSync; senders cannot override) ──
  string                       id           = 1;   // UUID v7, == receipt_id
  string                       from_node    = 2;   // sender node identifier
  string                       to_node      = 3;
  google.protobuf.Timestamp    created_at   = 4;
  string                       content_type = 5;   // sniffed MIME (RFC 6838)
  uint64                       size_bytes   = 6;
  string                       sha256       = 7;   // hex lowercase
  string                       file_name    = 8;
  string                       protocol     = 9;   // "quic" | "http" | "s3" | "ftp"

  // ── Well-known optional fields (recognized but not enforced) ──
  optional string              trace_id           = 20;
  optional string              conversation_id    = 21;
  repeated string              parent_file_ids    = 22;   // lineage
  optional google.protobuf.Timestamp expires_at   = 23;   // hint, not enforced in v0.2
  optional Lifecycle           lifecycle          = 24;   // hint
  optional string              correlation_id     = 25;

  // ── User free-form fields ──
  map<string, string>          user_metadata      = 99;
}

enum Lifecycle {
  LIFECYCLE_UNSPECIFIED = 0;
  LIFECYCLE_TRANSIENT   = 1;   // can be deleted after ack
  LIFECYCLE_DURABLE     = 2;   // archive
  LIFECYCLE_EPHEMERAL   = 3;   // delete on transfer regardless of ack
}
```

`Metadata` rides inside `TransferStart` (RFC-002 §6.3, field 5). Field
IDs ≥ 100 are reserved for future system extensions; ≥ 200 are reserved
for SDK use; ≥ 1000 are reserved forever for user proto extensions.

### 1.1 Field meanings

- `id` — the receipt UUID. Identical to `Receipt::id()` on both ends.
- `from_node` / `to_node` — node identifiers. `from_node` defaults to
  the OS hostname; override with
  `TransferEngine::with_node_id(<id>)`.
- `created_at` — engine wall clock at the moment the envelope is
  sealed (i.e. `send_with_metadata` enters the bridge).
- `content_type` — sniffed MIME (see §3). If the caller passes
  `MetadataBuilder::content_type(...)`, that value wins.
- `size_bytes` / `sha256` / `file_name` — copied from the
  `TransferTask`. `sha256` is **not recomputed** from disk — the caller
  (or chunker) is expected to set `task.sha256` before `send_*`.
- `protocol` — the transport, set by the engine when it picks the
  adapter (`"quic" | "http" | …`).
- `trace_id` / `conversation_id` / `correlation_id` — opaque
  identifiers for distributed-tracing correlation. We do not interpret
  them.
- `parent_file_ids` — receipt UUIDs (or any opaque ids) of upstream
  files this transfer was derived from. Used for lineage queries.
- `expires_at` — hint to the receiver that the file is stale after
  this point. **Not enforced** in v0.2 (open question Q1, accepted).
- `lifecycle` — hint about retention semantics
  (`transient` / `durable` / `ephemeral`).
- `user_metadata` — free-form `string → string` map. UTF-8 only;
  applications that need binary should base64-encode (open question
  Q2, accepted).

## 2. Size limits

Enforced by `MetadataBuilder::build` *before* the envelope reaches the
engine. Violations produce a typed `MetadataError`, never a wire-time
crash.

| Constraint                                   | Limit       | `MetadataError` variant      |
| -------------------------------------------- | ----------- | ---------------------------- |
| Total serialized `Metadata` message          | 64 KiB      | `OversizeEnvelope`           |
| `user_metadata` entry count                  | 256         | `TooManyUserEntries`         |
| `user_metadata` key length                   | 128 bytes   | `UserKeyTooLong`             |
| `user_metadata` value length                 | 16 KiB      | `UserValueTooLong`           |
| `parent_file_ids` count                      | 64          | `TooManyParents`             |
| `file_name` length                           | 1024 bytes  | `FileNameTooLong`            |

Total size is computed via `prost::Message::encoded_len`, so the
limit is exact on the wire. 64 KiB is enough for moderate JSON blobs
(routing hints, agent metadata) but small enough that nobody is
tempted to ship the actual payload through the envelope.

## 3. Content-type sniffing

`src/core/sniff.rs::sniff_content_type` runs on the sender, exactly
once, before the first chunk:

1. If the application passes `MetadataBuilder::content_type(...)`
   (or `--content-type` on the CLI), trust it verbatim.
2. Otherwise, peek the first 8 KiB and run [`infer::get`] on it
   (magic-byte detection for ~100 formats, MIT-licensed, ≈30 KiB of
   tables, zero deps).
3. If `infer` returns nothing, fall back to extension-based guessing
   via [`mime_guess`].
4. Final fallback: `application/octet-stream`.

The receiver does **not** re-sniff. Whatever `Metadata.content_type`
says on the wire is what `IncomingFile::metadata().content_type` will
return.

## 4. Persistence and queries

### 4.1 On-disk format (v0.2.0)

In v0.2.0 the receiver `HistoryStore` is JSONL at
`~/.config/aerosync/history.jsonl`. Each record gains an optional
`metadata` field of shape `MetadataJson` (a hand-written serde
adapter that mirrors the protobuf schema; see
`src/core/metadata.rs::MetadataJson`). Records written by older
versions deserialize cleanly to `metadata: None`.

> **Limitation (deferred to v0.2.1):** there is no SQLite/JSON1 index
> on `metadata` in v0.2.0. `HistoryStore::query` performs a full
> linear scan of the JSONL file; cost is `O(N)` where `N` is the
> number of history records. RFC-003 §11 task #5 (SQLite migration
> with `json_extract` indices on `trace_id` and `conversation_id`)
> is tracked for the v0.2.1 milestone. `rusqlite` / `sqlx` are
> intentionally **not** added to `Cargo.toml` yet.

### 4.2 Query model

`HistoryStore::query(filter: HistoryQuery)` supports:

| Filter field      | Predicate                                                  |
| ----------------- | ---------------------------------------------------------- |
| `metadata_eq`     | `user_metadata` ⊇ `metadata_eq` (AND across pairs)         |
| `trace_id`        | `metadata.trace_id == trace_id`                            |
| `lifecycle`       | `metadata.lifecycle == lifecycle`                          |
| `since`           | `metadata.created_at >= since`                             |
| `until`           | `metadata.created_at <= until`                             |
| `limit`           | cap on returned rows (post-filter)                         |

All predicates AND together. Rows missing a `metadata` field (legacy
records) are skipped silently when any metadata-aware predicate is
set.

## 5. CLI

Send with metadata:

```sh
aerosync send report.parquet \
  --meta tenant=acme \
  --meta agent=cleaner \
  --trace-id run-123 \
  --conversation-id conv-1 \
  --lifecycle transient \
  --parent blob-source-a \
  --parent blob-source-b \
  --content-type application/parquet
```

`--meta key=value` is repeatable and writes into `user_metadata`.
Well-known fields have dedicated flags so they get typed validation
rather than landing in the free-form map.

Query history with metadata filters:

```sh
aerosync history --trace-id run-123
aerosync history --meta tenant=acme --meta agent=cleaner
aerosync history --lifecycle transient --since 2026-04-01T00:00:00Z
```

The output includes `trace_id` and a one-line summary of
`user_metadata` per row.

## 6. MCP

The `send_file` tool gained these optional parameters:

| Parameter           | Type                       | Notes                                  |
| ------------------- | -------------------------- | -------------------------------------- |
| `metadata`          | `map<string, string>`      | Goes to `user_metadata`.               |
| `trace_id`          | `string`                   | Well-known shortcut.                   |
| `conversation_id`   | `string`                   | Well-known shortcut.                   |
| `parent_file_ids`   | `array<string>`            | Lineage.                               |
| `lifecycle`         | `"transient"\|"durable"\|"ephemeral"` | Parsed strictly.            |
| `correlation_id`    | `string`                   | Well-known shortcut.                   |
| `content_type`      | `string`                   | Skips sniffing.                        |

The `list_history` tool gained:

| Parameter           | Type                       | Notes                                  |
| ------------------- | -------------------------- | -------------------------------------- |
| `metadata_filter`   | `map<string, string>`      | AND over `user_metadata`.              |
| `trace_id`          | `string`                   | Filter by well-known field.            |
| `lifecycle`         | `string`                   | Same enum strings as above.            |
| `since` / `until`   | `string` (RFC-3339)        | Filter by `metadata.created_at`.       |

The tool's JSON response now includes a `metadata_json` object per row
(the same `MetadataJson` shape used on disk), so an agent can chain
filters without an extra round-trip.

## 7. Rust API

```rust
use aerosync::core::metadata::{MetadataBuilder, MetadataError};
use aerosync::core::transfer::TransferEngine;
use aerosync_proto::Lifecycle;

let envelope = MetadataBuilder::new()
    .trace_id("run-123")
    .conversation_id("conv-1")
    .lifecycle(Lifecycle::Transient)
    .parent("blob-source-a")
    .user("tenant", "acme")
    .build()?;                                  // -> Metadata or MetadataError

let receipt = engine.send_with_metadata(task, envelope).await?;
let sealed: Metadata = engine
    .metadata_for(receipt.id())
    .await
    .expect("envelope sealed at send time");
```

On the receiver side:

```rust
async for incoming in receiver.incoming().await {
    let m = incoming.metadata();
    if m.lifecycle == Some(Lifecycle::Transient as i32) {
        archive_to_cold_storage(incoming.path()).await?;
        incoming.ack().await?;
    }
}
```

`Metadata` is the prost-generated type re-exported from
`aerosync-proto`. `MetadataJson` (the on-disk shape) is exposed from
`aerosync::core::metadata` for tools that want to read history files
directly.

## 8. Examples (lifted from RFC-003 §9)

### 8.1 AI workflow lineage

```rust
// Step 1: scraper sends raw HTML.
let raw = MetadataBuilder::new()
    .trace_id("run-42")
    .lifecycle(Lifecycle::Transient)
    .user("agent", "scraper")
    .build()?;
let r1 = engine.send_with_metadata(raw_task, raw).await?;

// Step 2: cleaner picks up the file, derives a cleaned version,
// and references the upstream file id for lineage.
let cleaned = MetadataBuilder::new()
    .trace_id("run-42")
    .lifecycle(Lifecycle::Durable)
    .parent(r1.id().to_string())
    .user("agent", "cleaner")
    .build()?;
engine.send_with_metadata(cleaned_task, cleaned).await?;
```

### 8.2 Multi-tenant routing

```rust
let envelope = MetadataBuilder::new()
    .user("tenant", tenant_id)
    .user("priority", "high")
    .build()?;
engine.send_with_metadata(task, envelope).await?;
```

A receiver can route at ack time:

```rust
let m = incoming.metadata();
let tenant = m.user_metadata.get("tenant").map(String::as_str).unwrap_or("default");
route_to_tenant_inbox(tenant, incoming.path()).await?;
```

## 9. Forward compatibility

- New system fields land at IDs 10–19 (free).
- New well-known fields land at IDs 26–98 (free).
- `Metadata` field 99 (`user_metadata`) is the open extension point;
  anything an application wants to ship and the schema doesn't model
  belongs there.
- The wire-version stays `aerosync-wire/1` as long as no field is
  removed or renumbered. v0.2.1's SQLite migration is on-disk only
  and does not bump the wire version.

## 10. See also

- [RFC-003 Metadata Envelope](../rfcs/RFC-003-metadata-envelope.md) — design rationale, open questions, task list.
- [RFC-002 Receipt Protocol](../rfcs/RFC-002-receipt-protocol.md) — `TransferStart` envelope and ack-time `Acked.metadata`.
- `aerosync-proto/proto/aerosync/wire/v1.proto` — the canonical schema.
- `src/core/metadata.rs` — `MetadataBuilder`, `MetadataError`,
  `MetadataJson`.
- `src/core/sniff.rs` — content-type sniffing pipeline.
- `src/core/history.rs` — `HistoryStore::{query, write_metadata}`,
  `HistoryQuery`.

[`infer::get`]: https://docs.rs/infer
[`mime_guess`]: https://docs.rs/mime_guess
