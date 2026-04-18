"""Pure-Python dataclass declarations for the AeroSync record types.

These mirror the `#[pyclass]` shapes declared in
`aerosync/src/records.rs`. The Rust extension returns `_native.Peer`
etc.; this module re-declares the same field shapes as `@dataclass`
so IDE / mypy / docstrings have a stable Python-native target.

w6 task #16 will wire these dataclasses to the Rust-side records via
`__init_subclass__` registration so the two views unify at runtime.
For w5 the dataclasses are the documentation surface only.
"""

from __future__ import annotations

from dataclasses import dataclass
from datetime import datetime
from enum import Enum
from typing import Optional


class Lifecycle(str, Enum):
    """RFC-003 §5 metadata lifecycle hint.

    The string values are the canonical wire spelling used by the
    `Metadata.lifecycle` field on every transfer.
    """

    UNSPECIFIED = "unspecified"
    TRANSIENT = "transient"
    DURABLE = "durable"
    EPHEMERAL = "ephemeral"


@dataclass(frozen=True, slots=True)
class Peer:
    """A single AeroSync peer discovered via mDNS or rendezvous."""

    name: str
    address: str
    port: int


@dataclass(frozen=True, slots=True)
class Progress:
    """Snapshot of an in-flight transfer.

    `bytes_sent` / `bytes_total` are over the entire request (a
    directory transfer aggregates across files). `files_sent` /
    `files_total` are 1 for single-file `send()`.
    """

    bytes_sent: int
    bytes_total: int
    files_sent: int
    files_total: int
    elapsed_secs: float


@dataclass(frozen=True, slots=True)
class Outcome:
    """Terminal outcome returned by `await receipt.processed()`.

    The Rust binding returns this as a plain ``dict`` (cheap & no
    extra dataclass round-trip on the hot path); this dataclass is
    the documented Python-side mirror so users who want a typed
    handle can write ``Outcome(**outcome_dict)``.

    `status` is one of ``"acked"``, ``"nacked"``, ``"cancelled"``,
    ``"errored"``. `reason` carries the free-form string for nack /
    cancel; `code` and `detail` carry the numeric + textual diagnostic
    for the errored path.
    """

    status: str
    reason: Optional[str] = None
    code: Optional[int] = None
    detail: Optional[str] = None


@dataclass(frozen=True, slots=True)
class HistoryEntry:
    """One row of the persisted transfer history.

    Mirrors the Rust `HistoryEntry` minus engine-internal fields
    (`avg_speed_bps`, `duration_ms`, …) that would otherwise clutter
    the v0.2 surface. w6 task #9 wires `Client.history()` to surface
    these via the `_NativeHistoryEntry` returned by the Rust side and
    converted via this dataclass shape.
    """

    id: str
    direction: str
    file_name: str
    size_bytes: int
    sha256: Optional[str] = None
    completed_at: Optional[datetime] = None
    receipt_id: Optional[str] = None
    receipt_state: Optional[str] = None
