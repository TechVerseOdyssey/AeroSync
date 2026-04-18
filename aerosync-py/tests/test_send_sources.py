"""Group C pytest coverage for `Client.send` source variants
(RFC-001 §6.2 / w6 task #12) and the `on_progress` callback
(RFC-001 §5.2 / w6 task #13).

These tests stage transfers against an unroutable peer
(`nowhere-host:1`) — the actual upload always fails, but the
binding-side wiring (source materialisation, callback marshalling,
temp-file lifecycle) runs end-to-end before the failure surfaces.
"""

from __future__ import annotations

import asyncio
import io
import re
from pathlib import Path
from typing import List

import pytest

import aerosync

UUID_RE = re.compile(r"^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$")


# ── #12 source variants: bytes / BinaryIO / path ─────────────────────


def test_send_accepts_bytes_source(tmp_path: Path) -> None:
    """`source=b"..."` stages a NamedTempFile and returns a Receipt."""

    async def run() -> str:
        async with aerosync.client() as c:
            r = await c.send(b"hello world", to="nowhere-host:1")
            return r.id

    rid = asyncio.run(run())
    assert UUID_RE.match(rid)


def test_send_accepts_empty_bytes_source(tmp_path: Path) -> None:
    """Zero-length payload still produces a Receipt — the engine is
    happy with empty files and so are we."""

    async def run() -> str:
        async with aerosync.client() as c:
            r = await c.send(b"", to="nowhere-host:1")
            return r.id

    rid = asyncio.run(run())
    assert UUID_RE.match(rid)


def test_send_accepts_binaryio_source(tmp_path: Path) -> None:
    """Anything with `.read(size)` returning bytes is a valid source."""
    buf = io.BytesIO(b"x" * (3 * 1024 * 1024 + 7))  # crosses two 1 MiB chunks

    async def run() -> str:
        async with aerosync.client() as c:
            r = await c.send(buf, to="nowhere-host:1")
            return r.id

    rid = asyncio.run(run())
    assert UUID_RE.match(rid)


def test_send_accepts_pathlib_path(tmp_path: Path) -> None:
    """The classic path-source path still works after the source
    extraction was generalised to Bound<PyAny>."""
    f = tmp_path / "blob.bin"
    f.write_bytes(b"hello")

    async def run() -> str:
        async with aerosync.client() as c:
            r = await c.send(f, to="nowhere-host:1")
            return r.id

    rid = asyncio.run(run())
    assert UUID_RE.match(rid)


def test_send_accepts_str_path(tmp_path: Path) -> None:
    f = tmp_path / "blob.bin"
    f.write_bytes(b"hello")

    async def run() -> str:
        async with aerosync.client() as c:
            r = await c.send(str(f), to="nowhere-host:1")
            return r.id

    rid = asyncio.run(run())
    assert UUID_RE.match(rid)


def test_send_rejects_unsupported_source_type(tmp_path: Path) -> None:
    """An int (or any non-path / non-bytes / non-BinaryIO) is a TypeError."""

    async def run() -> None:
        async with aerosync.client() as c:
            await c.send(12345, to="alice")

    with pytest.raises(TypeError):
        asyncio.run(run())


# ── #13 on_progress callback ─────────────────────────────────────────


def test_send_rejects_non_callable_on_progress(tmp_path: Path) -> None:
    f = tmp_path / "blob.bin"
    f.write_bytes(b"x")

    async def run() -> None:
        async with aerosync.client() as c:
            await c.send(f, to="alice", on_progress=42)

    with pytest.raises(TypeError):
        asyncio.run(run())


def test_send_invokes_on_progress_at_least_once(tmp_path: Path) -> None:
    """The polling pump fires the callback once per observed change
    in `bytes_transferred`, and once more on terminal. We trigger
    terminal explicitly via `cancel` so the test doesn't depend on
    a real worker draining the task."""
    f = tmp_path / "blob.bin"
    f.write_bytes(b"x" * 16)

    received: List[aerosync.Progress] = []

    def cb(progress: aerosync.Progress) -> None:
        received.append(progress)

    async def run() -> str:
        async with aerosync.client() as c:
            r = await c.send(f, to="nowhere-host:1", on_progress=cb)
            try:
                await r.cancel("test-progress")
            except ValueError:
                pass
            # Give the polling task a chance to observe the terminal
            # transition. Two POLL_MS windows is plenty.
            await asyncio.sleep(0.3)
            return r.state

    state = asyncio.run(run())
    assert state in {"failed", "completed"}
    assert len(received) >= 1
    last = received[-1]
    assert isinstance(last, aerosync._NativeProgress) or hasattr(last, "bytes_sent")
    # bytes_total should reflect the staged file size.
    assert last.bytes_total >= 16


def test_send_progress_callback_exception_does_not_crash(tmp_path: Path) -> None:
    """An exception inside `on_progress` must not propagate into the
    awaitable. The pump logs and exits."""
    f = tmp_path / "blob.bin"
    f.write_bytes(b"x" * 4)

    def cb(_progress: aerosync.Progress) -> None:
        raise RuntimeError("user code broke")

    async def run() -> str:
        async with aerosync.client() as c:
            r = await c.send(f, to="nowhere-host:1", on_progress=cb)
            try:
                await r.cancel("test-cb-error")
            except ValueError:
                pass
            await asyncio.sleep(0.3)
            return r.state

    state = asyncio.run(run())
    assert state in {"failed", "completed"}


def test_send_bytes_with_on_progress_combines_cleanly(tmp_path: Path) -> None:
    """The two new features compose: bytes source + on_progress
    must work in the same call."""
    received: List[aerosync.Progress] = []

    def cb(p: aerosync.Progress) -> None:
        received.append(p)

    async def run() -> str:
        async with aerosync.client() as c:
            r = await c.send(b"payload", to="nowhere-host:1", on_progress=cb)
            try:
                await r.cancel("test-combo")
            except ValueError:
                pass
            await asyncio.sleep(0.3)
            return r.id

    rid = asyncio.run(run())
    assert UUID_RE.match(rid)
    # Callback may or may not have fired depending on timing, but
    # the call itself must have completed without raising.
    for p in received:
        assert hasattr(p, "bytes_sent")
