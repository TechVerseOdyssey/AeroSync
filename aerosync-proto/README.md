# aerosync-proto

Generated Rust types for the [AeroSync](../README.md) wire protocol —
the binary format spoken under ALPN `aerosync/1`.

This crate exists so that every component of AeroSync (the core
library, the MCP shim, the future Python SDK, and any third-party
client) can share **one** typed view of the wire format.

## Status

`v0.1.x`: implementation in progress, breaking changes allowed.
The crate is **not** published to crates.io yet (`publish = false` in
`Cargo.toml`); it ships alongside `aerosync` once we cut v0.2.0, at
which point the wire is frozen and only additive field changes are
permitted.

## Layout

| File                                | Role                                  |
| ----------------------------------- | ------------------------------------- |
| `proto/aerosync/wire/v1.proto`      | Source of truth — see RFC-002 §6.3.   |
| `build.rs`                          | Runs `prost-build` against the proto. |
| `src/lib.rs`                        | Thin re-export of generated types.    |

## Specification

The canonical specification of every message and field number is
[RFC-002 — Receipt Protocol](../docs/rfcs/RFC-002-receipt-protocol.md),
particularly:

- §5 wire format
- §6.1 stream layout
- §6.3 ALPN + capabilities

The `Metadata` message referenced by `TransferStart.metadata` (field
5) and `Acked.metadata` (field 1) is specified in
[RFC-003 — Metadata Envelope](../docs/rfcs/RFC-003-metadata-envelope.md)
and lands in Week 4 of the v0.2.0 plan; until then the field numbers
are reserved.

## Regenerating the bindings

You don't have to do anything: `cargo build -p aerosync-proto` invokes
`build.rs`, which runs `prost-build` against `proto/aerosync/wire/v1.proto`
on every build (with `cargo:rerun-if-changed` set on the proto file
and the build script itself).

A vendored `protoc` ships via
[`protoc-bin-vendored`](https://crates.io/crates/protoc-bin-vendored)
so contributors do **not** need to install Protocol Buffers
system-wide.

## Why `bytes::Bytes` for `bytes` fields?

`build.rs` invokes `prost_build::Config::bytes(["."])`, which makes
every protobuf `bytes` field map to `bytes::Bytes` instead of
`Vec<u8>`. `Bytes` is `Arc`-backed and cheap to clone, which matters
when frames pass between async tasks.

## Feature flags

None. The crate is a thin schema crate; downstream features live in
`aerosync` itself.
