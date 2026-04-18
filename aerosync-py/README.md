# aerosync (Python SDK)

In-process Python bindings for the [AeroSync](https://github.com/TechVerseOdyssey/AeroSync)
file transfer engine. Lets a Python application send and receive files
via QUIC / HTTP without spawning subprocesses or speaking MCP.

> Status: **alpha** — w5 of v0.2.0. Phase 1A scaffold (this commit)
> covers the workspace member, tokio runtime bridge, and the public
> API shells. See [RFC-001](../docs/rfcs/RFC-001-python-sdk.md) for
> the full design and the v0.2.0 task table.

## Install (from source)

Wheels for `pip install aerosync` ship in w7 (RFC-001 task #19). Until
then, build locally with [maturin](https://www.maturin.rs/):

```bash
pip install maturin pytest
maturin develop -m aerosync-py/Cargo.toml
pytest aerosync-py/tests/
```

You will need a working Rust toolchain (1.89+) and Python 3.9+.

## Usage

```python
import aerosync

async def producer():
    async with aerosync.client() as c:
        receipt = await c.send("output.csv", to="data-cleaner")
        await receipt.processed()

async def consumer():
    async with aerosync.receiver(name="data-cleaner") as r:
        async for incoming in r:
            print(incoming.path)
            # await incoming.ack()    # w6
```

## What is in this commit (Phase 1A — w5)

| RFC-001 task | Status |
| ------------ | ------ |
| #1 scaffold + maturin + pyo3 hello-world | done |
| #2 pyo3-async-runtimes tokio bridge      | done |
| #3 `Client.send()` MVP                   | Group B (this week) |
| #4 `Client.send_directory()`             | Group B (this week) |
| #5 `Receiver` + async iterator           | Group C (this week) |
| #6 `IncomingFile` (no ack/nack yet)      | Group C (this week) |
| #7 `aerosync.discover()` mDNS wrapper    | Group C (this week) |
| #8 records (`Peer`/`Progress`/`HistoryEntry`) | Group C (this week) |

Tasks #9 (`Client.history()`), #10 (custom exception hierarchy),
#11 (`Config`), #12 (bytes / `BinaryIO` sources), #13 (`on_progress`),
#14 (full `Receipt` state machine), #15 (`IncomingFile.ack/nack`), and
#16 (metadata pass-through) are scheduled for w6 (Phase 1B / 2). Tasks
#17–#22 (mypy / pytest matrix / wheel CI / PyPI / docs / notebooks)
land in w7 + w8 (Phase 3).

## Open-questions sign-off (RFC-001 §11)

| # | Question | Answer accepted in w5 |
| - | -------- | --------------------- |
| 1 | abi3 vs full Python matrix | `abi3-py39` — one wheel per OS |
| 2 | Module name layout | `aerosync._native` (Rust) re-exported by `aerosync` (public) |
| 3 | tokio runtime sharing | one shared runtime per process, `OnceCell` |
| 4 | Pydantic dependency | no — `@dataclass(slots=True)` |
| 5 | sync wrapper | no — document `asyncio.run()` recipe |
