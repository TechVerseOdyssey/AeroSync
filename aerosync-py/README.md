# aerosync (Python SDK)

In-process Python bindings for the [AeroSync](https://github.com/TechVerseOdyssey/AeroSync)
file-transfer engine. Lets a Python application send and receive files
over QUIC / HTTP without spawning subprocesses or speaking MCP, while
keeping the engine's resume / receipts / metadata-envelope semantics
intact.

## Install

```bash
pip install aerosync
```

We ship a single `abi3-py39` wheel per OS — one wheel works on
CPython 3.9, 3.10, 3.11, 3.12, … macOS (universal2) / Linux glibc
(x86_64 + aarch64) / Linux musl (x86_64) / Windows (x86_64).
Bumping the abi3 floor would be a SemVer-breaking change for the SDK.

## Quickstart

```python
import asyncio
import aerosync

async def main():
    async with aerosync.client() as c:
        receipt = await c.send(
            "report.csv",
            to="127.0.0.1:7788",
            metadata={"trace_id": "run-123", "agent_id": "scraper"},
        )
        outcome = await receipt.processed()
        print(outcome["status"])  # → "acked"

asyncio.run(main())
```

Pair it with a receiver loop on the other side; the verbatim
end-to-end round-trip (sender + receiver async-with) lives in the
[top-level README](../README.md#python-sdk-quickstart-v030-rc1) and
is exercised by `tests/test_killer_demo.py` — if that test fails,
both READMEs are a lie.

## Reference

- Design rationale and public-API contract: [RFC-001 Python SDK](../docs/rfcs/RFC-001-python-sdk.md).
- Wire metadata schema (shared with the Rust crate): [`docs/protocol/metadata-v1.md`](../docs/protocol/metadata-v1.md).
- Release / publish dance for maintainers: [`docs/python/RELEASE-CHECKLIST.md`](../docs/python/RELEASE-CHECKLIST.md).
- Type stubs (PEP 561 `py.typed`, `mypy --strict` clean): [`python/aerosync/_native.pyi`](python/aerosync/_native.pyi).

Current: **v0.3.0-rc1** (matches the workspace `[package].version` in `Cargo.toml`).
