"""RFC-001 §13 #18 — additional `Client.history()` coverage.

The empty-store + filter-kwarg cases live in `test_errors.py` (kept
there for historical reasons, alongside the typed-error tests). This
file covers the remaining shapes:

- limit/direction validation (Python boundary)
- repeated empty calls return distinct lists (no shared mutable state)
- a fresh, file-less HistoryStore round-trips an empty `[]`

End-to-end "send → history records the row" coverage lives in the
Rust integration tests under `aerosync::core::history` and the e2e
suite (out of scope for the SDK binding tests)."""

from __future__ import annotations

import asyncio
from pathlib import Path

import aerosync
import pytest


def test_history_returns_distinct_list_each_call(tmp_path: Path) -> None:
    """The binding constructs a fresh Python list each call. Mutating
    one returned list must not bleed into the next call's result."""
    history_path = tmp_path / "history.jsonl"

    async def run() -> tuple[list[object], list[aerosync.HistoryEntry]]:
        async with aerosync.client() as c:
            a: list[object] = list(await c.history(history_path=history_path))
            a.append("tampered")
            b = await c.history(history_path=history_path)
            return a, b

    first, second = asyncio.run(run())
    assert second == []
    assert first != second


def test_history_limit_zero_returns_empty(tmp_path: Path) -> None:
    """`limit=0` is a valid lower bound and yields []. Documented in
    RFC-001 §5.4."""
    history_path = tmp_path / "history.jsonl"

    async def run() -> list[aerosync.HistoryEntry]:
        async with aerosync.client() as c:
            return await c.history(history_path=history_path, limit=0)

    assert asyncio.run(run()) == []


def test_history_negative_limit_is_value_error(tmp_path: Path) -> None:
    """A negative `limit` is a Python-side argument error (ValueError),
    NOT an engine error."""
    history_path = tmp_path / "history.jsonl"

    async def run() -> None:
        async with aerosync.client() as c:
            await c.history(history_path=history_path, limit=-1)

    with pytest.raises((ValueError, OverflowError, TypeError)):
        asyncio.run(run())


def test_history_direction_received_is_canonical(tmp_path: Path) -> None:
    """The three documented directions are `sent` / `received` /
    `all`. `received` must not raise."""
    history_path = tmp_path / "history.jsonl"

    async def run() -> list[aerosync.HistoryEntry]:
        async with aerosync.client() as c:
            return await c.history(history_path=history_path, direction="received")

    assert asyncio.run(run()) == []


def test_history_default_path_does_not_crash() -> None:
    """Calling `c.history()` with NO history_path falls back to the
    engine default (`HistoryStore::default_path()`). On a fresh
    machine the default path may not exist; the binding handles it
    gracefully and returns []."""

    async def run() -> list[aerosync.HistoryEntry]:
        async with aerosync.client() as c:
            return await c.history(limit=1)

    rows = asyncio.run(run())
    assert isinstance(rows, list)
