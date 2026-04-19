"""Type stubs for the `aerosync._native` PyO3 extension module.

PyO3 abi3 cdylibs cannot emit `.pyi` automatically (the abi3 boundary
hides per-method docstrings + signatures from any introspection-based
generator), so this file is hand-maintained. Keep it in sync with the
Rust `#[pyclass]` / `#[pyfunction]` declarations in `aerosync-py/src/`.

Stability contract: every name re-exported by `aerosync/__init__.py`
must also be declared here at the matching shape — `mypy --strict`
relies on this file for the entire native surface.
"""

from __future__ import annotations

import os
from collections.abc import Awaitable, Mapping
from typing import (
    Any,
    Callable,
)

# ── Module-level entry points ─────────────────────────────────────────

def version() -> str: ...
def client(*, config: Any | None = ...) -> Client: ...
def receiver(
    name: str | None = ...,
    listen: str | None = ...,
    save_dir: str | os.PathLike[str] | None = ...,
    config: Any | None = ...,
) -> Receiver: ...
def discover(timeout: float = ...) -> Awaitable[list[Peer]]: ...

# ── Records (frozen `#[pyclass]`) ─────────────────────────────────────

class Peer:
    name: str
    address: str
    port: int

    def __init__(self, name: str, address: str, port: int) -> None: ...
    def __repr__(self) -> str: ...

class Progress:
    bytes_sent: int
    bytes_total: int
    files_sent: int
    files_total: int
    elapsed_secs: float

    def __init__(
        self,
        bytes_sent: int,
        bytes_total: int,
        files_sent: int,
        files_total: int,
        elapsed_secs: float,
    ) -> None: ...
    def __repr__(self) -> str: ...

class HistoryEntry:
    id: str
    direction: str
    file_name: str
    size_bytes: int
    sha256: str | None
    completed_at: int | None
    receipt_id: str | None
    receipt_state: str | None

    def __init__(
        self,
        id: str,
        direction: str,
        file_name: str,
        size_bytes: int,
        sha256: str | None = ...,
        completed_at: int | None = ...,
        receipt_id: str | None = ...,
        receipt_state: str | None = ...,
    ) -> None: ...
    def __repr__(self) -> str: ...

# ── Receipt ───────────────────────────────────────────────────────────

class Receipt:
    @property
    def id(self) -> str: ...
    @property
    def state(self) -> str: ...
    def sent(self) -> Awaitable[str]: ...
    def received(self) -> Awaitable[str]: ...
    def processed(self) -> Awaitable[dict[str, Any]]: ...
    def cancel(self, reason: str = ...) -> Awaitable[str]: ...
    def watch(self) -> ReceiptWatcher: ...
    def __repr__(self) -> str: ...

class ReceiptWatcher:
    def __aiter__(self) -> ReceiptWatcher: ...
    def __anext__(self) -> Awaitable[str]: ...

# ── Client ────────────────────────────────────────────────────────────

class Client:
    def __aenter__(self) -> Awaitable[Client]: ...
    # Returns `Awaitable[None]` (NOT `Awaitable[bool]`): the binding's
    # `__aexit__` always returns `False`, never suppressing a raised
    # exception. Typing it as `None` lets mypy --strict treat the
    # `async with` block as transparent for control-flow analysis.
    def __aexit__(
        self,
        exc_type: type[BaseException] | None = ...,
        exc_value: BaseException | None = ...,
        traceback: Any | None = ...,
    ) -> Awaitable[None]: ...
    def send(
        self,
        source: str | os.PathLike[str] | bytes | Any,
        *,
        to: str,
        metadata: Mapping[str, str] | None = ...,
        chunk_size: int | None = ...,
        timeout: float | None = ...,
        # `on_progress` accepts the native `Progress` instance; users
        # often type-annotate against `aerosync.Progress` (re-exported
        # from `_types.py` as the public-facing dataclass shape) which
        # is structurally identical but a separate class. We accept
        # `Callable[[Any], None]` here so both annotations type-check;
        # at runtime the binding always passes a `_native.Progress`.
        on_progress: Callable[[Any], None] | None = ...,
    ) -> Awaitable[Receipt]: ...
    def send_directory(
        self,
        source: str | os.PathLike[str],
        *,
        to: str,
        metadata: Mapping[str, str] | None = ...,
    ) -> Awaitable[Receipt]: ...
    def history(
        self,
        *,
        limit: int = ...,
        direction: str = ...,
        metadata_filter: Mapping[str, str] | None = ...,
        history_path: str | os.PathLike[str] | None = ...,
    ) -> Awaitable[list[HistoryEntry]]: ...

# ── Receiver + IncomingFile ───────────────────────────────────────────

class Receiver:
    @property
    def name(self) -> str | None: ...
    @property
    def address(self) -> str: ...
    @property
    def quic_address(self) -> str | None: ...
    def __aenter__(self) -> Awaitable[Receiver]: ...
    def __aexit__(
        self,
        exc_type: type[BaseException] | None = ...,
        exc_value: BaseException | None = ...,
        traceback: Any | None = ...,
    ) -> Awaitable[None]: ...
    def __aiter__(self) -> Receiver: ...
    def __anext__(self) -> Awaitable[IncomingFile]: ...

class IncomingFile:
    @property
    def path(self) -> os.PathLike[str]: ...
    @property
    def file_name(self) -> str: ...
    @property
    def size_bytes(self) -> int: ...
    @property
    def sha256(self) -> str | None: ...
    @property
    def receipt_id(self) -> str | None: ...
    @property
    def state(self) -> str | None: ...
    @property
    def metadata(self) -> dict[str, str]: ...
    @property
    def trace_id(self) -> str | None: ...
    @property
    def conversation_id(self) -> str | None: ...
    @property
    def correlation_id(self) -> str | None: ...
    @property
    def lifecycle(self) -> str | None: ...
    def ack(
        self, metadata: Mapping[str, str] | None = ...
    ) -> Awaitable[str]: ...
    def nack(self, reason: str) -> Awaitable[str]: ...
    def __repr__(self) -> str: ...

# Test-only constructor — kept here so test files that use it pass
# `mypy --strict`. Underscore prefix marks it as private; the contract
# is "may change between releases without notice".
def _test_make_incoming_file(
    path: str | os.PathLike[str],
    file_name: str,
    size_bytes: int = ...,
    sha256: str | None = ...,
    user_metadata: Mapping[str, str] | None = ...,
) -> IncomingFile: ...

# Test-only encoder — produces the canonical wire-format value for the
# `X-Aerosync-Metadata` HTTP header (`base64(protobuf(Metadata))`,
# RFC-003 §8.4) from a flat dict matching `Client.send(metadata=...)`.
# Used by `tests/test_metadata_propagation.py` to drive the receiver
# through the HTTP transport without needing a fully-wired Python
# sender engine.
def _test_encode_metadata_header(
    metadata: Mapping[str, str] | None = ...,
) -> str: ...

# ── Exception hierarchy (RFC-001 §5.8) ────────────────────────────────
#
# Every class subclasses `AeroSyncError` (which itself subclasses
# `Exception`). Each instance carries `.code` (snake_case identifier)
# and `.detail` (original Rust message); we annotate them on the base
# so attribute access does not require a per-subclass override.

class AeroSyncError(Exception):
    code: str
    detail: str

class ConfigError(AeroSyncError): ...
class PeerNotFoundError(AeroSyncError): ...
class ConnectionError(AeroSyncError): ...
class AuthError(AeroSyncError): ...
class TransferFailed(AeroSyncError): ...
class ChecksumMismatch(TransferFailed): ...
class TimeoutError(AeroSyncError): ...
class EngineError(AeroSyncError): ...
