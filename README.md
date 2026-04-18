# AeroSync

[![CI](https://github.com/TechVerseOdyssey/AeroSync/actions/workflows/rust.yml/badge.svg)](https://github.com/TechVerseOdyssey/AeroSync/actions/workflows/rust.yml)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)
[![MSRV](https://img.shields.io/badge/rust-1.82%2B-orange.svg)](https://www.rust-lang.org)
[![Crates.io](https://img.shields.io/crates/v/aerosync.svg)](https://crates.io/crates/aerosync)

> **AI-native file transfer.** A fast Rust CLI **and** an MCP server — let Claude, ChatGPT, Cursor or your own agent move files between machines as easily as `scp`, with automatic QUIC upgrade, resumable chunked uploads and zero infrastructure.

[简体中文](README.zh-CN.md) · [Architecture](#architecture) · [MCP for AI agents](docs/mcp-integration.md)

---

## Why AeroSync?

| Tool       | Resume | QUIC auto | Built-in receiver | LAN discovery (mDNS) | MCP server for AI agents |
| ---------- | :----: | :-------: | :---------------: | :------------------: | :----------------------: |
| `scp`      |   ✗    |     ✗     |   ssh required    |          ✗           |            ✗             |
| `rsync`    |   ✓    |     ✗     |   ssh required    |          ✗           |            ✗             |
| `croc`     |   ✗    |     ✗     |         ✓         |          ✗           |            ✗             |
| `rclone`   |   ✓    |     ✗     |         ✗         |          ✗           |            ✗             |
| **AeroSync** | **✓**  |   **✓**   |       **✓**       |        **✓**         |          **✓**           |

Designed for the use case nothing else covers cleanly: **one agent on machine A asks another agent on machine B "send me that 30 GB dataset"**, and it just works — over LAN (QUIC, mDNS-discovered) or WAN (HTTP fallback), resumable, with a single binary on each side.

## Features

- **Auto protocol negotiation** — probes the peer for AeroSync; upgrades to QUIC if both sides support it, falls back to HTTP otherwise.
- **Resumable transfers** — 32 MB chunks, state persisted to local JSON, recovers automatically after a crash or `Ctrl-C`.
- **Concurrency tuned to file size** — 16-way for `<1 MB`, 8-way for `<64 MB`, chunked for `>64 MB`.
- **Multi-protocol** — HTTP, QUIC, S3 (incl. MinIO), FTP — all behind one CLI.
- **Recursive directory transfer** with full structure preservation (`--recursive`).
- **End-to-end SHA-256** integrity check.
- **HMAC-SHA256 bearer token** auth.
- **TOML config** with CLI overrides.
- **MCP server** — exposes 8 tools (`send_file`, `send_directory`, `start_receiver`, …) so AI agents can drive transfers natively. See [`docs/mcp-integration.md`](docs/mcp-integration.md).

## Install

### From source (any platform with Rust ≥ 1.82)

```bash
git clone https://github.com/TechVerseOdyssey/AeroSync.git
cd AeroSync
cargo build --release
# binaries: target/release/aerosync, target/release/aerosync-mcp
```

### One-line install (macOS / Linux)

```bash
curl -fsSL https://raw.githubusercontent.com/TechVerseOdyssey/AeroSync/master/install.sh | bash
```

> Other channels (Homebrew, Cargo, prebuilt GitHub Releases, npm wrapper) are tracked in [`docs/install.md`](docs/install.md).

## Quick start

**Receiver** (target machine):

```bash
aerosync receive --port 7788 --save-to ./downloads
```

**Sender** (source machine):

```bash
# Single file (auto-negotiates protocol)
aerosync send ./video.mp4 192.168.1.10:7788

# Recursive directory (preserves structure)
aerosync send ./project/ 192.168.1.10:7788 --recursive

# Force HTTP
aerosync send ./file.zip http://192.168.1.10:7788/upload

# Upload to S3 / MinIO
aerosync send ./data.tar.gz s3://my-bucket/backups/data.tar.gz

# Upload to FTP
aerosync send ./report.pdf ftp://ftpserver:21/uploads/report.pdf
```

## CLI reference

### `aerosync send`

```
aerosync send <SOURCE> <DESTINATION> [OPTIONS]
```

| Option            | Description                                                                | Default        |
| ----------------- | -------------------------------------------------------------------------- | -------------- |
| `<SOURCE>`        | Source file or directory                                                   | —              |
| `<DESTINATION>`   | `host:port`, `http://`, `quic://`, `s3://` or `ftp://`                     | —              |
| `-r, --recursive` | Send a directory recursively                                               | false          |
| `--protocol`      | Force a protocol: `quic` \| `http`                                         | auto-negotiate |
| `--token`         | Auth token                                                                 | —              |
| `--parallel`      | Number of concurrent streams                                               | 4              |
| `--no-verify`     | Skip the SHA-256 check                                                     | false          |
| `--dry-run`       | Print the transfer plan and exit                                           | false          |
| `--no-resume`     | Disable resumable transfer                                                 | false          |

### `aerosync receive`

```
aerosync receive [OPTIONS]
```

| Option         | Description                                | Default     |
| -------------- | ------------------------------------------ | ----------- |
| `--port`       | HTTP listen port                           | 7788        |
| `--quic-port`  | QUIC listen port                           | 7789        |
| `--save-to`    | Directory to save received files           | ./received  |
| `--bind`       | Bind address                               | 0.0.0.0     |
| `--auth-token` | Require this token from senders            | —           |
| `--one-shot`   | Exit after one file is received            | false       |
| `--overwrite`  | Allow overwriting files with the same name | false       |
| `--max-size`   | Maximum file size (bytes)                  | 100 GB      |
| `--http-only`  | HTTP only, disable QUIC                    | false       |

### `aerosync token`

```bash
# Generate a token (24 h validity)
aerosync token generate --hours 24

# Use a custom secret
aerosync token generate --secret my-secret-key

# Verify a token
aerosync token verify <TOKEN> --secret my-secret-key
```

### `aerosync resume`

```bash
aerosync resume list                  # list unfinished transfers
aerosync resume clear <TASK_ID>       # clear one task's resume state
aerosync resume clear-all             # clear them all
```

### `aerosync status`

```bash
aerosync status 192.168.1.10:7788
```

## Configuration

Default path: `~/.aerosync/config.toml` (override with `--config`).

```toml
[transfer]
max_concurrent  = 4    # max concurrent tasks
chunk_size_mb   = 32   # chunk size for resumable upload
retry_attempts  = 3    # max retries per chunk
timeout_seconds = 60   # per-request timeout

[auth]
token = ""             # default auth token

[server]
http_port = 7788
quic_port = 7789
save_to   = "./received"
bind      = "0.0.0.0"
```

CLI flags always override the config file.

## Use with AI agents (MCP)

AeroSync ships an [MCP](https://modelcontextprotocol.io) server (`aerosync-mcp`) that lets any MCP-compatible client — **Claude Desktop, Claude Code, Cursor, ChatGPT (via plugins), Continue.dev** — drive file transfers natively.

```jsonc
// Claude Desktop config (~/Library/Application Support/Claude/claude_desktop_config.json)
{
  "mcpServers": {
    "aerosync": {
      "command": "aerosync-mcp",
      "env": { "AEROSYNC_MCP_SECRET": "change-me" }
    }
  }
}
```

8 tools are exposed: `send_file`, `send_directory`, `start_receiver`, `stop_receiver`, `get_receiver_status`, `get_transfer_status`, `discover_receivers`, `list_history`. Full schemas, runtime envs and security notes: [`docs/mcp-integration.md`](docs/mcp-integration.md).

## Protocol details

### QUIC auto-negotiation

When the destination is `host:port`, AeroSync probes `http://host:port/health` (2 s timeout). If the response carries the header `X-AeroSync: true`, it upgrades to QUIC on `port + 1`; otherwise it falls back to HTTP.

```
host:7788  →  probe  →  AeroSync detected   →  quic://host:7789
                     →  no AeroSync          →  http://host:7788/upload
```

### S3-compatible storage

```bash
aerosync send ./file.tar.gz s3://bucket/prefix/file.tar.gz   # AWS S3
# MinIO: configure s3_config.endpoint = Some("http://minio:9000")
```

### FTP (passive mode)

```bash
aerosync send ./file.csv ftp://ftpserver:21/data/file.csv
```

## Resumable transfers

Files larger than 64 MB automatically use chunked upload (32 MB per chunk). State is stored under `~/.aerosync/.aerosync/<task_id>.json`.

```bash
aerosync send ./large_file.bin 192.168.1.10:7788     # interrupt with Ctrl-C, re-run to resume
aerosync resume list                                  # list unfinished transfers
aerosync send ./large_file.bin 192.168.1.10:7788 --no-resume   # force a fresh upload
```

## Architecture

```
aerosync (CLI)              aerosync-mcp (MCP server for AI agents)
       │                              │
       └──────────────┬───────────────┘
                      ▼
              aerosync-core
              ├── TransferEngine    concurrent workers (FuturesUnordered + Semaphore)
              ├── ProgressMonitor   progress reporting
              ├── ResumeStore       chunked-resume persistence
              ├── FileReceiver      HTTP/QUIC receiver
              └── AuthManager       HMAC-SHA256 tokens
                      │
                      ▼
            aerosync-protocols
            ├── AutoAdapter   protocol routing (auto-negotiation)
            ├── HttpTransfer  HTTP up/down (shared Arc<Client>)
            ├── QuicTransfer  QUIC (quinn + rustls)
            ├── S3Transfer    S3 (AWS SigV4)
            └── FtpTransfer   FTP (suppaftp async)
```

### Concurrency strategy

| File size      | Strategy                       | Concurrency       |
| -------------- | ------------------------------ | ----------------- |
| `< 1 MB`       | high-concurrency batch         | 16                |
| `1 – 64 MB`    | medium-concurrency batch       | 8                 |
| `> 64 MB`      | chunked + resumable upload     | 1 (per-chunk pipe)|

## Development

```bash
cargo test --workspace                    # full test suite
cargo test -p aerosync-core               # core only
cargo test -p aerosync-protocols          # protocols only
cargo test -p aerosync-protocols --test pipeline   # E2E pipeline
cargo build --release                     # release build
```

Contributions are very welcome — see [`CONTRIBUTING.md`](CONTRIBUTING.md) and [`SECURITY.md`](SECURITY.md).

## License

MIT — see [`LICENSE`](LICENSE).
