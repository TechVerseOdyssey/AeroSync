# aerosync-mcp

[![crates.io](https://img.shields.io/crates/v/aerosync-mcp.svg)](https://crates.io/crates/aerosync-mcp)
[![Docs.rs](https://docs.rs/aerosync-mcp/badge.svg)](https://docs.rs/aerosync-mcp)
[![License](https://img.shields.io/crates/l/aerosync-mcp.svg)](https://github.com/TechVerseOdyssey/AeroSync/blob/master/LICENSE)

**Model Context Protocol server that turns [AeroSync](https://crates.io/crates/aerosync) into 8 tools for AI agents.**

`aerosync-mcp` is a thin stdio server that exposes AeroSync's file-transfer
engine to any MCP-compatible AI client — Claude Desktop, Cursor, ChatGPT,
Continue, etc. The agent can send files and whole directories, start a
receiver on the local machine, discover peers on the LAN via mDNS, and query
transfer history — all without leaving the chat window.

---

## Quick start

### 1. Install

```bash
cargo install aerosync-mcp
```

Pre-built binaries (macOS / Linux / Windows) are also attached to every
[GitHub Release](https://github.com/TechVerseOdyssey/AeroSync/releases).

### 2. Wire it into your AI client

#### Claude Desktop (`~/Library/Application Support/Claude/claude_desktop_config.json` on macOS)

```json
{
  "mcpServers": {
    "aerosync": {
      "command": "aerosync-mcp"
    }
  }
}
```

#### Cursor (`~/.cursor/mcp.json` — project-level also supported)

```json
{
  "mcpServers": {
    "aerosync": {
      "command": "aerosync-mcp"
    }
  }
}
```

#### Any other MCP client

`aerosync-mcp` speaks MCP over stdio. Point your client at the
`aerosync-mcp` binary with no extra arguments.

### 3. Restart the AI client

The 8 AeroSync tools now show up in the tool picker.

---

## The 8 tools

| Tool                   | What the agent can do                                                                          |
| ---------------------- | ---------------------------------------------------------------------------------------------- |
| `send_file`            | Send a single file to a remote address. Auto-negotiates QUIC or HTTP. Returns a task_id.      |
| `send_directory`       | Recursively send an entire directory, preserving structure. Returns a task_id.                |
| `start_receiver`       | Launch a background receiver server that accepts incoming files into a chosen directory.      |
| `stop_receiver`        | Shut down the running receiver.                                                                |
| `get_receiver_status`  | List files received so far and the receiver's current state.                                   |
| `discover_receivers`   | mDNS scan the local network and return available AeroSync peers with their capabilities.      |
| `list_history`         | Query past transfers (speed, status, errors).                                                  |
| `get_transfer_status`  | Poll a background transfer by task_id.                                                         |

All long-running tools (`send_file`, `send_directory`, `start_receiver`)
return **immediately** with a task or server handle — the agent is never
blocked waiting for a multi-gigabyte transfer to finish. Progress is
observed by polling `get_transfer_status` / `get_receiver_status`.

---

## Example prompts

Once connected, try asking the agent things like:

> "Scan my network for AeroSync peers and send `~/Downloads/report.pdf` to
> the first one you find."

> "Start a receiver in `~/Incoming` and tell me when a file lands."

> "Show me the 10 most recent transfers with their average speed."

The agent will pick the right tools and chain them automatically.

---

## Logging and configuration

- Logs go to **stderr** (stdout is reserved for MCP's JSON-RPC frames).
- Set `RUST_LOG=aerosync=debug,aerosync_mcp=debug` for verbose tracing.
- Transfer history and the auth token store live under `~/.aerosync/` by
  default — same locations as the `aerosync` CLI, so state is shared.

---

## How it relates to the `aerosync` crate

`aerosync-mcp` depends only on the [`aerosync`](https://crates.io/crates/aerosync)
library crate. If you want the same functionality from a terminal instead of
from an AI agent, install `aerosync` directly:

```bash
cargo install aerosync        # CLI
aerosync send file.bin 10.0.0.5:9000
```

Both binaries read from and write to the same `~/.aerosync/` state, so you
can mix-and-match — e.g. start a receiver from Claude and check its status
from the CLI.

---

## License

MIT — see [`LICENSE`](https://github.com/TechVerseOdyssey/AeroSync/blob/master/LICENSE) in the repository root.
