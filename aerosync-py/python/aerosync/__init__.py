"""AeroSync — file bus for AI agents (Python SDK).

Public re-exports of the `aerosync._native` PyO3 extension plus the
pure-Python dataclass declarations used by IDE / mypy.

See RFC-001 for the full design: the only constructors are the
`client()` / `receiver()` async factories; everything else is reached
through their methods.
"""

from __future__ import annotations

from aerosync._config import Config
from aerosync._native import (
    AeroSyncError,
    AuthError,
    ChecksumMismatch,
    Client,
    ConfigError,
    ConnectionError,
    EngineError,
    HistoryEntry,
    IncomingFile,
    Peer,
    PeerNotFoundError,
    Progress,
    Receipt,
    Receiver,
    TimeoutError,
    TransferFailed,
    client,
    discover,
    receiver,
    version,
)
from aerosync._types import HistoryEntry as _DocHistoryEntry
from aerosync._types import Lifecycle, Outcome
from aerosync._types import Peer as _DocPeer
from aerosync._types import Progress as _DocProgress

# Backward-compatible aliases for the previous (w5/w6) public names.
# The native classes have always been the runtime types returned by
# the SDK; the dataclass mirrors in `_types.py` are kept as IDE / docs
# helpers under `_DocXxx` and reachable via the historical `_NativeXxx`
# aliases below for any user code that has reached past `__all__`.
_NativeHistoryEntry = HistoryEntry
_NativePeer = Peer
_NativeProgress = Progress

__all__ = [
    "Config",
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
