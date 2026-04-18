"""RFC-001 §13 #18 / RFC-003 §6 — Python-side metadata validation.

The Rust `MetadataBuilder` enforces all the size + cardinality limits
in `src/core/metadata.rs`. The PyO3 binding maps `MetadataError` to
`ValueError` (see `aerosync-py/src/errors.rs::metadata_err_to_py`),
which keeps the long-standing Python convention "bad argument is
ValueError". These tests pin that contract from the Python side so
future engine refactors cannot silently change the user-visible
exception type.

Each test stages a `send()` against an unroutable peer; the metadata
builder runs first (synchronously inside the call), so we can assert
the validation error without any network I/O happening.
"""

from __future__ import annotations

import asyncio
from pathlib import Path

import aerosync
import pytest

# Mirrors of `src/core/metadata.rs` constants. Kept as module-level
# numbers so the tests fail loudly if RFC-003 §6 ever moves a limit.
MAX_USER_VALUE_BYTES = 16 * 1024
MAX_USER_ENTRIES = 256


def _client_send(
    payload: bytes,
    *,
    metadata: dict[str, str],
) -> None:
    """Run a single `c.send(payload, metadata=...)` to completion (or
    until the metadata builder rejects). The peer is unroutable, so on
    happy-path the call returns a Receipt; on validation failure the
    `await c.send` itself raises `ValueError` synchronously."""

    async def run() -> None:
        async with aerosync.client() as c:
            await c.send(payload, to="nowhere-host:1", metadata=metadata)

    asyncio.run(run())


def test_metadata_oversize_user_value_rejected(tmp_path: Path) -> None:
    """A single `user_metadata` value larger than 16 KiB raises
    ValueError before any network I/O happens."""
    payload = b"x"
    big_value = "v" * (MAX_USER_VALUE_BYTES + 1)
    with pytest.raises(ValueError, match="value"):
        _client_send(payload, metadata={"big": big_value})


def test_metadata_too_many_entries_rejected(tmp_path: Path) -> None:
    """More than 256 user entries → ValueError."""
    payload = b"x"
    meta = {f"k{i}": "v" for i in range(MAX_USER_ENTRIES + 1)}
    with pytest.raises(ValueError, match="entries"):
        _client_send(payload, metadata=meta)


def test_metadata_at_entry_limit_is_accepted(tmp_path: Path) -> None:
    """Exactly 256 entries is the allowed maximum (RFC-003 §6).
    The send itself fails downstream because the peer is bogus, but
    the metadata builder must NOT raise."""
    payload = b"x"
    meta = {f"k{i}": "v" for i in range(MAX_USER_ENTRIES)}

    async def run() -> str:
        async with aerosync.client() as c:
            r = await c.send(payload, to="nowhere-host:1", metadata=meta)
            return r.id

    rid = asyncio.run(run())
    assert rid


def test_metadata_system_field_shadow_is_accepted(tmp_path: Path) -> None:
    """RFC-003 §4.1 / §4.2: writing a system-field name (e.g.
    `"sha256"`) into `user_metadata` MUST succeed — the engine logs a
    warning and the system value wins on read. Crucially: no exception
    on the Python boundary.

    The `metadata={"sha256": "..."}` dict goes straight to the Rust
    builder, which calls `tracing::warn!` and keeps the entry."""
    payload = b"x"
    meta = {"sha256": "deadbeef" * 8}

    async def run() -> str:
        async with aerosync.client() as c:
            r = await c.send(payload, to="nowhere-host:1", metadata=meta)
            return r.id

    rid = asyncio.run(run())
    assert rid


def test_metadata_well_known_lifecycle_string_round_trip(tmp_path: Path) -> None:
    """`metadata['lifecycle']` is a well-known field — accepts the
    documented string vocabulary (RFC-003 §4) and lifts it out of
    `user_metadata`. Bogus values raise ValueError."""
    payload = b"x"
    bad_meta = {"lifecycle": "definitely-not-a-real-stage"}
    with pytest.raises(ValueError, match="lifecycle"):
        _client_send(payload, metadata=bad_meta)
