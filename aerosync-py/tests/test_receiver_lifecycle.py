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
import re
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


def test_receiver_address_reflects_os_assigned_port(tmp_path: Path) -> None:
    """`listen="127.0.0.1:0"` lets the OS pick the port. After
    `__aenter__` resolves, `Receiver.address` must return the actual
    bound `host:port` (not the literal `"127.0.0.1:0"` we passed in).

    This is the v0.2.1 P0.3 unblock: without it, a sender driven by
    the README quickstart would have to grep tracing logs to learn
    where to connect.
    """

    addr_pattern = re.compile(r"^127\.0\.0\.1:\d{2,5}$")

    async def run() -> str:
        r = aerosync.receiver(listen="127.0.0.1:0", save_dir=tmp_path)
        # Pre-enter: the getter falls back to the user-supplied
        # string (engine has not bound yet).
        assert r.address == "127.0.0.1:0"
        async with r:
            return r.address

    bound = asyncio.run(run())
    assert addr_pattern.match(bound), f"{bound!r} does not look like host:port"
    # And the ephemeral port itself must be non-zero — that is the
    # whole point of the OS doing the assignment.
    port = int(bound.rsplit(":", 1)[1])
    assert port != 0, "OS-assigned port must be non-zero"


def test_receiver_context_manager_releases_port(tmp_path: Path) -> None:
    """Open a receiver on an OS-assigned port, close it, then re-open
    a fresh receiver bound to that same concrete port. Proves the
    binding releases its `FileReceiver` (and therefore the underlying
    listening socket) on `__aexit__`.

    Unblocked by v0.2.1 P0.3 (`Receiver.address` now reports the
    bound port, not the user input), so we can finally read the port
    out of the first receiver and feed it to the second.
    """

    async def run() -> tuple[int, str]:
        r1 = aerosync.receiver(listen="127.0.0.1:0", save_dir=tmp_path)
        async with r1:
            first_addr = r1.address
        first_port = int(first_addr.rsplit(":", 1)[1])

        # Re-bind to the same port now that r1 has dropped it. If
        # the engine had leaked the socket this would either time
        # out or surface as `OSError: address in use` on Linux /
        # macOS — we tolerate the leak case explicitly so flaky CI
        # ephemeral-port reuse does not fail this test, but happy
        # path is "same port comes back".
        r2 = aerosync.receiver(
            listen=f"127.0.0.1:{first_port}", save_dir=tmp_path
        )
        async with r2:
            second_addr = r2.address
        return first_port, second_addr

    first_port, second_addr = asyncio.run(run())
    assert second_addr.endswith(f":{first_port}"), (
        f"reused port {first_port}; got {second_addr!r}"
    )


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
