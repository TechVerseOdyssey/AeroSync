# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

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
