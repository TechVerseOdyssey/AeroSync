"""RFC-001 §1 — the "killer demo" round-trip, end-to-end through the
public Python SDK surface.

# Status (v0.2.1, Batch D)

Marked ``xfail(strict=True)`` pending two follow-ups whose fixes
overlap and require a deliberate cross-cut decision rather than a
rushed patch in the closing batch of the v0.2.1 honesty release.

# Why xfail (post-Batch-C)

The Python SDK demo from `README.md` requires three pieces working
together in one process (a Python sender talking to a Python
receiver bound at `127.0.0.1:0`):

1. `aerosync.client().send(...)` building a `Metadata` envelope and
   actually delivering bytes — currently fails because the Python
   `Client` constructs a `TransferEngine` but never calls
   `engine.start(adapter)`. Without a worker the queued
   `TransferTask` sits in `Pending` forever, `ProgressMonitor` never
   reports `Completed`, and the engine bridge never advances the
   sender-side receipt past `StreamOpened`. (`aerosync-py/src/lib.rs`
   `make_client` → `aerosync-py/src/client.rs` `from_transfer_config`.)
2. Even if (1) is wired, `await receipt.processed()` still hangs:
   the engine bridge in `src/core/transfer.rs::send_with_metadata`
   walks `Open → Close → Close → Process` on the sender-side
   `Receipt<Sender>` and **stops at `Processing`**. The terminal
   `Acked` transition has to come from somewhere — the QUIC path
   (Batch C) takes it from the inbound `ReceiptFrame::Acked`
   on the receipt stream; the HTTP path has no equivalent wire
   for ack-from-receiver in v0.2.x. RFC-002 §6.4 documents the
   `POST /v1/receipts/:id/ack` endpoint but the Python receiver's
   `IncomingFile.ack()` only walks its locally-constructed
   *receiver-side* synthetic receipt (see the "Linkage caveat" in
   `aerosync-py/src/receiver.rs`); it does not know the sender's
   address, so it cannot post the ack.
3. Auto-acking the sender's HTTP receipt at the engine bridge on
   `TransferStatus::Completed` would close the Python gap in one
   line — but it would also break the
   `tests/receipts_e2e.rs::e2e_quic_receipt_nack_with_reason` flow
   (which fakes a "completed transfer" via `SuccessAdapter` and
   then expects the receipt to remain in `Processing` while the
   receiver-side application decides ack vs nack). That trade-off
   is an RFC-002 §6 semantic decision — "is HTTP 200 application-
   level Ack?" — and belongs to v0.3.0's transport-vs-application
   split (see `docs/v0.3.0-refactor-plan.md`), not a closing batch
   of v0.2.1.

# When this test should flip back to passing

Either of:

- **v0.3.0 Plan §"Receipt control plane unification"** wires the
  Python `Client` to start an `AutoAdapter` and adds explicit
  receiver→sender ack propagation for the HTTP path (parallel to
  the QUIC receipt stream). Once that lands, remove the `xfail`
  and the demo round-trips end-to-end.
- **OR** a deliberate v0.2.x patch decides `HTTP 200 = Ack` is the
  right semantic for the sender-side receipt and updates the
  receipts_e2e expectations accordingly. (Not the call to make in
  Batch D.)

The body of the test below is the verbatim `README.md` quickstart;
when ``xfail`` flips to `XPASS` CI will hard-fail and prompt us to
remove the marker.
"""

from __future__ import annotations

import asyncio
from pathlib import Path

import aerosync
import pytest


@pytest.mark.xfail(
    strict=True,
    reason=(
        "v0.2.1 honesty deferral: the Python `aerosync.client()` factory "
        "does not yet call `engine.start(adapter)`, so queued transfers "
        "never run; even once started, the engine bridge in "
        "`src/core/transfer.rs::send_with_metadata` parks the sender's "
        "receipt at `Processing` because the HTTP transport has no "
        "ack-from-receiver wire (RFC-002 §6.4 endpoint exists but the "
        "Python receiver's `IncomingFile.ack()` only walks a synthetic "
        "local receipt — see `aerosync-py/src/receiver.rs` 'Linkage "
        "caveat'). Auto-acking on HTTP 200 in the bridge would close "
        "the Python gap but break `tests/receipts_e2e.rs` "
        "`e2e_quic_receipt_nack_with_reason`, which is an RFC-002 §6 "
        "semantic decision tracked for the v0.3.0 receipt-control-plane "
        "unification (see `docs/v0.3.0-refactor-plan.md`). When that "
        "lands, this xfail flips to XPASS and CI yells at us to remove "
        "the marker."
    ),
)
def test_killer_demo_round_trip(tmp_path: Path) -> None:
    """The verbatim RFC-001 §1 / README.md Python quickstart demo.

    Exercises:
      - `aerosync.client()` async context manager
      - `Client.send(source, to=..., metadata={...})` returning a
        `Receipt`
      - `Receipt.processed()` resolving to an `Outcome` dict whose
        `status` field is one of the canonical terminal labels
      - `aerosync.receiver(name=..., listen=..., save_dir=...)` async
        context manager + async-iterator
      - `IncomingFile.metadata` exposing the sender's envelope
      - `IncomingFile.ack()` driving the receiver-side terminal

    This is the SDK contract from `README.md` "Send a file" / "Receive
    a file" — if it passes, the README is accurate. If it does not, the
    README is a lie and the skip reason MUST be updated to reflect
    whichever new deferral surfaced.
    """

    src = tmp_path / "report.csv"
    src.write_bytes(b"hello,world\n1,2\n")

    received: list[dict[str, object]] = []

    async def producer(peer_addr: str) -> dict[str, object]:
        async with aerosync.client() as c:
            receipt = await c.send(
                str(src),
                to=peer_addr,
                metadata={"trace_id": "demo-trace", "agent_id": "producer"},
            )
            outcome: dict[str, object] = await asyncio.wait_for(
                receipt.processed(), timeout=10.0
            )
            return outcome

    async def consumer(addr_q: asyncio.Queue[str]) -> None:
        async with aerosync.receiver(
            name="demo-receiver",
            listen="127.0.0.1:0",
            save_dir=str(tmp_path / "inbox"),
        ) as r:
            await addr_q.put(r.address)
            async for incoming in r:
                received.append(
                    {
                        "trace_id": incoming.metadata.get("trace_id"),
                        "agent_id": incoming.metadata.get("agent_id"),
                        "size": incoming.size_bytes,
                        "file_name": incoming.file_name,
                    }
                )
                await incoming.ack()
                break

    async def go() -> dict[str, object]:
        addr_q: asyncio.Queue[str] = asyncio.Queue()
        consumer_task = asyncio.create_task(consumer(addr_q))
        addr = await asyncio.wait_for(addr_q.get(), timeout=5.0)
        outcome = await producer(addr)
        await asyncio.wait_for(consumer_task, timeout=5.0)
        return outcome

    outcome = asyncio.run(go())

    assert outcome["status"] in ("acked", "completed")
    assert len(received) == 1
    assert received[0]["trace_id"] == "demo-trace"
    assert received[0]["agent_id"] == "producer"
    assert received[0]["size"] == src.stat().st_size
    assert received[0]["file_name"] == "report.csv"
