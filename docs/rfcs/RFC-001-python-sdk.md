# RFC-001: Python SDK (`aerosync` on PyPI)

| Field          | Value                                                |
| -------------- | ---------------------------------------------------- |
| Status         | Draft                                                |
| Author         | TechVerseOdyssey                                     |
| Created        | 2026-04-18                                           |
| Target version | v0.2.0                                               |
| Estimated work | ~34.5 engineer-days                                  |
| Depends on     | RFC-002 (Receipt protocol), RFC-003 (Metadata)       |

## Implementation status (2026-04-22)

This RFC intentionally remains **Draft**, but the Python SDK itself is
already substantially implemented and shipped. The current workspace
contains a full `aerosync-py` package, the `CHANGELOG.md` documents
shipped follow-ups through `v0.2.1`, and `docs/v0.3.0-frozen-api.md`
now describes the effective public contract more accurately than some
older examples in this RFC.

### Implemented in the codebase

- Core package layout, maturin/PyO3 binding, `py.typed`, and typed
  Python entry points.
- `client()`, `receiver()`, `version()`, `Config`, `Client`,
  `Receiver`, `IncomingFile`, `Receipt`, and the exported error
  hierarchy.
- Send/receive/history flows, receipt watching, metadata propagation,
  and the Python quickstart paths used by the current docs and tests.

### Drift from the original RFC text

- `discover()` is implemented as an awaitable returning `list[Peer]`,
  not the original `AsyncIterator[Peer]` sketch.
- `Receipt.watch()` and send-time progress callbacks became the primary
  observation APIs; the RFC's standalone `Receipt.progress()` shape was
  not implemented as written.
- The current public contract is best read from
  `docs/v0.3.0-frozen-api.md`, not from every literal type signature in
  this RFC.

### Still open or deferred

- PyPI release operations, trusted-publisher rollout, and some
  docs-site/cookbook deliverables remain operational or documentation
  work rather than code gaps.
- The approval checklist at the end of this RFC has not been updated to
  reflect what already shipped.

## 1. Summary

Ship a Python package `aerosync` (on PyPI) that lets a Python application
send and receive files through the AeroSync transfer engine without
spawning subprocesses or speaking MCP. The package is implemented as a
PyO3 binding over the existing Rust crate `aerosync` and exposes an
async-first, type-annotated API designed to feel native in modern
Python codebases (Python ≥ 3.9, asyncio).

After this RFC ships, the AI-agent killer demo from the v0.2 plan
should run verbatim:

```python
import aerosync

async def producer():
    async with aerosync.client() as c:
        receipt = await c.send(
            "output.csv",
            to="data-cleaner",
            metadata={"trace_id": "run-123"},
        )
        await receipt.processed()

async def consumer():
    async with aerosync.receiver(name="data-cleaner") as r:
        async for incoming in r:
            cleaned = process(incoming.path)
            await incoming.ack()
```

## 2. Motivation

The dominant language for building AI agents is Python (LangChain,
LlamaIndex, CrewAI, AutoGen, SmolAgents, FastAPI-based glue). Today
the only ways for a Python program to use AeroSync are:

1. `subprocess.run(["aerosync", "send", ...])` — slow, no streaming
   progress, no type-safe error model, unable to share auth state.
2. Drive the MCP server through an MCP client library — only useful if
   the calling code is itself an LLM-orchestrated agent; useless for
   server-side cron jobs, ETL, or any non-LLM control flow.

A first-class Python SDK is the **only** path to AeroSync becoming
"the file bus for AI agents". Without it, every code-level integration
will pay subprocess overhead or sit behind an LLM, both of which are
non-starters for production workloads.

## 3. Goals

- Idiomatic Python: `async`/`await`, context managers, async iterators,
  `dataclass` records, full type hints, PEP-561 marker.
- Zero subprocess: in-process Rust runtime via PyO3, shared with the
  user's asyncio event loop.
- Wheels for the platforms AeroSync ships binaries for: macOS x86_64 +
  arm64, Linux x86_64 + aarch64 (manylinux2014 + musllinux), Windows
  x86_64. Python 3.9–3.13.
- Feature parity with the Rust API for the **80% case**: send file,
  send directory, receive, query history, mDNS discovery, status
  polling, receipt handling, metadata.
- One `pip install aerosync` and the killer demo runs.

## 4. Non-goals

- **Sync (blocking) API.** Out of scope for v0.2. We will document the
  trivial `asyncio.run(...)` wrapper for users who really need it.
- **Pydantic models.** Adding a runtime dependency on Pydantic is
  expensive (multi-MB), forces version conflicts, and provides no
  meaningful value for our flat record types. Use `@dataclass(slots=True)`.
- **GIL-free / nogil-build optimization.** Defer until PEP 703 ships.
- **Bundled MCP client.** MCP is reachable via `mcp` (Anthropic
  package); we do not duplicate it here.
- **Type stubs as a separate `aerosync-stubs` package.** Ship inline
  with the binary wheel via `py.typed`.

## 5. Public API surface

All names below are stable as of v0.2.0 and follow SemVer.

### 5.1 Module-level entry points

```python
import aerosync

aerosync.client(*, config: Config | None = None) -> AsyncContextManager[Client]
aerosync.receiver(
    *,
    name: str | None = None,
    listen: str = "0.0.0.0:0",
    save_dir: str | os.PathLike | None = None,
    config: Config | None = None,
) -> AsyncContextManager[Receiver]

aerosync.discover(*, timeout: float = 5.0) -> AsyncIterator[Peer]
aerosync.version() -> str
```

The two context managers are the only "constructors"; everything else
is reached through their methods. There are no module-level
state or globals — calling `client()` twice in the same process gives
two independent clients sharing the same tokio runtime under the hood.

### 5.2 `Client`

```python
class Client:
    async def send(
        self,
        source: str | os.PathLike | bytes | BinaryIO,
        *,
        to: str,                              # peer name (mDNS or rendezvous)
        metadata: Mapping[str, str] | None = None,
        chunk_size: int | None = None,        # default: engine choice
        on_progress: Callable[[Progress], None] | None = None,
        timeout: float | None = None,
    ) -> Receipt: ...

    async def send_directory(
        self,
        source: str | os.PathLike,
        *,
        to: str,
        metadata: Mapping[str, str] | None = None,
        on_progress: Callable[[Progress], None] | None = None,
    ) -> Receipt: ...

    async def history(
        self,
        *,
        limit: int = 100,
        direction: Literal["sent", "received", "all"] = "all",
    ) -> list[HistoryEntry]: ...
```

### 5.3 `Receiver`

```python
class Receiver:
    @property
    def address(self) -> str: ...   # actual bound socket address

    @property
    def name(self) -> str | None: ... # mDNS-advertised name (if any)

    async def __aiter__(self) -> AsyncIterator[IncomingFile]: ...

    async def status(self) -> ReceiverStatus: ...
    async def stop(self) -> None: ...     # also called by __aexit__
```

The async iterator yields one `IncomingFile` per completed transfer.
Backpressure is handled by the iterator: if the consumer is slow, the
receiver keeps buffering chunks to disk but does not yield until the
file is fully written, checksum verified, and metadata available.

### 5.4 Records (frozen dataclasses)

```python
@dataclass(frozen=True, slots=True)
class Peer:
    name: str
    address: str           # "host:port"
    protocols: list[str]   # e.g. ["quic", "http"]

@dataclass(frozen=True, slots=True)
class Progress:
    transferred_bytes: int
    total_bytes: int
    elapsed_seconds: float
    bytes_per_second: float

@dataclass(frozen=True, slots=True)
class HistoryEntry:
    id: str
    direction: Literal["sent", "received"]
    peer: str
    file_name: str
    size_bytes: int
    started_at: datetime
    finished_at: datetime | None
    status: Literal["completed", "failed", "in_progress"]
    error: str | None
```

### 5.5 `Receipt`

`Receipt` is the handle returned by `send()`. It exposes a state
machine that mirrors the Rust side (see RFC-002).

```python
class Receipt:
    @property
    def id(self) -> str: ...
    @property
    def state(self) -> ReceiptState: ...

    async def sent(self, *, timeout: float | None = None) -> None: ...
    async def received(self, *, timeout: float | None = None) -> None: ...
    async def processed(self, *, timeout: float | None = None) -> None: ...

    async def cancel(self) -> None: ...
    async def progress(self) -> AsyncIterator[Progress]: ...

ReceiptState = Literal[
    "pending", "sending", "sent",
    "received", "processed", "failed", "cancelled",
]
```

`await receipt.received()` resolves once the receiver has the bytes and
checksum has matched. `await receipt.processed()` resolves only after
the receiver application calls `incoming.ack()`. Both raise
`TransferFailed` if the receipt transitions to `failed`.

### 5.6 `IncomingFile`

```python
class IncomingFile:
    @property
    def id(self) -> str: ...
    @property
    def path(self) -> str: ...           # final path on local disk
    @property
    def size_bytes(self) -> int: ...
    @property
    def from_peer(self) -> str: ...
    @property
    def metadata(self) -> Mapping[str, str]: ...

    async def ack(self, *, metadata: Mapping[str, str] | None = None) -> None: ...
    async def nack(self, *, reason: str) -> None: ...
```

### 5.7 Configuration

```python
@dataclass
class Config:
    state_dir: str | os.PathLike | None = None       # default: ~/.aerosync
    rendezvous_url: str | None = None                # v0.3+
    auth_token: str | None = None                    # v0.2 shared-secret
    log_level: Literal["error", "warn", "info", "debug", "trace"] = "warn"
```

If `Config` is omitted, the SDK reads `~/.aerosync/config.toml` for
parity with the CLI.

### 5.8 Exception hierarchy

```
AeroSyncError                    # base
├── ConfigError                  # bad config / missing token / invalid path
├── PeerNotFoundError            # mDNS / rendezvous lookup failed
├── ConnectionError              # network unreachable, handshake failed
├── AuthError                    # token mismatch / signature invalid
├── TransferFailed               # transfer aborted mid-flight
│   └── ChecksumMismatch
├── TimeoutError                 # explicit timeout exceeded
└── EngineError                  # internal Rust error, not user-fixable
```

All exceptions subclass `AeroSyncError` so `except aerosync.AeroSyncError`
catches everything. Each carries the original Rust error message in
`.detail` and a stable error code in `.code` (snake_case).

## 6. Architecture

```
┌─────────────────────────────────────────────────┐
│   Python user code (asyncio)                    │
│   await client.send(...)                        │
└────────────────────┬────────────────────────────┘
                     │ pyo3-async-runtimes
                     │  (asyncio future ↔ tokio JoinHandle)
                     ▼
┌─────────────────────────────────────────────────┐
│   aerosync_py (PyO3 bindings; thin shim)        │
│   - PyClient, PyReceiver, PyReceipt …           │
│   - dataclass <-> Rust struct conversion        │
│   - Error mapping                               │
└────────────────────┬────────────────────────────┘
                     │ Rust function calls (no IPC)
                     ▼
┌─────────────────────────────────────────────────┐
│   aerosync (Rust library, unchanged)            │
│   - core::transfer::TransferEngine              │
│   - core::server::FileReceiver                  │
│   - protocols::AutoAdapter                      │
└─────────────────────────────────────────────────┘
                     │
                     ▼
              tokio multi-thread runtime
              (one per process, lazily initialized)
```

### 6.1 Tokio ↔ asyncio bridging

We use `pyo3-async-runtimes` (≥ 0.21) with the `tokio` feature. Each
`async fn` exported from Rust is wrapped with `pyo3_async_runtimes::tokio::future_into_py`
so it returns a Python awaitable backed by a tokio JoinHandle. The
tokio runtime is started **lazily on first call** to any AeroSync
function, with `worker_threads = max(2, num_cpus / 2)`. It is
`Arc<Runtime>` shared across all Clients/Receivers in the process.

The runtime is **never** torn down inside the SDK — Python's
shutdown handler runs `runtime.shutdown_background()` to avoid hanging
on still-open QUIC connections.

### 6.2 Memory model

- File payloads never cross the FFI boundary as Python bytes — the
  Rust engine reads/writes the file system directly.
- `bytes` source for `client.send(b"...", to=...)` is converted to a
  temporary file via `NamedTemporaryFile` for v0.2 (cheap & correct).
  Streaming bytes is a v0.3 enhancement.
- `BinaryIO` source is read in 1 MiB chunks on a background thread to
  avoid blocking the event loop; not zero-copy in v0.2.
- Metadata maps cross the boundary as `HashMap<String, String>` — a
  copy is acceptable since metadata is bounded to 64 KiB total
  (RFC-003).

### 6.3 GIL handling

Long-running operations release the GIL via `Python::allow_threads`.
This is automatic for any Rust `async fn` exposed via
`future_into_py`. Synchronous helpers (e.g. `version()`) do not
release the GIL.

### 6.4 Error mapping

A `From<aerosync::AeroSyncError> for PyErr` impl in the bindings
crate maps each Rust error variant to the corresponding Python
exception class. The Python class is created once per module init
and stored in a `OnceCell` for cheap reuse.

## 7. Workspace layout

A new workspace member, **not** published to crates.io:

```
AeroSync/
├── Cargo.toml             # add "aerosync-py" to workspace members
├── aerosync-py/
│   ├── Cargo.toml         # cdylib, [package] publish = false
│   ├── pyproject.toml     # maturin build backend
│   ├── src/
│   │   ├── lib.rs         # #[pymodule] entry
│   │   ├── client.rs      # PyClient
│   │   ├── receiver.rs    # PyReceiver, PyIncomingFile
│   │   ├── receipt.rs     # PyReceipt
│   │   ├── records.rs     # Peer / Progress / HistoryEntry
│   │   └── errors.rs      # exception hierarchy + mapping
│   ├── python/
│   │   └── aerosync/
│   │       ├── __init__.py    # public re-exports + py.typed marker
│   │       ├── _types.py      # dataclass re-declarations for IDE
│   │       └── py.typed       # PEP 561 marker (empty file)
│   └── tests/
│       ├── test_send_recv.py
│       ├── test_metadata.py
│       ├── test_receipt.py
│       └── test_errors.py
```

`pyproject.toml` (skeleton):

```toml
[build-system]
requires = ["maturin>=1.5,<2.0"]
build-backend = "maturin"

[project]
name = "aerosync"
version = "0.2.0"
requires-python = ">=3.9"
description = "AeroSync — file bus for AI agents (Python SDK)"
license = { text = "MIT" }
readme = "../README.md"
keywords = ["file-transfer", "mcp", "ai-agent", "quic"]
classifiers = [
  "Development Status :: 3 - Alpha",
  "Programming Language :: Python :: 3 :: Only",
  "Programming Language :: Rust",
  "Topic :: Communications :: File Sharing",
]

[project.urls]
Homepage = "https://github.com/TechVerseOdyssey/AeroSync"
Repository = "https://github.com/TechVerseOdyssey/AeroSync"
Documentation = "https://aerosync.dev/python"

[tool.maturin]
features = ["pyo3/extension-module"]
python-source = "python"
module-name = "aerosync._native"
strip = true
```

## 8. Build & distribution

### 8.1 Wheel matrix

| OS / arch                     | Python  | manylinux / musllinux        |
| ----------------------------- | ------- | ---------------------------- |
| macOS x86_64                  | 3.9-3.13 | n/a                          |
| macOS arm64                   | 3.9-3.13 | n/a                          |
| Linux x86_64 (glibc)          | 3.9-3.13 | manylinux_2_17_x86_64        |
| Linux aarch64 (glibc)         | 3.9-3.13 | manylinux_2_17_aarch64       |
| Linux x86_64 (musl)           | 3.9-3.13 | musllinux_1_1_x86_64         |
| Windows x86_64                | 3.9-3.13 | n/a                          |
| **abi3 universal**            | 3.9+     | one wheel per OS, all Pythons |

We target **abi3** (`pyo3 = { features = ["abi3-py39"] }`), reducing
the wheel matrix from 30 to 6 builds. No measurable performance loss
for our use case.

### 8.2 GitHub Actions release pipeline

Reuse the existing `.github/workflows/release.yml` matrix; add a new
`maturin build --release` step for each platform and upload to PyPI
via `pypa/gh-action-pypi-publish` with **trusted publisher**
(no API token in repo).

The pipeline triggers on the same `v*.*.*` tags. Wheel version is
derived from the tag.

### 8.3 PyPI publishing checklist

- [ ] Reserve `aerosync` name on PyPI (TestPyPI dry-run first).
- [ ] Configure trusted publisher: PyPI → Project → Trusted publishers
      → add `TechVerseOdyssey/AeroSync` repo + `release.yml` workflow.
- [ ] Add `[project.urls]` Documentation pointing to the future docs
      site (GitHub Pages until aerosync.dev is up).

## 9. Testing strategy

| Layer            | Tooling             | Count target |
| ---------------- | ------------------- | ------------ |
| Unit (Rust)      | existing            | unchanged    |
| Bindings (Rust)  | `pyo3-pytests`      | 30+          |
| Python integration | `pytest-asyncio`  | 60+          |
| End-to-end (cross-platform) | `pytest` + matrix CI | 10+   |
| Type checking    | `mypy --strict` on `python/`     | clean        |
| Doc build        | `mkdocs build --strict`          | clean        |

Each Python test runs against an in-process receiver bound to a
random port; no external services required. CI runs the full test
matrix on the same 5 platforms as `release.yml`.

## 10. Documentation

- `docs/python/quickstart.md` — install + hello world (10-line demo).
- `docs/python/cookbook.md` — 8 recipes (send, receive, fan-out,
  metadata routing, mDNS discovery, error handling, progress
  reporting, integrating with FastAPI).
- `docs/python/api.md` — auto-generated via `pdoc` from docstrings.
- `examples/python/` — runnable scripts mirroring the cookbook.

Documentation is built by mkdocs and deployed to GitHub Pages on
every push to `master` (separate workflow, ~30 s).

## 11. Adoption

The Python SDK is the recommended entry point for any non-LLM
program speaking to AeroSync. The cookbook includes a "from
`subprocess`" recipe for users who previously shelled out to other
file-transfer CLIs.

There are no v0.1.0 Python users to migrate from — this is the first
shipped Python surface.

## 12. Open questions

| # | Question                                                                                                | Default if unanswered                                  |
| - | ------------------------------------------------------------------------------------------------------- | ------------------------------------------------------ |
| 1 | Should we expose `client.connect_to(peer)` as a separate step before `send()`, for connection reuse?    | No — implicit connection cache, transparent to user    |
| 2 | Should `Receipt.progress()` yield events from a tokio broadcast channel or be polling-based?            | Broadcast channel (push), bridged via asyncio queue    |
| 3 | Should we ship a `aerosync.cli` Python wrapper for parity with the Rust CLI?                            | No — would compete with `cargo install aerosync`       |
| 4 | Should `Config` be Pydantic for validation?                                                             | No — see §4 non-goals; validate manually               |
| 5 | Do we expose `aerosync.protocols.HttpConfig` etc. for advanced tuning?                                  | Defer to v0.3, keep v0.2 surface minimal               |
| 6 | Should we publish a separate `aerosync-mcp` Python package to spawn the MCP server programmatically?    | No — that is the `mcp` Anthropic package's job         |

## 13. Implementation tasks (open as GitHub issues)

| #  | Task                                                                  | Estimate | Depends on |
| -- | --------------------------------------------------------------------- | -------- | ---------- |
| 1  | Scaffold `aerosync-py/` workspace member, maturin + pyo3 hello-world  | 1 d      | —          |
| 2  | Set up `pyo3-async-runtimes` tokio bridge with shared runtime         | 2 d      | #1         |
| 3  | Implement `Client.send()` MVP (file path source, no metadata)         | 2 d      | #2         |
| 4  | Implement `Client.send_directory()`                                   | 1 d      | #3         |
| 5  | Implement `Receiver` + async iterator                                 | 3 d      | #2         |
| 6  | Implement `IncomingFile` (without ack/nack — ack lands with RFC-002)  | 1 d      | #5         |
| 7  | Implement `aerosync.discover()` mDNS wrapper                          | 1 d      | #2         |
| 8  | Define `Peer` / `Progress` / `HistoryEntry` dataclasses + bridges     | 1 d      | #2         |
| 9  | Implement `Client.history()`                                          | 1 d      | #2, #8     |
| 10 | Exception hierarchy + Rust→Python error mapping                       | 1 d      | #2         |
| 11 | `Config` parsing (from dict, from `~/.aerosync/config.toml`)          | 1 d      | #2         |
| 12 | bytes / BinaryIO source support in `send()`                           | 1.5 d    | #3         |
| 13 | `on_progress` callback wiring (background tokio task → asyncio queue) | 2 d      | #3         |
| 14 | `Receipt` skeleton (state enum, `await sent/received/processed`)      | 2 d      | RFC-002 lands |
| 15 | `IncomingFile.ack()/nack()` wiring                                    | 1.5 d    | RFC-002 lands |
| 16 | Metadata pass-through (`metadata=` arg)                               | 1 d      | RFC-003 lands |
| 17 | Type stubs review + `py.typed` marker + mypy --strict CI              | 1 d      | API stable |
| 18 | pytest test suite (60+ tests)                                         | 4 d      | parallel   |
| 19 | maturin wheel build matrix in `release.yml`                           | 2 d      | #1         |
| 20 | PyPI trusted publisher setup + first release                          | 0.5 d    | #19        |
| 21 | mkdocs documentation site (quickstart + cookbook + api)               | 3 d      | API stable |
| 22 | 5 example notebooks runnable in Colab                                 | 1 d      | docs       |

**Total: ~34.5 engineer-days**, broken down as:

| Phase                                                   | Days  |
| ------------------------------------------------------- | ----- |
| Phase 1 — tasks #1–#13, can start immediately           | 18.5  |
| Phase 2 — tasks #14, #15 (wait on RFC-002)              | 3.5   |
| Phase 2 — task  #16 (waits on RFC-003)                  | 1     |
| Phase 3 — tasks #17–#22 (API stable, test, release, docs) | 11.5 |

## 14. Approval

This RFC requires sign-off from:

- [ ] Author (you) on overall API shape and timeline
- [ ] At least one Python-fluent design partner on `Client` / `Receiver`
      ergonomics (target: 1 alpha review before any code is merged)
