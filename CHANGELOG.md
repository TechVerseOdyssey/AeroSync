# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added (v0.2.0 — Metadata Envelope, RFC-003)
- **Metadata Envelope (RFC-003)**: every transfer can now carry a
  structured `Metadata` message with system fields (`id`, `from_node`,
  `to_node`, `created_at`, `content_type`, `size_bytes`, `sha256`,
  `file_name`, `protocol`), well-known fields (`trace_id`,
  `conversation_id`, `parent_file_ids`, `expires_at`, `lifecycle`,
  `correlation_id`) and free-form `user_metadata` (`map<string,string>`).
  See [`docs/protocol/metadata-v1.md`](docs/protocol/metadata-v1.md).
- `aerosync::core::metadata::MetadataBuilder` — programmatic builder
  with eager validation: 64 KiB envelope cap, 256 user entries, 128-byte
  keys, 16 KiB values, 64 parent ids, 1024-byte file names, UTF-8
  enforcement, oversized-input typed errors (`MetadataError`).
- `aerosync::core::sniff::sniff_content_type` — automatic content-type
  detection via `infer` (magic bytes) with `mime_guess` (extension)
  fallback; default `application/octet-stream`.
- `TransferEngine::send_with_metadata` — accepts a caller-built
  envelope; system fields (`id`, `from_node`, `created_at`, `sha256`,
  `size_bytes`, `content_type`, `file_name`, `protocol`) are sealed in
  by the engine and cannot be spoofed via `user_metadata`.
- `IncomingFile::metadata()` — receivers can read the envelope on the
  ack channel.
- `HistoryStore` extension — every JSONL record now persists a
  `MetadataJson` mirror of the proto (additive, backward-compatible
  with pre-v0.2.0 records, no `null` written when absent).
- `HistoryStore::query(&HistoryFilter)` — linear-scan filtering by
  `metadata_eq`, `trace_id`, `lifecycle`, `since`, `until` on top of
  existing direction/protocol/success filters. Indexed lookup is
  deferred to v0.2.1 (SQLite migration).
- CLI `aerosync send` flags: `--meta key=value` (repeatable),
  `--trace-id`, `--conversation-id`, `--parent`, `--lifecycle`,
  `--correlation-id`, `--content-type`.
- CLI `aerosync history` flags: `--meta key=value` (repeatable),
  `--trace-id`, `--lifecycle`, `--since`, `--until`. Output now
  includes `trace_id` and a compact `user_metadata` summary.
- MCP tools `send_file` / `send_directory`: optional `metadata`,
  `trace_id`, `conversation_id`, `parent_file_ids`, `lifecycle`,
  `correlation_id`, `content_type` parameters.
- MCP tool `list_history`: optional `metadata_filter`, `trace_id`,
  `lifecycle`, `since`, `until` parameters; response now includes the
  full `MetadataJson` per record and a top-level `trace_id` shortcut.
- Reference doc `docs/protocol/metadata-v1.md` and 5 new
  metadata-focused integration tests in `tests/metadata_e2e.rs`
  (roundtrip, persistence, lineage, oversize rejection, system-field
  anti-spoofing).

### Notes & known limitations (v0.2.0)
- The `Metadata` message is **not yet wired into the QUIC
  `TransferStart` frame** end-to-end; the envelope is sealed in the
  sender engine and surfaced via `Acked.metadata` and the JSONL
  history. Full `TransferStart` integration ships with the upcoming
  `quic_receipt` task.
- `HistoryStore::query` is an `O(N)` scan over the JSONL file. It is
  fine for thousands of records but will be replaced by the SQLite
  backend in v0.2.1.
- `expires_at` is a **hint only** — no enforcement in v0.2.
- The Python SDK does not yet expose a `metadata=` keyword argument
  (deferred to `w5-py-phase1a`); raw `metadata_json` is exposed in the
  meantime.

### Changed (v0.2.0 prep — TLS stack modernization)
- Upgraded QUIC transport to `quinn 0.11` (from 0.10), which required
  reworking `ClientConfig::new` to wrap rustls via
  `quinn::crypto::rustls::QuicClientConfig` and making
  `SendStream::finish()` synchronous at every call site.
  Closes **RUSTSEC-2026-0037** (Quinn DoS).
- Upgraded `rustls 0.21 → 0.23`. `PinnedCertVerifier` was rewritten to
  the new `rustls::client::danger::ServerCertVerifier` trait (requires
  explicit `supported_verify_schemes`, `verify_tls12_signature`,
  `verify_tls13_signature` implementations) and uses
  `rustls_pki_types::CertificateDer<'_>` / `ServerName<'_>` /
  `UnixTime` in place of the legacy rustls types. The rustls process-
  wide crypto provider (`ring`) is installed lazily on first QUIC use.
  Closes **RUSTSEC-2025-0009** (ring AES panic).
- Upgraded `rustls-pemfile 1 → 2` and migrated `load_tls_from_pem`
  to the iterator-based parsers that return
  `rustls_pki_types::{CertificateDer, PrivateKeyDer}` directly.
- Upgraded `rcgen 0.11 → 0.13`. Self-signed cert generation now uses
  the infallible `CertifiedKey::cert.pem()` /
  `key_pair.serialize_pem()` accessors.
- Upgraded `reqwest 0.11 → 0.12` (pulled in by the new rustls chain;
  no code changes required on our side).
- `deny.toml` ignore list dropped from 7 to 5 entries; reasons updated
  to point to the RFC-002 follow-up that will remove warp.
- `rustls-native-certs` removed from `Cargo.toml` — it was declared but
  never imported in any source file.

### Added
- LICENSE (MIT) and full crate metadata so each crate can be published
  to crates.io.
- English `README.md`; the original Chinese version is preserved as
  `README.zh-CN.md`.
- `SECURITY.md`, `CONTRIBUTING.md`, this changelog, plus issue and PR
  templates.
- Multi-platform release workflow (`.github/workflows/release.yml`):
  tag-triggered builds for macOS (x86_64 + aarch64), Linux (x86_64 +
  aarch64 musl) and Windows (x86_64), uploaded to GitHub Releases.
- `cargo-deny` and `cargo-audit` jobs in CI, plus an MSRV check
  (Rust 1.89).
- `install.sh` one-line installer and a Homebrew formula template under
  `docs/install/`.
- README comparison table vs `scp` / `rsync` / `croc` / `rclone` and a
  dedicated MCP section.

### Changed
- `aerosync-mcp`: every JSON Schema field description for the 8 tools
  is now in English so AI agents (Claude, GPT, Cursor, …) understand
  parameter intent on the first call.
- `get_transfer_status`: the "task not found" error now reports the
  actual configured TTL and points at `AEROSYNC_MCP_TASK_TTL_SECS`.

### Fixed
- (P1) `send_file` now records `resume_json_path` in SQLite, so chunked
  transfers can be resumed after the MCP server is restarted.
- (P2) Renamed `_auth_token` to `mcp_auth_token` across all MCP tools;
  the old name keeps working via `serde(alias)` for backward
  compatibility.
- (P3a) Unified transfer-timeout (1 h default) and task-TTL (24 h
  default); both are now configurable via `AEROSYNC_MCP_TRANSFER_TIMEOUT_SECS`
  and `AEROSYNC_MCP_TASK_TTL_SECS`.

## [0.1.0] - TBD

Initial public release.

[Unreleased]: https://github.com/TechVerseOdyssey/AeroSync/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/TechVerseOdyssey/AeroSync/releases/tag/v0.1.0
