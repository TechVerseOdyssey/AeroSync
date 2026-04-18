"""Group A pytest coverage for `Receipt`, `IncomingFile.ack/nack`,
and `Client.send(metadata=...)` pass-through.

The receipt state machine lives in Rust (`aerosync::core::receipt`)
and is exhaustively tested there; these tests check that the
PyO3 binding faithfully exposes the contract from Python.
"""

from __future__ import annotations

import asyncio
import re
import uuid
from pathlib import Path

import aerosync
import pytest
from aerosync import _native

UUID_RE = re.compile(r"^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$")


# ── Receipt: id / state / cancel ──────────────────────────────────────


def test_receipt_id_is_uuid(tmp_path: Path) -> None:
    f = tmp_path / "blob.bin"
    f.write_bytes(b"x" * 4)

    async def run() -> str:
        async with aerosync.client() as c:
            r = await c.send(f, to="nowhere-host:1")
            return r.id

    rid = asyncio.run(run())
    assert UUID_RE.match(rid), f"receipt id is not a UUID: {rid}"
    # UUID v7 has version nibble = 7. Engine uses v4 today
    # (`Uuid::new_v4()` in `transfer.rs`); this is a soft check that
    # the id parses as *some* UUID variant.
    parsed = uuid.UUID(rid)
    assert parsed.version in (4, 7)


def test_receipt_state_is_canonical_label(tmp_path: Path) -> None:
    """Right after `send()` returns the engine has already moved
    the receipt to `stream_opened` (engine code applies `Event::Open`
    immediately). The exact intermediate state isn't part of the
    public contract — what matters is that `state` is one of the
    canonical labels."""
    f = tmp_path / "blob.bin"
    f.write_bytes(b"x" * 4)

    async def run() -> str:
        async with aerosync.client() as c:
            r = await c.send(f, to="nowhere-host:1")
            return r.state

    state = asyncio.run(run())
    assert state in {
        "initiated",
        "stream_opened",
        "data_transferred",
        "stream_closed",
        "processing",
        "completed",
        "failed",
    }


def test_receipt_cancel_transitions_to_failed(tmp_path: Path) -> None:
    f = tmp_path / "blob.bin"
    f.write_bytes(b"x" * 4)

    async def run() -> str:
        async with aerosync.client() as c:
            r = await c.send(f, to="nowhere-host:1")
            # Cancel is best-effort; if the receipt is already
            # terminal (transfer raced to failure) cancel raises.
            try:
                await r.cancel("test-abort")
            except ValueError:
                pass
            return r.state

    state = asyncio.run(run())
    # After cancel the local receipt MUST be terminal — either
    # `failed` (cancel won) or `completed` (terminal raced cancel).
    assert state in {"failed", "completed"}


def test_receipt_processed_returns_outcome_dict(tmp_path: Path) -> None:
    """Drive a Receipt through cancel and verify `processed()` returns
    the documented Outcome dict shape."""
    f = tmp_path / "blob.bin"
    f.write_bytes(b"x" * 4)

    async def run() -> dict[str, object]:
        async with aerosync.client() as c:
            r = await c.send(f, to="nowhere-host:1")
            try:
                await r.cancel("test-outcome")
            except ValueError:
                pass
            return await r.processed()

    outcome = asyncio.run(run())
    assert isinstance(outcome, dict)
    assert outcome["status"] in {"acked", "nacked", "cancelled", "errored"}
    # Outcome dataclass mirror should accept the dict verbatim.
    # mypy can't narrow `dict[str, object]` to `Outcome.__init__`
    # field types one-by-one, so the **kwargs splat is widened to
    # `object`. The runtime contract is dict-of-{str|int|None}.
    typed = aerosync.Outcome(**outcome)  # type: ignore[arg-type]
    assert typed.status == outcome["status"]


# ── IncomingFile.ack / nack via the test helper ───────────────────────


def test_incoming_file_ack_drives_receipt_to_completed(tmp_path: Path) -> None:
    target = tmp_path / "received.bin"
    target.write_bytes(b"hello")
    incoming = _native._test_make_incoming_file(target, "received.bin", 5)
    assert incoming.state == "processing"

    async def go() -> str:
        return await incoming.ack()

    result = asyncio.run(go())
    # `ack` returns the post-event state label.
    assert result == "completed"
    assert incoming.state == "completed"


def test_incoming_file_nack_drives_receipt_to_failed(tmp_path: Path) -> None:
    target = tmp_path / "rejected.bin"
    target.write_bytes(b"nope")
    incoming = _native._test_make_incoming_file(target, "rejected.bin", 4)

    async def go() -> str:
        return await incoming.nack("schema-mismatch")

    state = asyncio.run(go())
    assert state == "failed"


def test_incoming_file_metadata_default_empty(tmp_path: Path) -> None:
    target = tmp_path / "x.bin"
    target.write_bytes(b"")
    incoming = _native._test_make_incoming_file(target, "x.bin", 0)
    assert incoming.metadata == {}


def test_incoming_file_metadata_round_trips_user_dict(tmp_path: Path) -> None:
    target = tmp_path / "x.bin"
    target.write_bytes(b"")
    incoming = _native._test_make_incoming_file(
        target, "x.bin", 0, None, {"trace_id": "run-7", "tenant": "acme"}
    )
    md = incoming.metadata
    assert md["trace_id"] == "run-7"
    assert md["tenant"] == "acme"


def test_incoming_file_double_ack_raises(tmp_path: Path) -> None:
    """Re-acking a terminal receipt must raise (state machine §6.4
    'absorbing' invariant) rather than silently no-oping."""
    target = tmp_path / "x.bin"
    target.write_bytes(b"")
    incoming = _native._test_make_incoming_file(target, "x.bin", 0)

    async def go() -> None:
        await incoming.ack()
        await incoming.ack()

    with pytest.raises(ValueError):
        asyncio.run(go())


# ── metadata pass-through (#16) ───────────────────────────────────────


def test_metadata_pass_through_accepts_well_known_and_user_keys(
    tmp_path: Path,
) -> None:
    """`metadata={"trace_id": ..., "tenant": ...}` must build a valid
    sealed envelope. The transfer itself fails (no receiver), but a
    `Receipt` is still issued — proof that the `MetadataBuilder`
    accepted both the well-known shortcut and the free-form user key."""
    f = tmp_path / "blob.bin"
    f.write_bytes(b"hello")

    async def run() -> str:
        async with aerosync.client() as c:
            r = await c.send(
                f,
                to="nowhere-host:1",
                metadata={"trace_id": "run-7", "tenant": "acme"},
            )
            return r.id

    rid = asyncio.run(run())
    assert UUID_RE.match(rid)


def test_metadata_invalid_lifecycle_value_rejected(tmp_path: Path) -> None:
    f = tmp_path / "blob.bin"
    f.write_bytes(b"x")

    async def run() -> None:
        async with aerosync.client() as c:
            await c.send(f, to="alice", metadata={"lifecycle": "permanent"})

    with pytest.raises(ValueError, match="lifecycle"):
        asyncio.run(run())


def test_metadata_oversize_user_value_raises_value_error(tmp_path: Path) -> None:
    f = tmp_path / "blob.bin"
    f.write_bytes(b"x")
    huge = {"k": "v" * (16 * 1024 + 1)}

    async def run() -> None:
        async with aerosync.client() as c:
            await c.send(f, to="alice", metadata=huge)

    with pytest.raises(ValueError):
        asyncio.run(run())


def test_metadata_lifecycle_string_round_trips_canonical_value(
    tmp_path: Path,
) -> None:
    """All four canonical Lifecycle values must parse without error."""
    f = tmp_path / "blob.bin"
    f.write_bytes(b"x")

    async def run() -> None:
        async with aerosync.client() as c:
            for lc in ("unspecified", "transient", "durable", "ephemeral"):
                r = await c.send(f, to="alice", metadata={"lifecycle": lc})
                assert UUID_RE.match(r.id)

    asyncio.run(run())
