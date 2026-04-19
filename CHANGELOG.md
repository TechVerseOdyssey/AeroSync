# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

> v0.2.1 development. Targets quality-of-life fixes around the v0.2.0
> Python SDK + a leaner default Cargo build. No wire-format changes.

### Added

- **`FileReceiver::local_http_addr()` / `local_quic_addr()`** — public
  Rust accessors that return the OS-assigned `SocketAddr` once
  `start()` has bound the listener. `None` before start and after
  `stop()`. Lets callers who pass `http_port = 0` (or
  `listen="127.0.0.1:0"` from Python) discover the real port instead
  of grepping tracing output. (P0.3)
- **Per-protocol Cargo features** on the `aerosync` crate — embedders
  can now opt out of the heavy protocol stacks they don't need:
  - `http` *(default)* — HTTP transport
  - `quic` *(default)* — QUIC transport (`quinn` + receipt frames)
  - `s3`   *(default)* — S3 / MinIO destination support
  - `ftp`  *(default)* — FTP / FTPS destination support (`suppaftp`)
  - `mdns` *(default)* — LAN service discovery (`mdns-sd`)
  - `mcp-helpers` *(default)* — receipt-registry hooks for
    `aerosync-mcp` and the Python SDK
  - `wan-rendezvous`, `wan-relay` — placeholder feature names so
    downstream `Cargo.toml` files can begin referencing the v0.3.0
    WAN surface (see `docs/rfcs/RFC-004-wan-rendezvous.md`); compile
    to empty modules today.

  The default feature set preserves full functionality for the
  shipped `aerosync` CLI and the `aerosync-mcp` server. The
  `aerosync` binary declares
  `required-features = ["http", "quic", "mdns", "mcp-helpers"]`,
  so library-only embedders can build with
  `cargo build -p aerosync --no-default-features --features http,quic`
  without dragging in the binary or its mDNS / MCP scaffolding.
  (P2.2; future v0.3.0 RFC-004)

### Changed

- **`Receiver.address` (Python SDK)** — now returns the actual
  bound `host:port` after `__aenter__` resolves, falling back to the
  user-supplied `listen=` string only before the receiver has bound.
  Unblocks the README quickstart pattern where a sender needs to
  discover the receiver's port after binding to `127.0.0.1:0`.
  (P0.3, RFC-001 §5.3 follow-up)
- **Rustls crypto provider installer** moved from
  `protocols::quic::ensure_crypto_provider_installed` to a new
  `core::tls::ensure_rustls_provider_installed` so the HTTPS
  receiver keeps working when the `quic` feature is disabled.
  Both call sites in `core::server` and the QUIC integration tests
  in `protocols::quic_receipt` were updated. No behaviour change
  for default builds. (P2.2)


## [v0.2.0] - 2026-04-18

> v0.2.0 turns AeroSync from a single-language CLI into a multi-tenant
> file bus for AI agents: a first-class **Python SDK** on PyPI, a
> **bidirectional Receipt protocol** (sender knows when the receiver
> actually processed), and a **Metadata envelope** that carries
> structured context with every transfer. The wire moves from
> ad-hoc serde_json to formally specified protobuf
> (`aerosync_proto::wire::v1`); ALPN bumps from `aerosync` to
> `aerosync/1`.
>
> Design references — frozen for this release:
> [RFC-001](docs/rfcs/RFC-001-python-sdk.md) · [RFC-002](docs/rfcs/RFC-002-receipt-protocol.md) · [RFC-003](docs/rfcs/RFC-003-metadata-envelope.md).

### Added

#### Python SDK (`pip install aerosync`) — RFC-001

- **Public surface**: `aerosync.client()` / `aerosync.receiver()` /
  `aerosync.discover()` / `aerosync.version()` (see
  `aerosync-py/python/aerosync/__init__.py` for the full re-export
  list).
- `Client.send(source, *, to, metadata, chunk_size, timeout, on_progress)`
  — `source` accepts `str | os.PathLike | bytes | BinaryIO`. Returns
  an awaitable `Receipt`.
- `Client.send_directory(source, *, to, metadata)` for recursive
  uploads.
- `Client.history(*, limit, direction, metadata_filter, history_path)`
  — query persisted transfer history with metadata filter.
- `Receiver` async context manager + async iterator yielding
  `IncomingFile` instances; `IncomingFile.ack(metadata?)` /
  `IncomingFile.nack(reason)` drive the receiver-side terminal.
- `Receipt`: `id`, `state`, `watch()`, `sent()` / `received()` /
  `processed()`, `cancel(reason)` — the full RFC-002 §6 surface
  exposed in idiomatic async Python.
- `aerosync.discover(*, timeout)` — mDNS peer discovery returning
  `list[Peer]`.
- `Config` dataclass + `Config.from_dict` / `from_toml` /
  `from_default` (loads `~/.aerosync/config.toml` by default; applies
  `auth_token` zeroized, `state_dir`, `log_level`,
  `chunk_size_default`, `timeout_default` to the underlying engine).
  `tomli` is pulled only on Python <3.11.
- Typed exception hierarchy rooted at `AeroSyncError`:
  `ConfigError`, `PeerNotFoundError`, `ConnectionError`, `AuthError`,
  `TransferFailed` (+ `ChecksumMismatch` subclass), `TimeoutError`,
  `EngineError`. Every instance carries `.code` (snake_case) and
  `.detail` (original Rust message) for structured logging.
- PEP 561 `py.typed` marker + hand-maintained `_native.pyi` stubs.
  `mypy --strict aerosync-py/python/aerosync` is green.
- 90 pytest tests + 3 documented skips. The killer-demo test
  (`aerosync-py/tests/test_killer_demo.py`) is shipped-but-skipped
  pending `w3c-quic-receipt-wiring` and flips to passing
  automatically when that lands.

#### Receipt Protocol — RFC-002

- 7-state state machine: `Initiated → StreamOpened → DataTransferred
  → StreamClosed → Processing → Completed → Failed`. Implemented in
  `src/core/receipt.rs` with parameterized `Sender` / `Receiver`
  side types and an exhaustive `apply_event` transition table.
- Bidirectional QUIC stream (stream id 12) with protobuf framing:
  `src/protocols/quic_receipt.rs` ships the codec
  (`ReceiptCodec`, `run_sender_loop`, `run_receiver_loop`,
  `ReceiverVerdict`).
- HTTP SSE control plane (`src/core/receipts_http.rs`):
  - `GET /v1/receipts/:id/events` — Server-Sent Events stream of
    receipt-state transitions, terminating on the first
    `terminal=true` frame.
  - `POST /v1/receipts/:id/{ack,nack,cancel}` — idempotent (via
    `Idempotency-Key` header backed by `IdempotencyCache`).
- In-process `ReceiptRegistry<Side>` (`src/core/receipt_registry.rs`)
  — id-keyed lookup; auto-prunes on terminal.
- Capabilities negotiation: `aerosync_proto::HandshakeFrame`
  carries a 4-byte capability bitmask; `SUPPORTS_RECEIPTS` is the
  first defined bit. Senders that omit it get a legacy receiver
  surface (`IncomingFile::new_without_receipt`) where ack/nack are
  silent no-ops.
- `IncomingFile` receiver-side wrapper with
  `ack(metadata?)` / `nack(reason)` / `into_receipt()` / `metadata()`
  getter / `with_metadata()` builder.
- `TransferEngine::send` now returns
  `Arc<Receipt<Sender>>` (was `TransferOutcome`); the engine spawns
  a bridge task that mirrors `ProgressMonitor` terminal status into
  the receipt state machine.
- MCP tools: `wait_receipt` and `cancel_receipt` accept a
  `receipt_id` and act on the engine's `ReceiptRegistry`.
- Tracing instrumentation: receipt id and side are spanned across
  the engine, transport, registry, and HTTP control plane.
- 4 dedicated end-to-end tests (`tests/receipts_e2e.rs`) cover the
  QUIC ack happy path, QUIC nack-with-reason, HTTP SSE mirror, and
  the MCP cancel round-trip.

#### Metadata Envelope — RFC-003

- Single protobuf schema (`aerosync_proto::Metadata`) shared by
  Rust, Python SDK, MCP and CLI. See
  [`docs/protocol/metadata-v1.md`](docs/protocol/metadata-v1.md) for
  the wire reference.
- **System fields** (sealed by the engine, not spoofable from
  user code): `id` (= receipt id), `from_node`, `to_node`,
  `created_at`, `content_type` (sniffed via `infer` magic-bytes
  with `mime_guess` extension fallback), `size_bytes`, `sha256`,
  `file_name`, `protocol`.
- **Well-known fields** (caller-provided, schema-validated):
  `trace_id`, `conversation_id`, `parent_file_ids` (≤ 64),
  `expires_at` (hint, no enforcement in v0.2), `lifecycle`
  (`unspecified` / `transient` / `durable` / `ephemeral`),
  `correlation_id`.
- Free-form `user_metadata` (`map<string,string>`); hard caps:
  64 KiB envelope total, 256 entries, 128-byte keys, 16 KiB values
  — all enforced by `MetadataBuilder::build` and re-checked at the
  engine boundary (`validate_metadata`).
- Typed `MetadataError` hierarchy
  (`OversizeEnvelope` / `OversizeKey` / `OversizeValue` / `TooManyEntries`
  / `TooManyParents` / `OversizeFileName` / `NonUtf8` / …).
- `TransferEngine::send_with_metadata(task, envelope)` accepts a
  caller-built envelope; `metadata_for(receipt_id)` exposes the
  sealed copy until terminal GC.
- `IncomingFile::metadata()` returns the receiver-side envelope
  (decoded from the wire `TransferStart` frame once the QUIC
  wiring lands; bridged at the test layer in v0.2.0).
- `HistoryStore` extension: every JSONL record persists a
  `MetadataJson` mirror of the proto (additive, backward-compatible
  with pre-v0.2.0 records — no `null` written when absent).
- `HistoryStore::query(&HistoryQuery)` — linear-scan filtering by
  `metadata_eq` (AND-semantics over user fields), `trace_id`,
  `lifecycle`, `since`, `until` on top of existing
  direction/protocol/success filters.
- CLI `aerosync send` flags: `--meta key=value` (repeatable),
  `--trace-id`, `--conversation-id`, `--parent`, `--lifecycle`,
  `--correlation-id`, `--content-type`.
- CLI `aerosync history` flags: `--meta key=value`, `--trace-id`,
  `--lifecycle`, `--since`, `--until`. Output now includes
  `trace_id` and a compact `user_metadata` summary.
- MCP tools `send_file` / `send_directory` accept optional
  `metadata`, `trace_id`, `conversation_id`, `parent_file_ids`,
  `lifecycle`, `correlation_id`, `content_type` parameters.
- MCP tool `list_history` accepts `metadata_filter`, `trace_id`,
  `lifecycle`, `since`, `until`; response now includes the full
  `MetadataJson` per record and a top-level `trace_id` shortcut.
- 5 metadata-focused integration tests in `tests/metadata_e2e.rs`
  (roundtrip, persistence, lineage, oversize rejection, system-field
  anti-spoofing).

#### Cross-RFC integration

- `tests/cross_rfc_smoke.rs` — single big end-to-end test that
  exercises all three RFCs together (Python-SDK building blocks +
  Receipt SSE control plane + Metadata sealing/query). Honest about
  its manually-bridged paths (see file-level "Honesty clause"
  documenting the `w3c-quic-receipt-wiring` deferral).

### Changed

- **HTTP server**: migrated from `warp 0.3` → `axum 0.7`. Resolves
  5 RUSTSEC advisories (RUSTSEC-2026-0098/-0099/-0049,
  RUSTSEC-2025-0009 ring AES, RUSTSEC-2025-0134 rustls-pemfile
  unmaintained) by unblocking the rustls upgrade chain that warp
  0.3.7 had pinned to. ~1000 lines of warp filters rewritten as
  `Router::route()` + `tower::ServiceExt::oneshot` test harness.
- **TLS stack**: upgraded `quinn 0.10 → 0.11`, `rustls 0.21 → 0.23`,
  `rcgen 0.11 → 0.13`, `rustls-pemfile 1 → 2`, `reqwest 0.11 → 0.12`.
  `PinnedCertVerifier` rewritten to the new
  `rustls::client::danger::ServerCertVerifier` trait with explicit
  `supported_verify_schemes` / `verify_tls12_signature` /
  `verify_tls13_signature`. Process-wide rustls crypto provider
  (`ring`) is installed lazily on first QUIC use. Closes
  RUSTSEC-2026-0037 (Quinn DoS) and RUSTSEC-2025-0009 (ring AES).
- **ALPN protocol string**: `aerosync` → `aerosync/1`. v0.1.x peers
  cannot negotiate with v0.2.0 peers (see Migration notes).
- **`TransferEngine::send` return type**: now
  `Arc<Receipt<Sender>>` instead of `TransferOutcome`. **BREAKING**
  for direct Rust API consumers; see Migration notes.
- **`IncomingFile`**: surface widened with
  `ack(metadata?)` / `nack(reason)` / `metadata()` / `with_metadata()`.
- `deny.toml`: ignore list trimmed (warp-related entries removed);
  reasons updated to point at the remaining w2c/w3c deferrals.
- `rustls-native-certs` removed from `Cargo.toml` — declared but
  never imported.

### Known limitations (deferred to v0.2.1)

These are intentional scope cuts for the v0.2.0 cycle. They are
called out here so users can plan around them; the design exists
and is partially implemented in each case, only the integration
piece is parked.

- **`w2c-resume-sqlite`** — `ResumeStore` SQLite migration
  (RFC-002 §11). The store is currently JSON-file based; in-flight
  receipts may be lost on crash. SQLite + WAL migration is the
  v0.2.1 trigger.
- **`w3c-quic-receipt-wiring`** — live wiring of the receipt
  stream into the QUIC transport (RFC-002 §6.4). The codec and
  registry exist (`src/protocols/quic_receipt.rs`,
  `src/core/receipt_registry.rs`) and are exercised by the
  `tests/receipts_e2e.rs` suite, but `QuicTransfer::upload` and
  `FileReceiver`'s QUIC accept loop do **not** yet open the bidi
  receipt stream automatically. **Cross-process QUIC senders /
  receivers must use the HTTP SSE control plane to observe receipt
  state.** Same-process senders (Rust unit tests, Python in-process
  flows) work today via the engine bridge.
- **`HistoryStore` SQLite + JSON1 indices** (RFC-003 §8) —
  `HistoryStore::query` is an `O(N)` linear scan over the JSONL
  file. Fine for thousands of records; SQLite migration deferred
  to v0.2.1.
- **Cross-protocol fuzzing harness** (RFC-002 §13) — not built;
  v0.2.0-rc target.
- **`expires_at`** is a hint only — no enforcement in v0.2.
- **Receiver `address`** echoes the user-supplied `host:port`;
  the OS-assigned port is not yet surfaced back to Python
  (RFC-001 §5.3 follow-up).
- **Empty-iter timeout** on `async for f in receiver` blocks
  indefinitely; engine needs an idle-timeout knob.
- **`Config.rendezvous_url`** is accepted but currently a no-op
  (rendezvous transport lands in v0.2.1).
- **mypy `python_version`** is `"3.10"` even though wheels target
  `>=3.9`; `dataclass(slots=True)` is not modeled by mypy on 3.9.
  Runtime is fine on 3.9 via `__slots__`; static-analysis-only.
- **Windows-on-ARM** and **musllinux-aarch64** wheels are deferred
  from the v0.2.0 matrix; see comments in `python-release.yml`.

### Migration notes (v0.1.x → v0.2.0)

- **ALPN bump**. v0.1 senders cannot connect to v0.2 receivers and
  vice versa. Plan a coordinated rollout: roll receivers first,
  then senders.
- **`TransferEngine::send` signature change**. Existing Rust
  consumers must update to handle `Arc<Receipt<Sender>>`. Use
  `.processed().await` (returns `Outcome`) to wait for the terminal
  state with the same blocking semantics as the v0.1.x
  `TransferOutcome` await.
- **Direct callers of `IncomingFile`** that constructed it via the
  v0.1 path now need to choose between
  `IncomingFile::new_without_receipt` (legacy, ack/nack are
  no-ops) and `IncomingFile::new(received, receipt, registry)`
  (RFC-002-aware).
- **`Acked.metadata`** is now a separate piece of data attached to
  the receipt at ack time, not the file envelope (RFC-003 §3
  non-goal: metadata mutation after send).
- **CLI users**: no breaking flags removed; `aerosync send` /
  `receive` keep their pre-existing surface. New `--meta`,
  `--trace-id`, etc. flags are additive.

### CI

- New `python-quality` job (`.github/workflows/python.yml`):
  `mypy --strict` + `ruff check` + `pytest aerosync-py/tests/`
  on every PR.
- New `python-release` workflow
  (`.github/workflows/python-release.yml`): tag-triggered
  abi3-py39 wheel matrix — macOS x86_64 + aarch64, Linux glibc
  x86_64 + aarch64 (`manylinux_2_17`), Linux musl x86_64
  (`musllinux_1_1`), Windows x86_64; sdist also built. All wheels
  are smoke-tested with
  `python -c "import aerosync; aerosync.version()"` on the build
  host before upload. PyPI Trusted Publisher OIDC (no API tokens).
  See [`docs/python/RELEASE-CHECKLIST.md`](docs/python/RELEASE-CHECKLIST.md)
  for the per-release dance.

### Acknowledgements

This release pulls together the work tracked across the v0.2.0
weekly subagent series (w0 RFCs → w8 integration). Honest scope
cuts (`w2c-resume-sqlite`, `w3c-quic-receipt-wiring`) were chosen
deliberately to ship the user-visible API on schedule; both are
the v0.2.1 entry points.

---

## Detailed change log (per-week, archive)

This section preserves the granular per-week entries from the
v0.2.0 development cycle. The consolidated section above is the
canonical reference; this archive is retained for code-archaeology
purposes.

### Added (v0.2.0 — Metadata Envelope, RFC-003)
- **Metadata Envelope (RFC-003)**: every transfer can now carry a
  structured `Metadata` message with system fields (`id`, `from_node`,
  `to_node`, `created_at`, `content_type`, `size_bytes`, `sha256`,
  `file_name`, `protocol`), well-known fields (`trace_id`,
  `conversation_id`, `parent_file_ids`, `expires_at`, `lifecycle`,
  `correlation_id`) and free-form `user_metadata` (`map<string,string>`).
  See [`docs/protocol/metadata-v1.md`](docs/protocol/metadata-v1.md).
- `aerosync::core::metadata::MetadataBuilder` — programmatic builder
  with eager validation: 64 KiB envelope cap, 256 user entries, 128-byte
  keys, 16 KiB values, 64 parent ids, 1024-byte file names, UTF-8
  enforcement, oversized-input typed errors (`MetadataError`).
- `aerosync::core::sniff::sniff_content_type` — automatic content-type
  detection via `infer` (magic bytes) with `mime_guess` (extension)
  fallback; default `application/octet-stream`.
- `TransferEngine::send_with_metadata` — accepts a caller-built
  envelope; system fields (`id`, `from_node`, `created_at`, `sha256`,
  `size_bytes`, `content_type`, `file_name`, `protocol`) are sealed in
  by the engine and cannot be spoofed via `user_metadata`.
- `IncomingFile::metadata()` — receivers can read the envelope on the
  ack channel.
- `HistoryStore` extension — every JSONL record now persists a
  `MetadataJson` mirror of the proto (additive, backward-compatible
  with pre-v0.2.0 records, no `null` written when absent).
- `HistoryStore::query(&HistoryFilter)` — linear-scan filtering by
  `metadata_eq`, `trace_id`, `lifecycle`, `since`, `until` on top of
  existing direction/protocol/success filters. Indexed lookup is
  deferred to v0.2.1 (SQLite migration).
- CLI `aerosync send` flags: `--meta key=value` (repeatable),
  `--trace-id`, `--conversation-id`, `--parent`, `--lifecycle`,
  `--correlation-id`, `--content-type`.
- CLI `aerosync history` flags: `--meta key=value` (repeatable),
  `--trace-id`, `--lifecycle`, `--since`, `--until`. Output now
  includes `trace_id` and a compact `user_metadata` summary.
- MCP tools `send_file` / `send_directory`: optional `metadata`,
  `trace_id`, `conversation_id`, `parent_file_ids`, `lifecycle`,
  `correlation_id`, `content_type` parameters.
- MCP tool `list_history`: optional `metadata_filter`, `trace_id`,
  `lifecycle`, `since`, `until` parameters; response now includes the
  full `MetadataJson` per record and a top-level `trace_id` shortcut.
- Reference doc `docs/protocol/metadata-v1.md` and 5 new
  metadata-focused integration tests in `tests/metadata_e2e.rs`
  (roundtrip, persistence, lineage, oversize rejection, system-field
  anti-spoofing).

### Notes & known limitations (v0.2.0)
- The `Metadata` message is **not yet wired into the QUIC
  `TransferStart` frame** end-to-end; the envelope is sealed in the
  sender engine and surfaced via `Acked.metadata` and the JSONL
  history. Full `TransferStart` integration ships with the upcoming
  `quic_receipt` task.
- `HistoryStore::query` is an `O(N)` scan over the JSONL file. It is
  fine for thousands of records but will be replaced by the SQLite
  backend in v0.2.1.
- `expires_at` is a **hint only** — no enforcement in v0.2.
- The Python SDK does not yet expose a `metadata=` keyword argument
  (deferred to `w5-py-phase1a`); raw `metadata_json` is exposed in the
  meantime.

### Added (v0.2.0 — Python SDK, RFC-001)
- **Python SDK package `aerosync`** (PyO3 + `pyo3-async-runtimes`): a
  fully async public surface (`client()`, `receiver()`, `discover()`)
  built on the existing Rust core. RFC-001 §5.
- `aerosync.Config` dataclass + `Config.from_dict` / `from_toml` /
  `from_default`. Loads from `~/.aerosync/config.toml` by default;
  applies `auth_token` (zeroized), `state_dir`, `log_level`,
  `chunk_size_default`, `timeout_default` to the underlying engine.
  `tomli` is pulled in only on Python <3.11. RFC-001 §5.7 / #11.
- Strict typing: `mypy --strict` green, hand-maintained `_native.pyi`
  stubs, PEP 561 `py.typed` marker, `ruff check` green. New
  `.github/workflows/python.yml` CI job runs all three on every PR.
  RFC-001 §13 #17.
- pytest expansion: 90 passing + 2 skipped (parked against v0.2.1
  with explicit reason strings). Covers Config, history, metadata
  validation, receiver lifecycle, discover, and the existing
  client / receipt / send-source surface. RFC-001 §13 #18.
- **Wheel matrix CI** (`.github/workflows/python-release.yml`):
  abi3-py39 wheels for macOS x86_64, macOS aarch64, Linux glibc
  x86_64 + aarch64 (`manylinux_2_17`), Linux musl x86_64
  (`musllinux_1_1`), and Windows x86_64. sdist also built. All wheels
  are smoke-tested with `python -c "import aerosync; aerosync.version()"`
  on the build host before upload. RFC-001 §13 #19.
- **PyPI Trusted Publisher** (OIDC) workflow + checklist: see
  [`docs/python/RELEASE-CHECKLIST.md`](docs/python/RELEASE-CHECKLIST.md)
  for the one-time setup (PyPI / TestPyPI publisher config, GitHub
  Environments) and per-release dance (TestPyPI dry run → tag → PyPI
  publish via `pypa/gh-action-pypi-publish`). No API tokens stored
  anywhere. RFC-001 §13 #20.

### Notes & known limitations (Python SDK v0.2.0)
- `Config.rendezvous_url` is accepted but currently a no-op
  (rendezvous transport lands in v0.2.1).
- mypy's `python_version` is set to `"3.10"` even though the wheels
  target `>=3.9`; `dataclass(slots=True)` is not modelled by mypy on
  3.9. This is a static-analysis-only quirk (the runtime is fine on
  3.9 via `__slots__`); proper 3.9 mypy support is filed against
  v0.2.1.
- Receiver `address` echoes the user-supplied "host:port" string;
  the OS-assigned port is not yet surfaced back to Python.
- Empty-iter timeout on `async for f in receiver` requires an
  engine-side idle knob (also v0.2.1).
- Windows-on-ARM and musllinux-aarch64 wheels are deferred from the
  v0.2.0 matrix; see comments inline in `python-release.yml`.

### Changed (v0.2.0 prep — TLS stack modernization)
- Upgraded QUIC transport to `quinn 0.11` (from 0.10), which required
  reworking `ClientConfig::new` to wrap rustls via
  `quinn::crypto::rustls::QuicClientConfig` and making
  `SendStream::finish()` synchronous at every call site.
  Closes **RUSTSEC-2026-0037** (Quinn DoS).
- Upgraded `rustls 0.21 → 0.23`. `PinnedCertVerifier` was rewritten to
  the new `rustls::client::danger::ServerCertVerifier` trait (requires
  explicit `supported_verify_schemes`, `verify_tls12_signature`,
  `verify_tls13_signature` implementations) and uses
  `rustls_pki_types::CertificateDer<'_>` / `ServerName<'_>` /
  `UnixTime` in place of the legacy rustls types. The rustls process-
  wide crypto provider (`ring`) is installed lazily on first QUIC use.
  Closes **RUSTSEC-2025-0009** (ring AES panic).
- Upgraded `rustls-pemfile 1 → 2` and migrated `load_tls_from_pem`
  to the iterator-based parsers that return
  `rustls_pki_types::{CertificateDer, PrivateKeyDer}` directly.
- Upgraded `rcgen 0.11 → 0.13`. Self-signed cert generation now uses
  the infallible `CertifiedKey::cert.pem()` /
  `key_pair.serialize_pem()` accessors.
- Upgraded `reqwest 0.11 → 0.12` (pulled in by the new rustls chain;
  no code changes required on our side).
- `deny.toml` ignore list dropped from 7 to 5 entries; reasons updated
  to point to the RFC-002 follow-up that will remove warp.
- `rustls-native-certs` removed from `Cargo.toml` — it was declared but
  never imported in any source file.

### Added
- LICENSE (MIT) and full crate metadata so each crate can be published
  to crates.io.
- English `README.md`; the original Chinese version is preserved as
  `README.zh-CN.md`.
- `SECURITY.md`, `CONTRIBUTING.md`, this changelog, plus issue and PR
  templates.
- Multi-platform release workflow (`.github/workflows/release.yml`):
  tag-triggered builds for macOS (x86_64 + aarch64), Linux (x86_64 +
  aarch64 musl) and Windows (x86_64), uploaded to GitHub Releases.
- `cargo-deny` and `cargo-audit` jobs in CI, plus an MSRV check
  (Rust 1.89).
- `install.sh` one-line installer and a Homebrew formula template under
  `docs/install/`.
- README comparison table vs `scp` / `rsync` / `croc` / `rclone` and a
  dedicated MCP section.

### Changed
- `aerosync-mcp`: every JSON Schema field description for the 8 tools
  is now in English so AI agents (Claude, GPT, Cursor, …) understand
  parameter intent on the first call.
- `get_transfer_status`: the "task not found" error now reports the
  actual configured TTL and points at `AEROSYNC_MCP_TASK_TTL_SECS`.

### Fixed
- (P1) `send_file` now records `resume_json_path` in SQLite, so chunked
  transfers can be resumed after the MCP server is restarted.
- (P2) Renamed `_auth_token` to `mcp_auth_token` across all MCP tools;
  the old name keeps working via `serde(alias)` for backward
  compatibility.
- (P3a) Unified transfer-timeout (1 h default) and task-TTL (24 h
  default); both are now configurable via `AEROSYNC_MCP_TRANSFER_TIMEOUT_SECS`
  and `AEROSYNC_MCP_TASK_TTL_SECS`.

## [0.1.0] - TBD

Initial public release.

[Unreleased]: https://github.com/TechVerseOdyssey/AeroSync/compare/v0.2.0...HEAD
[v0.2.0]: https://github.com/TechVerseOdyssey/AeroSync/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/TechVerseOdyssey/AeroSync/releases/tag/v0.1.0
