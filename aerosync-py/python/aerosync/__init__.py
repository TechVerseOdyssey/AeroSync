"""AeroSync — file bus for AI agents (Python SDK).

Public re-exports of the `aerosync._native` PyO3 extension plus the
pure-Python dataclass declarations used by IDE / mypy.

See RFC-001 for the full design: the only constructors are the
`client()` / `receiver()` async factories; everything else is reached
through their methods.
"""

from __future__ import annotations

from aerosync._native import (
    AeroSyncError,
    AuthError,
    ChecksumMismatch,
    Client,
    ConfigError,
    ConnectionError,
    EngineError,
    HistoryEntry as _NativeHistoryEntry,
    IncomingFile,
    Peer as _NativePeer,
    PeerNotFoundError,
    Progress as _NativeProgress,
    Receipt,
    Receiver,
    TimeoutError,
    TransferFailed,
    client,
    discover,
    receiver,
    version,
)
from aerosync._types import HistoryEntry, Lifecycle, Outcome, Peer, Progress

__all__ = [
    # Exception hierarchy (RFC-001 §5.8). All subclass `AeroSyncError`
    # so user code can write `except aerosync.AeroSyncError` to catch
    # everything the SDK raises. Each instance also carries `.code`
    # (snake_case identifier) and `.detail` (original Rust message)
    # attributes for structured logging / metric tagging.
    "AeroSyncError",
    "AuthError",
    "ChecksumMismatch",
    "ConfigError",
    "ConnectionError",
    "EngineError",
    "PeerNotFoundError",
    "TimeoutError",
    "TransferFailed",
    "Client",
    "HistoryEntry",
    "IncomingFile",
    "Lifecycle",
    "Outcome",
    "Peer",
    "Progress",
    "Receipt",
    "Receiver",
    "client",
    "discover",
    "receiver",
    "version",
    # Native classes are re-exported under leading-underscore aliases
    # so advanced users can still reach them; the public surface uses
    # the dataclass shapes from `_types`.
    "_NativeHistoryEntry",
    "_NativePeer",
    "_NativeProgress",
]

__version__ = version()
