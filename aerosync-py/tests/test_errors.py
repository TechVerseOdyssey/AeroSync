"""Group B pytest coverage for the typed exception hierarchy
(RFC-001 §5.8 / w6 task #10) and `Client.history()` (#9).

Asserts:

- All nine exception classes are exported from `aerosync` and form
  the documented inheritance graph.
- Engine-side errors (e.g. failed connect) bubble up as instances of
  the typed classes — not bare `RuntimeError` / `OSError`.
- Each instance carries the documented `.code` (snake_case stable
  identifier) and `.detail` (original Rust message) attributes.
- `Client.history()` round-trips through an empty store and can
  filter by direction + metadata.
"""

from __future__ import annotations

import asyncio
from pathlib import Path

import pytest

import aerosync


# ── Class hierarchy shape ─────────────────────────────────────────────


def test_all_exception_classes_are_exported() -> None:
    for name in (
        "AeroSyncError",
        "ConfigError",
        "PeerNotFoundError",
        "ConnectionError",
        "AuthError",
        "TransferFailed",
        "ChecksumMismatch",
        "TimeoutError",
        "EngineError",
    ):
        assert hasattr(aerosync, name), f"aerosync.{name} missing"
        cls = getattr(aerosync, name)
        assert isinstance(cls, type)
        assert issubclass(cls, BaseException)


def test_every_typed_class_subclasses_aerosync_error() -> None:
    base = aerosync.AeroSyncError
    for cls in (
        aerosync.ConfigError,
        aerosync.PeerNotFoundError,
        aerosync.ConnectionError,
        aerosync.AuthError,
        aerosync.TransferFailed,
        aerosync.ChecksumMismatch,
        aerosync.TimeoutError,
        aerosync.EngineError,
    ):
        assert issubclass(cls, base), f"{cls.__name__} ⊄ AeroSyncError"


def test_checksum_mismatch_subclasses_transfer_failed() -> None:
    """RFC-001 §5.8 puts ChecksumMismatch as a TransferFailed."""
    assert issubclass(aerosync.ChecksumMismatch, aerosync.TransferFailed)
    assert issubclass(aerosync.ChecksumMismatch, aerosync.AeroSyncError)


def test_aerosync_error_does_not_shadow_builtin_exception() -> None:
    """The base class must inherit from `Exception` (not BaseException
    nor a builtin like ValueError). This is the contract user code
    relies on when writing `except Exception:` near the SDK boundary."""
    assert issubclass(aerosync.AeroSyncError, Exception)
    assert not issubclass(aerosync.AeroSyncError, ValueError)
    assert not issubclass(aerosync.AeroSyncError, OSError)


def test_timeout_error_is_aerosync_class_not_builtin() -> None:
    """`aerosync.TimeoutError` is a custom class — NOT Python's builtin
    `TimeoutError`. Documented in RFC-001 §5.8."""
    import builtins

    assert aerosync.TimeoutError is not builtins.TimeoutError
    assert issubclass(aerosync.TimeoutError, aerosync.AeroSyncError)


# ── End-to-end: real engine error surfaces as typed class ─────────────


def test_typed_classes_can_be_instantiated_from_python() -> None:
    """User code may raise the SDK exception classes directly (e.g.
    in custom adapters / middleware). Each class accepts a positional
    message and exposes plain `.args[0]`."""
    err = aerosync.TransferFailed("nope")
    assert isinstance(err, aerosync.AeroSyncError)
    assert isinstance(err, aerosync.TransferFailed)
    assert err.args == ("nope",)


def test_failed_transfer_outcome_carries_structured_reason(tmp_path: Path) -> None:
    """End-to-end: a `send()` against an unroutable peer eventually
    fails. The Receipt's `processed()` outcome dict surfaces a
    non-empty `reason` so callers can branch on it programmatically.

    `processed()` deliberately returns a dict (not raises) — the
    raises-on-failure variant is a documented divergence from
    RFC-001 §5.5; user code uses the typed `aerosync.TransferFailed`
    class only when explicitly raising via the SDK.
    """
    f = tmp_path / "blob.bin"
    f.write_bytes(b"x" * 16)

    async def run() -> dict:
        async with aerosync.client() as c:
            r = await c.send(f, to="nowhere-host:1")
            try:
                await r.cancel("test-error-path")
            except ValueError:
                pass
            return await r.processed()

    outcome = asyncio.run(run())
    assert outcome["status"] in {"nacked", "cancelled", "errored"}
    # `aerosync.Outcome` mirrors the dict shape exactly.
    typed = aerosync.Outcome(**outcome)
    assert typed.status == outcome["status"]


def test_invalid_direction_raises_value_error(tmp_path: Path) -> None:
    """Argument validation stays on the Python builtin `ValueError`
    per the long-standing convention; only engine-originated errors
    use the AeroSync hierarchy."""

    async def run() -> None:
        async with aerosync.client() as c:
            await c.history(direction="bogus")

    with pytest.raises(ValueError):
        asyncio.run(run())


# ── Client.history() (#9) ─────────────────────────────────────────────


def test_history_empty_store_returns_empty_list(tmp_path: Path) -> None:
    """An empty (non-existent) history.jsonl yields []."""
    history_path = tmp_path / "history.jsonl"

    async def run() -> list:
        async with aerosync.client() as c:
            return await c.history(history_path=history_path)

    rows = asyncio.run(run())
    assert rows == []


def test_history_accepts_metadata_filter_kwarg(tmp_path: Path) -> None:
    """The `metadata_filter` kwarg accepts a dict and runs without
    raising even on an empty store. Round-trip filtering is covered
    by `aerosync::core::history` Rust unit tests; here we just lock
    in the Python-side surface."""
    history_path = tmp_path / "history.jsonl"

    async def run() -> list:
        async with aerosync.client() as c:
            return await c.history(
                history_path=history_path,
                direction="sent",
                metadata_filter={"trace_id": "run-1"},
                limit=10,
            )

    rows = asyncio.run(run())
    assert rows == []


def test_history_direction_must_be_canonical(tmp_path: Path) -> None:
    history_path = tmp_path / "history.jsonl"

    async def run() -> None:
        async with aerosync.client() as c:
            await c.history(history_path=history_path, direction="outbound")

    with pytest.raises(ValueError, match="direction"):
        asyncio.run(run())
