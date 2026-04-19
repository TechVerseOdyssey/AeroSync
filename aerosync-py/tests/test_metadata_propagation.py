"""HTTP metadata propagation — Python SDK end-to-end (RFC-003 §8.4 /
w0.2.1 batch B "P0.1").

These tests exercise the Python-visible shape of the metadata
propagation work shipped in batch B by driving a real
`aerosync.Receiver` through its HTTP transport and asserting that
the receiving iterator surfaces the wire-decoded envelope on
`IncomingFile.metadata` plus the new well-known field getters
(`trace_id`, `conversation_id`, `correlation_id`, `lifecycle`).

## Why the sender is a hand-rolled HTTP POST, not `aerosync.client()`

The Python `Client` does **not** start its `TransferEngine` worker
in v0.2 — see `tests/test_killer_demo.py` (skipped pending
`w3c-quic-receipt-wiring`). So `await c.send(...)` returns a fresh
receipt but never actually moves bytes. The Rust integration test
(`tests/metadata_http_propagation.rs`) covers the *real* sender
path with `TransferEngine + AutoAdapter`; here we focus on the
Python *receiver* surface, posting bytes directly via stdlib
`urllib` so the test stays dependency-free.

## Scope

1. **Happy path** — POST a multipart upload with a properly
   `base64(protobuf(Metadata))`-encoded `X-Aerosync-Metadata`
   header and assert `IncomingFile.metadata`, `.trace_id`,
   `.conversation_id`, `.correlation_id`, `.lifecycle` match.
2. **No header** — POST without the metadata header lands a file
   whose `IncomingFile.metadata == {}` and whose well-known
   getters all return `None`.
3. **Negative — malformed base64** — `400` with the canonical
   `{"error": "invalid metadata header: ..."}` envelope; no
   `IncomingFile` materialises.
4. **Negative — malformed protobuf** — same canonical 400.
"""

from __future__ import annotations

import asyncio
import base64
import json
from pathlib import Path
from typing import Any
from urllib import request as urlrequest
from urllib.error import HTTPError

import aerosync
from aerosync import _native


def _multipart_body(filename: str, payload: bytes, *, boundary: str) -> tuple[bytes, str]:
    """Build a minimal `multipart/form-data` body matching what
    `Client.send` would emit. Returns `(body_bytes, content_type)`."""
    crlf = b"\r\n"
    parts: list[bytes] = []
    parts.append(f"--{boundary}".encode())
    parts.append(
        f'Content-Disposition: form-data; name="file"; filename="{filename}"'.encode()
    )
    parts.append(b"Content-Type: application/octet-stream")
    parts.append(b"")
    parts.append(payload)
    parts.append(f"--{boundary}--".encode())
    parts.append(b"")
    body = crlf.join(parts)
    return body, f"multipart/form-data; boundary={boundary}"


def _post_upload(
    addr: str,
    *,
    payload: bytes = b"hi",
    filename: str = "x.bin",
    metadata_header: str | None = None,
) -> tuple[int, bytes]:
    """Synchronously POST a multipart body to `/upload`. Returns
    `(status_code, body_bytes)`. Uses stdlib `urllib` so the test
    suite stays dependency-free."""
    body, content_type = _multipart_body(filename, payload, boundary="boundarymetadatatest")
    headers: dict[str, str] = {"Content-Type": content_type}
    if metadata_header is not None:
        headers["X-Aerosync-Metadata"] = metadata_header
    req = urlrequest.Request(
        f"http://{addr}/upload", data=body, method="POST", headers=headers
    )
    try:
        with urlrequest.urlopen(req, timeout=5.0) as resp:
            return int(resp.status), resp.read()
    except HTTPError as e:
        return int(e.code), e.read()


async def _drive_receiver(
    tmp_path: Path,
    posts: list[dict[str, Any]],
    *,
    expect_files: int,
) -> tuple[list[dict[str, object]], list[tuple[int, bytes]]]:
    """Boot a real receiver, run `posts` (each a kwargs dict for
    `_post_upload`) against it from a worker thread, and collect
    both (a) the receiver iterator's view of every file that
    landed, up to `expect_files`, and (b) the HTTP responses for
    each POST.

    Posts run sequentially so the asserter can correlate outcomes
    with the request that triggered them.
    """
    received: list[dict[str, object]] = []
    responses: list[tuple[int, bytes]] = []
    addr_q: asyncio.Queue[str] = asyncio.Queue()
    done = asyncio.Event()

    async def consumer() -> None:
        async with aerosync.receiver(
            name="prop-receiver",
            listen="127.0.0.1:0",
            save_dir=str(tmp_path / "inbox"),
        ) as r:
            await addr_q.put(r.address)
            if expect_files == 0:
                # Negative-only run — wait for the producer to
                # finish all POSTs and signal us out.
                await done.wait()
                return
            async for incoming in r:
                received.append(
                    {
                        "metadata": dict(incoming.metadata),
                        "trace_id": incoming.trace_id,
                        "conversation_id": incoming.conversation_id,
                        "correlation_id": incoming.correlation_id,
                        "lifecycle": incoming.lifecycle,
                        "file_name": incoming.file_name,
                        "size_bytes": incoming.size_bytes,
                    }
                )
                # Drive the receiver-side ack so the iterator's
                # internal receipt walks to a terminal state and
                # does not keep the loop wedged.
                await incoming.ack()
                if len(received) >= expect_files:
                    return

    consumer_task = asyncio.create_task(consumer())
    try:
        addr = await asyncio.wait_for(addr_q.get(), timeout=5.0)
        for post_kwargs in posts:
            status, body = await asyncio.to_thread(_post_upload, addr, **post_kwargs)
            responses.append((status, body))
        done.set()
        if expect_files > 0:
            await asyncio.wait_for(consumer_task, timeout=10.0)
        else:
            await asyncio.wait_for(consumer_task, timeout=5.0)
    finally:
        if not consumer_task.done():
            consumer_task.cancel()
            try:
                await consumer_task
            except (asyncio.CancelledError, StopAsyncIteration):
                pass

    return received, responses


# ── 1. Happy path ────────────────────────────────────────────────────


def test_http_metadata_propagation_end_to_end(tmp_path: Path) -> None:
    """A POST with a properly-encoded `X-Aerosync-Metadata` header
    surfaces every well-known field plus the user_metadata map on
    the receiving `IncomingFile`."""

    header = _native._test_encode_metadata_header(
        {
            "trace_id": "trace-py-1",
            "conversation_id": "conv-py-1",
            "correlation_id": "corr-py-1",
            "lifecycle": "durable",
            "tenant": "acme",
            "agent_id": "indexer",
        }
    )

    received, responses = asyncio.run(
        _drive_receiver(
            tmp_path,
            posts=[
                {
                    "filename": "metadata-payload.csv",
                    "payload": b"hello,metadata\n1,2\n",
                    "metadata_header": header,
                }
            ],
            expect_files=1,
        )
    )

    assert responses == [(200, b"\"ok\"")] or (
        responses[0][0] == 200
    ), f"upload should succeed, got {responses!r}"

    assert len(received) == 1, f"expected exactly one received file, got {received!r}"
    rec = received[0]

    assert rec["file_name"] == "metadata-payload.csv"
    assert rec["trace_id"] == "trace-py-1"
    assert rec["conversation_id"] == "conv-py-1"
    assert rec["correlation_id"] == "corr-py-1"
    # `lifecycle="durable"` projects to the `LIFECYCLE_DURABLE`
    # enum on the wire; the receiver maps that back to its
    # `as_str_name` form.
    assert rec["lifecycle"] == "LIFECYCLE_DURABLE"

    # Free-form user_metadata survives verbatim. Well-known fields
    # are NOT mirrored into user_metadata — the typed getter is
    # the canonical access path for them.
    metadata = rec["metadata"]
    assert isinstance(metadata, dict)
    assert metadata.get("tenant") == "acme"
    assert metadata.get("agent_id") == "indexer"
    assert "trace_id" not in metadata
    assert "lifecycle" not in metadata


# ── 2. Absent header ────────────────────────────────────────────────


def test_http_upload_without_metadata_yields_empty_metadata(tmp_path: Path) -> None:
    """A POST without the `X-Aerosync-Metadata` header lands a
    file whose `IncomingFile.metadata` is the empty dict and whose
    well-known getters all return `None`. The legacy / smoke-test
    code path must keep working unchanged."""

    received, responses = asyncio.run(
        _drive_receiver(
            tmp_path,
            posts=[
                {
                    "filename": "plain.bin",
                    "payload": b"plain",
                    "metadata_header": None,
                }
            ],
            expect_files=1,
        )
    )
    assert responses[0][0] == 200, f"upload should succeed, got {responses!r}"

    assert len(received) == 1
    rec = received[0]
    assert rec["metadata"] == {}
    assert rec["trace_id"] is None
    assert rec["conversation_id"] is None
    assert rec["correlation_id"] is None
    assert rec["lifecycle"] is None


# ── 3+4. Negative — malformed headers ───────────────────────────────


def _negative_test(tmp_path: Path, header_value: str) -> None:
    """Boot a receiver, hit `/upload` with a deliberately-broken
    `X-Aerosync-Metadata` header, and assert the response is the
    canonical 400 envelope."""

    received, responses = asyncio.run(
        _drive_receiver(
            tmp_path,
            posts=[
                {
                    "filename": "x.bin",
                    "payload": b"hi",
                    "metadata_header": header_value,
                }
            ],
            expect_files=0,
        )
    )

    assert responses[0][0] == 400, (
        f"malformed X-Aerosync-Metadata must 400, got {responses[0][0]!r}"
    )
    body = responses[0][1]
    assert isinstance(body, (bytes, bytearray))
    parsed = json.loads(body.decode("utf-8"))
    assert isinstance(parsed.get("error"), str), (
        f"400 body must contain a string `error`, got {parsed!r}"
    )
    assert parsed["error"].startswith("invalid metadata header:"), (
        f"400 body must carry the canonical 'invalid metadata header' prefix, got: {parsed['error']!r}"
    )
    # The receiver must NOT have materialised a file from the
    # rejected request.
    assert received == []


def test_http_upload_rejects_malformed_base64_metadata_header(tmp_path: Path) -> None:
    """`!!!not-base64!!!` is not valid under any of STANDARD,
    STANDARD_NO_PAD, URL_SAFE, or URL_SAFE_NO_PAD. The receiver
    must reject it before attempting protobuf decode."""

    _negative_test(tmp_path, "!!!not-base64!!!")


def test_http_upload_rejects_malformed_protobuf_metadata_header(tmp_path: Path) -> None:
    """A buffer that base64-decodes cleanly but is NOT a valid
    `Metadata` proto message must produce the same canonical 400."""

    bogus = bytes([0xFF] * 8)  # invalid prost tag
    header = base64.standard_b64encode(bogus).decode("ascii")
    _negative_test(tmp_path, header)
