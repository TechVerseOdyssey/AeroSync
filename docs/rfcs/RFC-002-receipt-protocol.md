# RFC-002: Receipt Protocol (bidirectional transfer status)

| Field          | Value                                              |
| -------------- | -------------------------------------------------- |
| Status         | Draft                                              |
| Author         | TechVerseOdyssey                                   |
| Created        | 2026-04-18                                         |
| Target version | v0.2.0                                             |
| Wire version   | `aerosync-wire/1` (first formally specified)       |
| Estimated work | ~20.5 engineer-days                                |
| Depends on     | none (RFC-001 and RFC-003 depend on this)          |

## 1. Summary

Today AeroSync's transfer is a one-way push: the sender knows when it
has flushed all bytes; the receiver knows when it has them. Neither
side knows the **other** side's state. This RFC introduces a
bidirectional **Receipt** channel that carries:

1. Sender → Receiver: protocol version, transfer metadata (RFC-003),
   cancellation requests.
2. Receiver → Sender: byte-receipt confirmation, checksum-verified
   confirmation, application-level **ack/nack**, and structured
   failure reasons.

The result is a small, deterministic state machine on each side,
exposed in the Rust API as `Receipt`, in the Python SDK (RFC-001) as
`await receipt.received() / .processed()`, and in MCP as a new
`wait_for_receipt` tool.

> **Compatibility scope.** This RFC defines the canonical AeroSync
> wire protocol. The pre-existing v0.1.0 binaries (which used an
> ad-hoc serde_json encoding) are treated as a closed pre-alpha and
> are **not** supported as peers. New code may freely break anything
> that v0.1.0 users would have relied on.

## 2. Goals

- A 7-state machine on each side, with **all** transitions either
  observable (`Receipt::watch()`) or terminal.
- Receiver-side application acknowledgement (`ack()` / `nack()`) so
  AI workflows can chain `agent A → agent B → agent C` without
  inotify or polling.
- Crash-safe: pending receipts survive sender restart, pending acks
  survive receiver restart. Idempotent ack via `receipt_id`.
- Forward-extensible: a 4-byte capability bitmask in the handshake
  lets v0.3+ add features without bumping ALPN.
- Cheap: ≤ 200 bytes per state transition on the wire; one extra
  bidirectional QUIC stream per transfer (no extra connection).
- HTTP-compatible: when running over HTTP/1.1 (no streams), use SSE
  for sender-side polling and a `POST /ack` endpoint for the
  receiver side. Same state machine, different wire encoding.

## 3. Non-goals

- **Multi-receiver fan-out.** A receipt represents one sender ↔ one
  receiver. Multi-receiver delivery semantics are deferred to v0.5
  (content-addressed mode).
- **Cross-process ack.** If receiver process A buffered the file and
  process B should ack it, that is a higher-level orchestration
  problem we do not solve here.
- **Persistent receipt across reboot of remote peer.** If the
  receiver crashes after ack but before the sender observes
  `PROCESSED`, the sender re-asks on reconnect; we do **not** queue
  receipts on a third party.
- **Cryptographic non-repudiation of ack.** Signed receipts wait for
  RFC-005 (Identity).

## 4. Sender-side state machine

```
                ┌─────────┐
                │ PENDING │  (send() called, queued)
                └────┬────┘
                     │ start
                     ▼
                ┌─────────┐
            ┌── │ SENDING │ ◄── (chunk progress)
            │   └────┬────┘
            │        │ last chunk flushed
            │        ▼
            │   ┌─────────┐
            │   │  SENT   │  (all bytes on wire, awaiting receiver)
            │   └────┬────┘
            │        │ receiver replies "received"
            │        ▼
            │   ┌─────────┐
            │   │RECEIVED │  (peer has bytes & checksum OK)
            │   └────┬────┘
            │        │ receiver app calls ack()
            │        ▼
            │   ┌──────────┐
            │   │PROCESSED │  ★ terminal, success
            │   └──────────┘
            │
            │   any error / receiver nack:
            │   ┌─────────┐
            └─► │  FAILED │  ★ terminal, with reason
                └─────────┘
            user calls cancel():
                ┌──────────┐
                │CANCELLED │ ★ terminal
                └──────────┘
```

**Allowed transitions** (anything else is a protocol violation):

| From       | To         | Trigger                                               |
| ---------- | ---------- | ----------------------------------------------------- |
| PENDING    | SENDING    | engine starts                                         |
| PENDING    | CANCELLED  | user `cancel()` before start                          |
| PENDING    | FAILED     | resolution / connect failure before start             |
| SENDING    | SENT       | last chunk flushed and acked at QUIC level            |
| SENDING    | FAILED     | connection lost, write error                          |
| SENDING    | CANCELLED  | user `cancel()`                                       |
| SENT       | RECEIVED   | receiver Receipt frame `Received { checksum_ok=true }`|
| SENT       | FAILED     | receiver Receipt frame `Failed { reason }`            |
| SENT       | FAILED     | timeout (default: 60 s, configurable)                 |
| RECEIVED   | PROCESSED  | receiver Receipt frame `Acked`                        |
| RECEIVED   | FAILED     | receiver Receipt frame `Nacked { reason }`            |
| RECEIVED   | FAILED     | timeout (default: configurable, no default)           |

`PROCESSED`, `FAILED`, `CANCELLED` are **terminal**. After terminal,
the receipt object is read-only forever.

## 5. Receiver-side state machine

```
                ┌──────────┐
                │ INCOMING │  (handshake + metadata received)
                └─────┬────┘
                      │ first chunk
                      ▼
                ┌──────────┐
                │RECEIVING │ ◄── (chunk progress)
                └─────┬────┘
                      │ last chunk
                      ▼
                ┌──────────┐
                │VERIFYING │  (sha256 check)
                └─────┬────┘
                      │ ok
                      ▼
                ┌──────────┐
                │ BUFFERED │  (yielded to app code as IncomingFile)
                └─────┬────┘
                      │ app calls ack() / nack() / timeout
                      ▼
            ┌─────────┴─────────┐
            ▼                   ▼
       ┌────────┐          ┌────────┐
       │ ACKED  │          │ NACKED │ ★ terminal
       └────────┘          └────────┘
            ▲ terminal

      any error along the way:
            ┌────────┐
            │ FAILED │ ★ terminal
            └────────┘
```

**Allowed transitions**:

| From        | To         | Trigger                                     |
| ----------- | ---------- | ------------------------------------------- |
| INCOMING    | RECEIVING  | first chunk arrived                         |
| INCOMING    | FAILED     | metadata parse error, auth fail             |
| RECEIVING   | VERIFYING  | last chunk written to disk                  |
| RECEIVING   | FAILED     | connection lost mid-transfer                |
| VERIFYING   | BUFFERED   | sha256 matches sender's claim               |
| VERIFYING   | FAILED     | checksum mismatch                           |
| BUFFERED    | ACKED      | app `ack()`                                 |
| BUFFERED    | NACKED     | app `nack(reason)`                          |
| BUFFERED    | FAILED     | ack timeout (configurable, default ∞)       |

## 6. Wire protocol

### 6.1 Versioning

`aerosync/1` is the only ALPN AeroSync speaks. Connections that fail
to negotiate it are rejected with a clean error. We deliberately do
not maintain compatibility with the pre-alpha v0.1.0 ad-hoc protocol.

For HTTP transports (no ALPN), the protocol version is the first
field of the `Handshake` message and rejected mismatches return HTTP
`400 Bad Request` with body `{"error":"unsupported_protocol_version","supported":["1"]}`.

### 6.2 QUIC stream layout (`aerosync/1`)

```
QUIC connection
├── stream  0  (bidi, control):   ControlFrame*  (handshake, metadata, control)
├── stream  4  (uni, sender→):    ChunkFrame*    (file chunk N)
├── stream  8  (uni, sender→):    ChunkFrame*    (file chunk N+1)   ─┐
├── … each chunk gets its own uni stream …                            │ existing
├── stream 12  (bidi, receipt):   ReceiptFrame*  (state updates)     ◄┴ ★ NEW
```

The control stream (`stream 0`) is bidirectional and carries
handshake, transfer-start, cancel, and heartbeat frames. The receipt
stream (`stream 12`) is opened by the receiver immediately after
handshake and carries all state transitions back to the sender.

### 6.3 Frame encoding: protobuf

All frames use **protobuf v3**. Each frame is length-prefixed (varint)
on its stream.

```protobuf
// File: proto/aerosync/wire/v1.proto
syntax = "proto3";
package aerosync.wire.v1;

message Handshake {
  string protocol_version = 1;     // "1"
  string sender_node_id  = 2;      // future: signed identity
  bytes  auth_proof      = 3;      // HMAC-SHA256 proof of shared token
  uint32 capabilities    = 4;      // bitmask: 0x1=receipts, 0x2=cancel, …
}

message TransferStart {
  string  receipt_id = 1;          // UUID v7, generated by sender
  string  file_name  = 2;
  uint64  size_bytes = 3;
  string  sha256     = 4;
  Metadata metadata  = 5;          // RFC-003
  uint32  chunk_size = 6;
}

// ── Sender → Receiver, on stream 0 ───────────────────────────────
message ControlFrame {
  oneof body {
    Handshake     handshake     = 1;
    TransferStart transfer_start = 2;
    Cancel        cancel        = 3;
    Heartbeat     heartbeat     = 4;
  }
}

message Cancel { string receipt_id = 1; string reason = 2; }
message Heartbeat { uint64 monotonic_ms = 1; }

// ── Receiver → Sender, on stream 12 (receipt stream) ─────────────
message ReceiptFrame {
  string receipt_id = 1;
  oneof body {
    BytesReceived bytes_received = 2;   // periodic progress, optional
    Received      received       = 3;   // checksum verified
    Failed        failed         = 4;
    Acked         acked          = 5;   // app-level ack
    Nacked        nacked         = 6;   // app-level nack
  }
  google.protobuf.Timestamp at = 99;
}

message BytesReceived { uint64 bytes = 1; }
message Received      { bool checksum_ok = 1; string sha256 = 2; }
message Failed        { ErrorCode code = 1; string detail = 2; }
message Acked         { Metadata metadata = 1; }   // optional ack-time metadata
message Nacked        { string reason = 1; }

enum ErrorCode {
  ERROR_UNSPECIFIED      = 0;
  ERROR_AUTH             = 1;
  ERROR_CHECKSUM         = 2;
  ERROR_DISK_FULL        = 3;
  ERROR_PERMISSION       = 4;
  ERROR_TIMEOUT          = 5;
  ERROR_CANCELLED_REMOTE = 6;
  ERROR_INTERNAL         = 99;
}
```

**Why protobuf, not serde_json?**
- Compact (≈ 30-50% smaller per frame).
- Schema evolution via field numbers; we add fields in v0.3+ without
  bumping the wire version.
- Cross-language: Python SDK and future TS/Go SDKs all consume the
  same `.proto`.
- One-time cost: generate Rust types via `prost-build`.

### 6.4 HTTP fallback

When the negotiated transport is HTTP (the non-QUIC AutoAdapter
fallback), we cannot multiplex streams. The receipt protocol degrades
to two HTTP endpoints + Server-Sent Events:

```
POST /v1/transfers
Body: TransferStart (protobuf or json+base64; Content-Type: application/x-protobuf)
→ 201 Created
   Location: /v1/transfers/{receipt_id}

PUT /v1/transfers/{receipt_id}/chunks/{n}
Body: chunk bytes
→ 204

POST /v1/transfers/{receipt_id}/finalize
→ 204     (sender announces it's done)

GET /v1/transfers/{receipt_id}/receipt
Accept: text/event-stream
→ stream of: data: {ReceiptFrame as JSON}\n\n
   until terminal state, then connection closes.

POST /v1/transfers/{receipt_id}/ack       (called by receiver app)
Body: { metadata: {...} }
→ 204

POST /v1/transfers/{receipt_id}/nack
Body: { reason: "..." }
→ 204
```

The receiver runs an SSE producer that flushes a frame every state
transition + every 5 s heartbeat (so dead connections die fast).

### 6.5 Capabilities byte (forward extensibility)

The 4-byte `capabilities` bitmask in `Handshake` lets v0.3+ peers
selectively enable features without bumping ALPN. v0.2 reserves the
following bits as **mandatory** (every v0.2 peer advertises `0x0F`):

| Bit  | Meaning                                  | v0.2  |
| ---- | ---------------------------------------- | :---: |
| 0x01 | Receipt stream / SSE                     | ✅    |
| 0x02 | `Cancel` frame                           | ✅    |
| 0x04 | `Heartbeat` frame                        | ✅    |
| 0x08 | User metadata (RFC-003)                  | ✅    |
| 0x10 | Reserved for v0.3 rendezvous identity    | —     |
| 0x20 | Reserved for v0.4 signed receipts        | —     |

A v0.2 peer that receives `capabilities` not matching `0x0F`
exactly closes the connection with `ERROR_INTERNAL` — there is no
graceful degradation in v0.2. Optional negotiation kicks in starting
v0.3 once we have multiple shipped versions in the wild.

## 7. Timeouts and retries

| Timer                         | Default | Configurable | Purpose                                          |
| ----------------------------- | ------- | ------------ | ------------------------------------------------ |
| sender connect                | 10 s    | yes          | Initial QUIC handshake                           |
| sender chunk inactivity       | 30 s    | yes          | No ack from QUIC for a chunk                     |
| sender SENT → RECEIVED        | 60 s    | yes          | Receiver hasn't replied after `SENT`             |
| sender RECEIVED → PROCESSED   | ∞       | yes          | App-level ack; user must opt-in to a finite wait |
| receiver INCOMING → RECEIVING | 30 s    | yes          | Sender announced but never wrote chunks          |
| receiver BUFFERED → ACKED     | ∞       | yes          | Same — app decides                               |
| receipt-stream heartbeat      | 15 s    | no           | Both sides; missed 3 → tear down                 |
| HTTP SSE keepalive            | 5 s     | no           | Per §6.4                                         |

**Retry policy**: the protocol itself does not retry. If a transfer
ends in `FAILED` due to network error, the **sender SDK** may
automatically restart it using the same `receipt_id` (idempotent),
respecting an exponential backoff from 1 s to 30 s, max 5 attempts.
This logic is policy, not protocol.

## 8. Persistence

### 8.1 Sender side (extend `ResumeStore`)

The existing `ResumeStore` (SQLite) tracks chunk progress for resume.
We extend it with one new table:

```sql
CREATE TABLE receipts (
  receipt_id   TEXT PRIMARY KEY,
  to_peer      TEXT NOT NULL,
  state        TEXT NOT NULL,            -- enum string
  fail_code    TEXT,
  fail_detail  TEXT,
  created_at   INTEGER NOT NULL,
  updated_at   INTEGER NOT NULL,
  expires_at   INTEGER NOT NULL          -- TTL: 7 days default
);
CREATE INDEX receipts_state ON receipts(state);
CREATE INDEX receipts_expires ON receipts(expires_at);
```

On sender startup we **resume** any receipts in non-terminal state by
attempting to reconnect to the peer and asking for the latest
`ReceiptFrame` for each `receipt_id`. (Receiver supports a
`GET /v1/transfers/{id}/receipt?since=now` endpoint and the QUIC
equivalent.)

### 8.2 Receiver side (extend `HistoryStore`)

The existing history table gains:

```sql
ALTER TABLE history ADD COLUMN receipt_id     TEXT;
ALTER TABLE history ADD COLUMN ack_state      TEXT;       -- 'pending'|'acked'|'nacked'|'failed'
ALTER TABLE history ADD COLUMN ack_metadata   TEXT;       -- JSON
ALTER TABLE history ADD COLUMN ack_at         INTEGER;
ALTER TABLE history ADD COLUMN nack_reason    TEXT;
CREATE INDEX history_receipt_id ON history(receipt_id);
```

If the receiver process crashes between `BUFFERED` and `ACKED`, on
restart we surface the un-acked transfer in a recovery iterator the
app can opt into:

```rust
// Rust
async fn recover(&self) -> impl Stream<Item = IncomingFile>;
```

Default policy if the app does not call `recover()` within 5 minutes
of startup: the receiver auto-`nack`s with reason `"orphaned"`.

## 9. Idempotency

`receipt_id` is generated by the sender as **UUID v7** (time-sorted,
monotonic). Both sides use it as the unique key. Specifically:

- Sender resending the same `receipt_id` with the same `sha256` is a
  resume; no duplicate file is created.
- Sender resending the same `receipt_id` with a **different** `sha256`
  is an error and rejected with `ERROR_INTERNAL`.
- Receiver acking the same `receipt_id` twice is a no-op.

## 10. Rust API surface

```rust
// crate aerosync, module core::receipt

#[non_exhaustive]
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReceiptState {
    Pending, Sending, Sent, Received, Processed,
    Failed { code: ErrorCode, detail: String },
    Cancelled,
}

#[derive(Clone)]
pub struct Receipt {
    id: String,
    inner: Arc<ReceiptInner>,
}

impl Receipt {
    pub fn id(&self) -> &str;
    pub fn state(&self) -> ReceiptState;
    pub fn watch(&self) -> tokio::sync::watch::Receiver<ReceiptState>;

    pub async fn sent(&self)      -> Result<(), AeroSyncError>;
    pub async fn received(&self)  -> Result<(), AeroSyncError>;
    pub async fn processed(&self) -> Result<(), AeroSyncError>;

    pub async fn cancel(&self)    -> Result<(), AeroSyncError>;
}

// On the receiver side, IncomingFile gains:
impl IncomingFile {
    pub async fn ack (&self, metadata: HashMap<String, String>) -> Result<(), AeroSyncError>;
    pub async fn nack(&self, reason: impl Into<String>)         -> Result<(), AeroSyncError>;
}
```

`TransferEngine::send` returns `Result<Receipt, AeroSyncError>`.
`Receipt::id()` is the canonical transfer identifier; the legacy
`TaskId` type from v0.1.0 is removed.

## 11. MCP surface

Tools:

| Tool            | Returns                                                       |
| --------------- | ------------------------------------------------------------- |
| `send_file`     | `{ receipt_id, accepted: true }`                              |
| `get_status`    | full receipt incl. `state`, `code`, `detail`                  |

New tools:

| Tool             | Args                                  | Returns                       |
| ---------------- | ------------------------------------- | ----------------------------- |
| `wait_receipt`   | `{ receipt_id, until?, timeout_ms? }` | latest `ReceiptFrame`         |
| `cancel_receipt` | `{ receipt_id, reason? }`             | `{ ok: true }`                |

`until` is one of `"sent" | "received" | "processed"`, default `"received"`.
`timeout_ms` defaults to 30000.

## 12. Observability

Every state transition emits a structured `tracing` event:

```
target: aerosync::receipt
level: INFO
fields:
  receipt_id, peer, from_state, to_state, elapsed_ms,
  bytes_total, error_code (if failed)
```

Per-receipt OpenTelemetry span with `receipt_id` as the trace ID is
deferred to v1.0.

## 13. Open questions

| # | Question                                                                            | Default                                          |
| - | ----------------------------------------------------------------------------------- | ------------------------------------------------ |
| 1 | Should `BytesReceived` frames be opt-in (capabilities bit) or always sent?          | Always sent if `chunk_size <= 1 MiB`             |
| 2 | Should receiver be able to ack from a **different** process than the one that received? | No in v0.2; deferred                          |
| 3 | Default ack timeout                                                                 | ∞ (we make app explicitly opt-in)                |
| 4 | If app never calls `ack()` and process exits, do we auto-`nack`?                    | Yes, on graceful shutdown only; on crash, recovery loop handles |
| 5 | Should `Receipt::watch()` be in the core API or behind a feature flag?              | Core; no flag                                    |

## 14. Implementation tasks

| #  | Task                                                                  | Estimate |
| -- | --------------------------------------------------------------------- | -------- |
| 1  | Add `prost-build` to root crate, generate types from `wire/v1.proto`  | 1 d      |
| 2  | Implement `core::receipt` module (state machine + `Receipt` type)     | 2 d      |
| 3  | Wire ALPN `aerosync/1` + capabilities byte in handshake               | 1 d      |
| 4  | Add bidirectional receipt stream to QUIC transport                    | 2 d      |
| 5  | Implement HTTP SSE fallback + `/ack` `/nack` endpoints (warp)         | 3 d      |
| 6  | Extend `ResumeStore` with `receipts` table + recovery on startup      | 2 d      |
| 7  | Extend `HistoryStore` with ack columns + recovery iterator            | 1 d      |
| 8  | Refactor `TransferEngine::send` to return `Receipt`                   | 1 d      |
| 9  | Add `IncomingFile::ack()` / `nack()`                                  | 1 d      |
| 10 | MCP tool updates: `wait_receipt`, `cancel_receipt`                    | 1 d      |
| 11 | Tracing instrumentation                                               | 0.5 d    |
| 12 | Test suite: state machine unit tests, integration over QUIC + HTTP    | 4 d      |
| 13 | `docs/protocol/wire-v1.md` reference                                  | 1 d      |

**Total: ~20.5 engineer-days.**

## 15. Approval

- [ ] Author on state machine and timeouts
- [ ] One Rust reviewer on QUIC stream layout (§6.2)
- [ ] One Python alpha user on the API mapping (§10 ↔ RFC-001 §5.5)
