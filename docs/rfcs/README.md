# AeroSync RFCs

Design documents for substantial protocol, API, or architectural
changes. RFCs land here as **Draft** before any code is merged; they
are the contract between design and implementation.

## RFC lifecycle

```
Draft  →  Review  →  Accepted  →  Implemented  →  (Superseded)
```

- **Draft**: under active discussion, may change in any direction.
- **Review**: at least one named reviewer assigned; merging blocked
  on their sign-off plus author + 1 alpha-user sign-off where
  applicable.
- **Accepted**: signed off; implementation may begin.
- **Implemented**: code shipped in the listed target version.
- **Superseded**: replaced by a later RFC; kept for history.

## Index

| #   | Title                       | Status | Target | Depends on |
| --- | --------------------------- | ------ | ------ | ---------- |
| 001 | [Python SDK](RFC-001-python-sdk.md)             | Draft  | v0.2.0 | RFC-002, RFC-003 |
| 002 | [Receipt Protocol](RFC-002-receipt-protocol.md) | Draft  | v0.2.0 | —          |
| 003 | [Metadata Envelope](RFC-003-metadata-envelope.md) | Draft  | v0.2.0 | RFC-002    |
| 004 | [WAN Rendezvous & NAT Traversal](RFC-004-wan-rendezvous.md) | Draft  | v0.3.0 | RFC-002, RFC-003 |

## v0.2.0 milestone summary

The three Draft RFCs above together define everything needed for the
"AI file bus" v0.2 release. v0.2 is the first formally specified
release of AeroSync; the published v0.1.0 binaries are treated as a
closed pre-alpha and are **not** considered as upgrade peers.

| RFC                         | Engineer-days |
| --------------------------- | ------------- |
| RFC-001 Python SDK          | 34.5          |
| RFC-002 Receipt Protocol    | 20.5          |
| RFC-003 Metadata Envelope   | 9.5           |
| Cross-cutting (CHANGELOG, cross-RFC integration smoke, launch) | 4 |
| **Total**                   | **68.5**      |

At 5 focused engineer-days per week:

- Single developer, 100% focus: ~14 weeks.
- 1.5 developers (1 senior Rust + 0.5 Python): ~10 weeks.
- 2 developers in parallel (Rust lane + Python lane): ~7-8 weeks once RFC-002's protobuf scaffolding lands.

These are **work-item sums**, not calendar time — review cycles,
context switching, and design-partner feedback add roughly 30%,
so the practical calendar range is ~9-18 weeks depending on staffing.

### Recommended implementation order (2-developer lane model)

Target timeline: **~8 weeks** with one senior Rust engineer on the
wire-protocol lane and one Python-fluent engineer on the SDK lane.

```
          Rust lane (Receipt + Metadata)           Python lane (SDK)
week 1    RFC-002 #1–#3 protobuf + ALPN skeleton   RFC-001 #1–#2 maturin + tokio bridge
week 2    RFC-002 #4 QUIC receipt stream           RFC-001 #3–#4 send() MVP + send_directory
week 3    RFC-002 #5 HTTP SSE fallback             RFC-001 #5–#6 Receiver + IncomingFile
          RFC-003 #1–#3 metadata wire + integration
week 4    RFC-002 #6–#9 persistence + send API    RFC-001 #7–#13 discover / history / errors / Config / bytes / progress
          RFC-003 #4–#5 sniffing + history indices
week 5    RFC-002 #10–#11 MCP tools + tracing     RFC-001 #14–#16 Receipt + ack/nack + metadata pass-through
          RFC-003 #6–#9 CLI + builder + MCP
week 6    RFC-002 #12 tests (4d)                   RFC-001 #17–#18 type checks + pytest suite
          RFC-003 #10–#11 tests + docs
week 7    Cross-RFC integration smoke (Rust)       RFC-001 #19–#22 wheel matrix + PyPI + docs site + notebooks
week 8    Hardening, release candidate, CHANGELOG, launch blog
```

**Single-developer fallback**: RFC-002 → RFC-003 → RFC-001, sequential,
~14 weeks. Not recommended because RFC-001's Python-specific stack
(PyO3, maturin, asyncio, PyPI) is a poor fit for a Rust-heavy engineer
and vice versa — you pay context-switching tax.

RFC-002 must land first because both RFC-001 and RFC-003 depend on
the protobuf wire format. RFC-003 can start mid-RFC-002 once the
protobuf scaffolding is up (end of week 1). RFC-001's Phase 2 tasks
(#14–#16) unblock once RFC-002's Rust API (`Receipt`, `IncomingFile.ack`)
stabilizes at end of week 4.

## Authoring conventions

- File name: `RFC-NNN-kebab-case-title.md`.
- Required sections: Summary, Goals, Non-goals, Open questions,
  Implementation tasks, Approval.
- All open questions must have a default; the default is what ships
  if the question is never resolved.
- Implementation tasks must be sized in **engineer-days** and small
  enough to fit one GitHub issue each.
