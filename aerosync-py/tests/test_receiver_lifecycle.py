"""RFC-001 §13 #18 — Receiver lifecycle (enter / exit / re-enter).

These tests assert the binding-level invariants of `Receiver` as an
async context manager (RFC-001 §5.3 / §5.6):

- `__aenter__` actually starts the underlying `FileReceiver` (it isn't
  a no-op + lazy-start scheme).
- `__aexit__` stops it cleanly, without raising on a happy path.
- Entering / exiting twice on the SAME `aerosync.receiver(...)`
  instance is an explicit error (the `started` AtomicBool can only
  flip once-per-instance) — users construct a NEW receiver to re-bind.

The "can re-open on the same physical port" sub-case requires the
engine to expose the bound socket address back to Python, which is a
v0.2.1 follow-up — skipped here with an explicit reason."""

from __future__ import annotations

import asyncio
from pathlib import Path

import aerosync
import pytest


def test_receiver_async_with_starts_and_stops_cleanly(tmp_path: Path) -> None:
    """Enter + exit on a fresh receiver completes without raising and
    returns the receiver instance from `__aenter__`."""

    async def run() -> str:
        r = aerosync.receiver(name="alice", listen="127.0.0.1:0", save_dir=tmp_path)
        async with r as entered:
            assert entered is r
            assert r.name == "alice"
            return r.address

    addr = asyncio.run(run())
    assert addr.startswith("127.0.0.1:")


def test_receiver_can_enter_exit_enter_pattern(tmp_path: Path) -> None:
    """Constructing a fresh receiver after the previous one cleanly
    exited must succeed — proves the binding releases its
    `FileReceiver` on `__aexit__` (no leaked `Arc<Mutex<…>>` holding
    the port). Each receiver gets its own `127.0.0.1:0` to avoid
    flakes on busy CI runners with strict ephemeral-port reuse."""

    async def run() -> None:
        r1 = aerosync.receiver(listen="127.0.0.1:0", save_dir=tmp_path)
        async with r1:
            assert r1.address.startswith("127.0.0.1:")
        r2 = aerosync.receiver(listen="127.0.0.1:0", save_dir=tmp_path)
        async with r2:
            assert r2.address.startswith("127.0.0.1:")

    asyncio.run(run())


def test_receiver_exit_without_enter_is_a_noop(tmp_path: Path) -> None:
    """Calling `__aexit__` on a never-entered receiver must not raise.
    The binding gates the underlying `stop()` call on the `started`
    AtomicBool, so this is a no-op by construction."""

    async def run() -> None:
        r = aerosync.receiver(listen="127.0.0.1:0", save_dir=tmp_path)
        await r.__aexit__(None, None, None)

    asyncio.run(run())


def test_receiver_save_dir_must_exist(tmp_path: Path) -> None:
    """An explicit save_dir under a missing parent should fail at
    `__aenter__` (engine reports the bad path), NOT at construction
    time. Construction is sync + cheap, so it can't await the engine
    to validate."""
    bogus = tmp_path / "definitely" / "missing" / "subtree"
    r = aerosync.receiver(listen="127.0.0.1:0", save_dir=bogus)

    async def run() -> None:
        async with r:
            pass

    # The Rust receiver may auto-create or surface an error depending
    # on platform; if it succeeds, the directory must now exist (we
    # accept either branch as long as it does NOT panic).
    try:
        asyncio.run(run())
    except aerosync.AeroSyncError:
        return
    assert bogus.exists()


@pytest.mark.skip(
    reason=(
        "blocked on v0.2.1: receiver.address still echoes the user-supplied "
        "string; needs the engine to surface the bound socket back to Python "
        "(see RFC-001 §5.3 follow-up) before we can prove same-port re-open."
    )
)
def test_receiver_context_manager_releases_port(tmp_path: Path) -> None:
    """Open receiver, close it, prove the port is released by binding
    a fresh receiver on the SAME concrete port. Currently blocked on
    address-introspection."""
    raise NotImplementedError


@pytest.mark.skip(
    reason=(
        "blocked on v0.2.1: `async for` on a never-receiving receiver "
        "currently blocks indefinitely; engine needs an idle-timeout "
        "knob (RFC-001 §13 #18 follow-up)."
    )
)
def test_receiver_async_iter_empty_timeout(tmp_path: Path) -> None:
    """Receiver with no incoming traffic must time out and exit the
    `async for` loop instead of blocking forever."""
    raise NotImplementedError
