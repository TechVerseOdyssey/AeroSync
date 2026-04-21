# MIGRATION — v0.2.x → v0.3.0

> **Phase 4c deliverable** — see `docs/v0.3.0-refactor-plan.md` §3 Phase 4.
> **Status**: Draft, in lockstep with `v0.3.0-rc1` cut.
> **Audience**: Embedders of `aerosync` (Python SDK users, MCP clients, CLI users, Rust crate users) and AeroSync contributors.

## 0. TL;DR

> **External embedders need to do nothing. v0.3.0 is binary-, source-, and wire-compatible with v0.2.1.** Bump the version, rebuild, ship.

The entirety of v0.3.0 is an **internal DDD refactor** that splits the workspace into `aerosync-proto` / `aerosync-domain` / `aerosync-infra` / `aerosync` / `aerosync-mcp`. Every public symbol the v0.2.x SDK exposed remains importable from the same path, every CLI flag keeps the same name and default, every MCP tool keeps the same schema, every byte on the wire is unchanged.

This document explains why no migration steps are required, lists the (purely additive) new internal API surfaces in case downstream Rust users want to opt in, and enumerates what was deferred.

## 1. For external embedders (Python SDK, MCP, CLI users)

### 1.1 Python SDK (`pip install aerosync`)

**Required changes: none.**

- Every name in `aerosync/__init__.py::__all__` resolves identically.
- `Config`, `Client`, `Receiver`, `IncomingFile`, `Receipt`, `Outcome` field names + types are byte-stable per `docs/v0.3.0-frozen-api.md` §3.
- The 9-class exception hierarchy (`AeroSyncError` → `ConfigError` / `ConnectionError` / `AuthError` / `TransferFailed` → `ChecksumMismatch` / `TimeoutError` / `EngineError` / `PeerNotFoundError`) is unchanged.
- `_native.pyi` declarations remain resolvable.

If you are pinning to `aerosync == 0.2.1`, you can move to `0.3.0` (when cut) by changing only the version pin.

### 1.2 MCP clients (Cursor / Claude Desktop / others)

**Required changes: none.**

- All 11 MCP tool names are frozen (`send_file`, `send_directory`, `start_receiver`, `request_file`, `stop_receiver`, `get_receiver_status`, `list_history`, `discover_receivers`, `get_transfer_status`, `wait_receipt`, `cancel_receipt`).
- Every JSON schema field name + type is unchanged.
- Audit log entry shape (`tool_call(name, args_str)`) unchanged.

Existing `mcp.json` / `cursor-mcp.json` configs that reference `aerosync-mcp` continue to work as-is.

### 1.3 CLI users (`cargo install aerosync`)

**Required changes: none.**

- Every command + subcommand name (`send`, `receive`, `token`, `status`, `resume`, `history`, `watch`, `discover`) is frozen.
- Every flag and short form is unchanged.
- Every default value is unchanged (e.g. `--port 7788`, `--quic-port 7789`, `--parallel 4`).
- TOML config schema (`~/.aerosync/config.toml`) keys are frozen; v0.3.0 may add new optional keys with defaults.

### 1.4 Wire format (any non-AeroSync peer speaking ALPN `aerosync/1`)

**Required changes: none.**

`proto/aerosync/wire/v1.proto` is byte-stable through v0.3.0. Every field number is permanent; no field type changed; no enum variant was removed. RFC-002 (`Handshake` / `TransferStart` / `Cancel` / `Heartbeat` / `BytesReceived` / `Received` / `Failed` / `Acked` / `Nacked` / `ReceiptFrame` / `ControlFrame`) and RFC-003 metadata envelope are unchanged in v0.3.0.

HTTP wire additions from Batch B (`X-Aerosync-Metadata` header) and Batch D.5 (`receipt_ack` JSON body field on 200/400) remain unchanged. QUIC stream multiplexing (Batch C: `0x00` control sentinel + length-delimited `ControlFrame` / `ReceiptFrame`) is unchanged.

## 2. For Rust embedders depending on the `aerosync` crate directly

**Required changes: none.**

Every symbol guaranteed by `docs/v0.3.0-frozen-api.md` §1.1–§1.3 still resolves through the root `aerosync` crate via `pub use` re-exports installed in `src/lib.rs` (top-level convenience names) and `src/core/mod.rs` (sub-module bridges).

### 2.1 What moved internally (informational)

| v0.2.x location | v0.3.0 actual home | Re-export bridge |
|---|---|---|
| `aerosync::core::error::{AeroSyncError, Result}` | `aerosync_domain::error::*` | `pub use aerosync_domain::error;` in `src/core/mod.rs` |
| `aerosync::core::metadata::{MetadataBuilder, MetadataJson, validate_sealed, MAX_*, SYSTEM_FIELD_NAMES, …}` | `aerosync_domain::metadata::*` | `pub use aerosync_domain::metadata;` in `src/core/mod.rs` |
| `aerosync::core::resume::{ResumeState, ResumeStore, ChunkState, DEFAULT_CHUNK_SIZE}` | `aerosync_infra::resume::*` (struct + impl); `aerosync_domain::storage::*` (value objects + traits) | `pub use aerosync_infra::resume;` in `src/core/mod.rs`; `aerosync_infra::resume` itself re-exports the value objects from `aerosync_domain::storage` |
| `aerosync::core::audit::{AuditLogger, AuditEntry, AuditEvent, AuditRecord, AuditResult, Direction}` | `aerosync_infra::audit::*` | `pub use aerosync_infra::audit;` in `src/core/mod.rs` |
| `aerosync::core::tls::ensure_rustls_provider_installed` | `aerosync_infra::tls::*` | `pub(crate) use aerosync_infra::tls;` in `src/core/mod.rs` (was `pub(crate)` before, still `pub(crate)`) |

You can keep importing from the v0.2.x paths indefinitely; nothing is deprecated.

### 2.2 Optional: migrate to canonical paths

If you maintain Rust code and prefer the new canonical locations, you may switch to:

```rust
// Old (still works in v0.3.0)
use aerosync::core::error::{AeroSyncError, Result};
use aerosync::core::metadata::MetadataBuilder;
use aerosync::core::resume::{ResumeStore, ResumeState};
use aerosync::core::audit::AuditLogger;

// New canonical (v0.3.0+, optional)
use aerosync_domain::error::{AeroSyncError, Result};
use aerosync_domain::metadata::MetadataBuilder;
use aerosync_domain::storage::ResumeState;          // value object
use aerosync_infra::resume::ResumeStore;            // file-backed impl
use aerosync_infra::audit::AuditLogger;
```

To depend on these crates explicitly, add to your `Cargo.toml`:

```toml
aerosync-domain = "0.3"   # API may break in v0.4.0 — see §3
aerosync-infra  = "0.3"   # API may break in v0.4.0 — see §3
```

**This migration is purely cosmetic.** It does not improve performance, does not unlock new APIs, and does not protect against future breakage — see §3 below.

## 3. What is new and additive

All additions in v0.3.0 are **internal**. They are not part of the frozen public surface and **may break in v0.4.0** without a major version bump.

### 3.1 New crates

| Crate | Purpose | Stability |
|-------|---------|-----------|
| `aerosync-domain` | Pure business logic — error types, metadata, storage trait abstractions | **Internal**. Will be versioned together with `aerosync`. API may break in v0.4.0. |
| `aerosync-infra` | Concrete IO impls — TLS bootstrap, audit JSONL, resume JSON file persistence | **Internal**. Same stability caveat as above. |

Both are `cargo publish`-ready (verified via `cargo publish --dry-run` in Phase 5) so the dependency graph publishes cleanly: `aerosync-proto → aerosync-domain → aerosync-infra → aerosync → aerosync-mcp`.

### 3.2 Storage traits in `aerosync_domain::storage`

```rust
#[async_trait]
pub trait ResumeStorage: Send + Sync + 'static {
    async fn put(&self, snapshot: ResumeState) -> Result<()>;
    async fn get(&self, task_id: Uuid) -> Result<Option<ResumeState>>;
    async fn delete(&self, task_id: Uuid) -> Result<()>;
    async fn list(&self) -> Result<Vec<ResumeState>>;
}

#[async_trait]
pub trait HistoryStorage: Send + Sync + 'static {
    async fn append(&self, entry: HistoryEntry) -> Result<()>;
    async fn query(&self, query: HistoryQuery) -> Result<Vec<HistoryEntry>>;
    async fn recent(&self, limit: usize) -> Result<Vec<HistoryEntry>>;
    async fn delete_older_than(&self, cutoff: DateTime<Utc>) -> Result<usize>;
}
```

Concrete impls land in `aerosync-infra` (Phase 2.2 for `ResumeStore`, Phase 2.3 for `HistoryStore`). Consumer migration to `Arc<dyn …>` injection is deferred to Phase 2.4 / Phase 3.

### 3.3 `ResumeStore::save` is now crash-safe

Commit `2522b51` rewrote `ResumeStore::save` to write to a sibling `.tmp` file and `rename(2)` over the target. Previously a process crash mid-write could leave the resume snapshot truncated.

This is a **strict contract strengthening** — the documented behaviour ("on success the snapshot is durably persisted") was already correct; the implementation now genuinely meets it. No caller-visible change.

## 4. What is deferred (transparent to users)

### 4.1 Landed in v0.3.0-rc1 (no longer deferred)

| Item | Phase | Commit |
|------|-------|--------|
| `TransferEngine` / `AutoAdapter` / `HttpTransfer` switch to `Arc<dyn ResumeStorage>` / `Arc<dyn HistoryStorage>` injection (concrete-typed builders kept for back-compat) | 3.4d | `3da365a` + `b7c024d` |
| `Receipt` state machine extraction into `aerosync_domain::receipt` | 3.4a | landed |
| `history.rs` physical move into `aerosync_infra::history` (Receipt promotion broke the cycle) | 3.4b | landed |
| `SessionId` / `SessionKind` / `SessionStatus` / `FileManifest` / `FileEntry` value objects in `aerosync_domain` | 3.1 + 3.2 | `2d53c0f` etc. |
| `TransferSession` aggregate root + `EventLog` + `ReceiptLedger` / `ReceiptEntry` | 3.3 + 3.4c | `f27fac6` etc. |
| Retire last `#[allow(missing_docs)]` on `aerosync_infra::resume` re-exports + workspace `cargo doc -D warnings` clean | 4d | landed |

### 4.2 Still deferred to v0.4 (will batch with WAN ship)

| Item | Phase | Visibility |
|------|-------|------------|
| `TransferEngine` ↔ `TransferSession` integration (engine actually constructs/owns a session) | 3.4e | Public API: `Receipt` gains `session_id` accessor; `TransferEngine::send` may return both. Deferred to avoid two breaking shape changes. |
| PyO3 `Receipt.session_id` getter exposing the new field | 3.4f | Python SDK additive — gated on 3.4e |
| Protobuf wire field `session_id` on `MetadataEnvelope` | 3.4g | Wire-additive (proto field number reserved); requires v0.4 wire bump-or-additive policy decision |
| `ReceiverSession` wired through `FileReceiver.accept` | 3.5 | Receiver-side equivalent of 3.4e; same batch |
| `config` module split into `aerosync_infra::config` | 4 (later) | Internal — does not affect TOML schema |

None of the v0.4 deferrals affect external embedders today; none break the v0.3.0 frozen contract within the v0.3.x line.

## 5. Build / regenerate steps

### 5.1 For users

**None required.** A normal `cargo update -p aerosync` (Rust) or `pip install --upgrade aerosync` (Python) is sufficient when v0.3.0 ships.

### 5.2 For contributors

Each new crate now builds standalone:

```bash
cargo build -p aerosync-proto       # already worked in v0.2.x
cargo build -p aerosync-domain      # new in v0.3.0
cargo build -p aerosync-infra       # new in v0.3.0
cargo build -p aerosync             # root crate — unchanged
cargo build -p aerosync-mcp         # unchanged
cargo build --workspace             # full workspace — unchanged
```

The `aerosync-domain` standalone build requires the workspace to declare `uuid = { features = ["v4", "serde"] }` (the `serde` feature was previously implicit via the root crate's transitive dep graph). This is already wired in the workspace Cargo.toml.

CI commands from `docs/v0.3.0-frozen-api.md` §10 remain authoritative for verifying the frozen surface:

```bash
cargo build --workspace
cargo build --workspace --no-default-features --features http
cargo build --workspace --no-default-features --features quic
cargo build --workspace --all-features
cargo test  --workspace 2>&1 | rg "test result"
cargo clippy --workspace --all-targets -- -D warnings
( cd aerosync-py && uv run pytest -x -v && uv run mypy --strict python/aerosync )
```

## 6. Pinned versions

| Crate / package | v0.2.x baseline | v0.3.0 (during refactor) | v0.3.0-rc1 (Phase 5) |
|-----------------|------------------|--------------------------|----------------------|
| `aerosync` | `0.2.1` | `0.2.1` (workspace.package.version unchanged through Phases 1-4) | `0.3.0-rc1` |
| `aerosync-proto` | `0.2.1` | `0.2.1` | `0.3.0-rc1` |
| `aerosync-domain` | — | `0.2.1` (new, follows workspace) | `0.3.0-rc1` |
| `aerosync-infra` | — | `0.2.1` (new, follows workspace) | `0.3.0-rc1` |
| `aerosync-mcp` | `0.2.1` | `0.2.1` | `0.3.0-rc1` |
| `aerosync-py` (PyPI) | `0.2.1` | `0.2.1` | `0.3.0rc1` |

Version bumps land in Phase 5 (`docs/v0.3.0-refactor-plan.md` §3 Phase 5).

## 7. Where to read more

- **Why a refactor at all**: `docs/v0.3.0-refactor-plan.md` §1
- **What is in / out of scope**: `docs/v0.3.0-refactor-plan.md` §2
- **Frozen public API**: `docs/v0.3.0-frozen-api.md` (full document — single source of truth for "what must not break")
- **Allowed refactor moves**: `docs/v0.3.0-frozen-api.md` §11
- **Architecture overview** (post-refactor): `docs/ARCHITECTURE_AND_DESIGN.md` §2-§4
- **Wire format**: `proto/aerosync/wire/v1.proto`, RFC-002 (`docs/rfcs/RFC-002-wire-format.md`), RFC-003 (`docs/rfcs/RFC-003-metadata-envelope.md`)
