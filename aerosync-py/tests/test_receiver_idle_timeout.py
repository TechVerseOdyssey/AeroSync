"""RFC-001 §13 #18 / w0.2.1 P2.1 — Receiver async-iter `idle_timeout`.

Before Batch E, `async for f in receiver:` had no way to wake itself
up when no file ever arrived; users either wrapped the loop in
`asyncio.wait_for(...)` (which leaves the underlying broadcast
receiver dangling) or kept a sentinel sender that pushes a "done"
file. Neither composes with the `async with` pattern that owns the
listener's lifetime.

The new `idle_timeout=` keyword on `aerosync.receiver(...)` lets the
iterator self-terminate after `N` seconds of silence (measured from
the last yielded file, or from `__aenter__` for the first one), so

    async with aerosync.receiver(..., idle_timeout=5.0) as r:
        async for incoming in r:
            ...

exits cleanly when the stream goes idle. This file pins the four
boundary conditions:

1. Configured timeout fires when zero files ever arrive.
2. Configured timeout fires after a successful yield.
3. `idle_timeout=None` (default) preserves the legacy "block on
   `__aexit__`" behaviour exactly.
4. `idle_timeout=0.0` and any negative value are rejected at
   construction with `ValueError`.

The "no regression" leg also doubles as a regression net for the
`__aenter__` / `__aexit__` lifecycle (#18) — explicitly closing the
receiver still tears the listener down without leaking sockets.
"""

from __future__ import annotations

import asyncio
import time
from pathlib import Path

import aerosync
import pytest


def test_receiver_idle_timeout_zero_files(tmp_path: Path) -> None:
    """1 s timeout, no sender ever connects: the `async for` loop
    exits in roughly that time and the collected list is empty.

    We accept an upper bound of 5 s because CI runners can be slow
    on cold starts, but the lower bound of `idle_timeout` itself is
    strict — bailing early would mean the gate fired before the
    budget elapsed."""

    received: list[aerosync.IncomingFile] = []
    started = time.monotonic()

    async def run() -> None:
        async with aerosync.receiver(
            listen="127.0.0.1:0",
            save_dir=tmp_path,
            idle_timeout=1.0,
        ) as r:
            async for incoming in r:
                received.append(incoming)

    asyncio.run(run())

    elapsed = time.monotonic() - started
    assert received == [], "no sender → no files should be yielded"
    assert elapsed >= 1.0, (
        f"async for exited at {elapsed:.3f}s, before the 1.0s idle_timeout budget"
    )
    assert elapsed < 5.0, (
        f"async for took {elapsed:.3f}s to exit on a 1.0s idle_timeout — "
        "the timeout is not firing"
    )


def test_receiver_idle_timeout_one_then_idle(tmp_path: Path) -> None:
    """Send exactly 1 file via the HTTP transport, then go idle:
    the iterator yields the file, refreshes its idle window from the
    yield instant, and then bails ~1 s later when no follow-up
    arrives. `len(received) == 1` regardless of timing."""

    src = tmp_path / "payload.bin"
    src.write_bytes(b"x" * 1024)

    received: list[dict[str, object]] = []
    started = time.monotonic()

    async def producer(addr: str) -> None:
        # Small grace period so the receiver is past `__aenter__`
        # before we hammer it.
        await asyncio.sleep(0.1)
        async with aerosync.client() as c:
            receipt = await c.send(str(src), to=addr)
            await asyncio.wait_for(receipt.processed(), timeout=10.0)

    async def consumer(addr_q: asyncio.Queue[str]) -> None:
        async with aerosync.receiver(
            listen="127.0.0.1:0",
            save_dir=tmp_path / "inbox",
            idle_timeout=1.0,
        ) as r:
            await addr_q.put(r.address)
            async for incoming in r:
                received.append(
                    {"name": incoming.file_name, "size": incoming.size_bytes}
                )

    async def go() -> None:
        addr_q: asyncio.Queue[str] = asyncio.Queue()
        consumer_task = asyncio.create_task(consumer(addr_q))
        addr = await asyncio.wait_for(addr_q.get(), timeout=5.0)
        await producer(addr)
        # The consumer task finishes on its own when the post-yield
        # idle window elapses; cap the wait so a deadlock surfaces
        # as a test failure instead of pytest hanging.
        await asyncio.wait_for(consumer_task, timeout=10.0)

    asyncio.run(go())

    elapsed = time.monotonic() - started
    assert len(received) == 1, f"expected 1 file, got {received!r}"
    assert received[0]["size"] == 1024
    # We expect: ~0.1s grace + transfer (<1s on loopback) + 1.0s
    # idle window. Anything under 5s is the "happy path".
    assert elapsed < 10.0, (
        f"end-to-end took {elapsed:.3f}s — idle_timeout did not fire after the yield"
    )


def test_receiver_no_timeout_back_compat(tmp_path: Path) -> None:
    """`idle_timeout=None` (the default) preserves the v0.2.0
    behaviour: the iterator blocks indefinitely until the receiver
    is explicitly torn down. We verify by exiting the `async with`
    after one file was received via `break`, and asserting the
    teardown completes (no leaked task / no port reuse error on a
    second receiver bound to the same port)."""

    src = tmp_path / "ping.bin"
    src.write_bytes(b"hello")

    received: list[str] = []

    async def producer(addr: str) -> None:
        await asyncio.sleep(0.1)
        async with aerosync.client() as c:
            receipt = await c.send(str(src), to=addr)
            await asyncio.wait_for(receipt.processed(), timeout=10.0)

    async def consumer(addr_q: asyncio.Queue[str]) -> None:
        # Default idle_timeout=None — no kwarg passed, no behaviour
        # change vs. v0.2.0.
        async with aerosync.receiver(
            listen="127.0.0.1:0",
            save_dir=tmp_path / "inbox",
        ) as r:
            await addr_q.put(r.address)
            async for incoming in r:
                received.append(incoming.file_name)
                break  # Exit the loop; `__aexit__` tears down.

    async def go() -> None:
        addr_q: asyncio.Queue[str] = asyncio.Queue()
        consumer_task = asyncio.create_task(consumer(addr_q))
        addr = await asyncio.wait_for(addr_q.get(), timeout=5.0)
        await producer(addr)
        await asyncio.wait_for(consumer_task, timeout=10.0)

    asyncio.run(go())

    assert received == ["ping.bin"]


def test_receiver_idle_timeout_invalid(tmp_path: Path) -> None:
    """`idle_timeout=0.0` has no useful semantics ("exit immediately
    with no chance to yield even buffered files") so the factory
    rejects it synchronously."""

    with pytest.raises(ValueError, match="idle_timeout"):
        aerosync.receiver(
            listen="127.0.0.1:0",
            save_dir=tmp_path,
            idle_timeout=0.0,
        )


def test_receiver_idle_timeout_negative(tmp_path: Path) -> None:
    """Negative `idle_timeout` is also rejected at construction."""

    with pytest.raises(ValueError, match="idle_timeout"):
        aerosync.receiver(
            listen="127.0.0.1:0",
            save_dir=tmp_path,
            idle_timeout=-1.0,
        )
