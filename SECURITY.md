# Security policy

## Supported versions

AeroSync is pre-1.0; only the latest minor release on `master` receives
security fixes.

| Version | Supported          |
| ------- | ------------------ |
| 0.1.x   | :white_check_mark: |

## Reporting a vulnerability

Please **do not** open public GitHub issues for security problems.

Send a private report to **tiancheng-liu@qq.com** with:

- A description of the issue and the affected component (`aerosync`,
  `aerosync-core`, `aerosync-protocols`, `aerosync-mcp`).
- Steps to reproduce or a minimal proof-of-concept.
- Versions and platforms involved.

You should receive an acknowledgment within **72 hours**. We aim to ship a
fix or mitigation within **14 days** for high-severity issues.

If you'd like, we'll credit you in `CHANGELOG.md` once the fix is released.

## Threat model

AeroSync exposes three surfaces with different trust assumptions:

1. **CLI receiver (`aerosync receive`).** Network-facing. Must be deployed
   behind a firewall or with `--auth-token`. Without a token, anyone who
   can reach the port may upload files up to `--max-size`.
2. **MCP server (`aerosync-mcp`).** Speaks MCP over stdio. The local
   process is trusted; any process that can spawn `aerosync-mcp` can call
   every tool. Set `AEROSYNC_MCP_SECRET` and pass `mcp_auth_token` to
   gate calls when running on a multi-user box.
3. **Resume state (`~/.aerosync/.aerosync/*.json`).** Stores file paths
   and chunk offsets. Treat as user-private; do not commit to VCS.

## Cryptography

- TLS for HTTPS / QUIC uses [`rustls`](https://github.com/rustls/rustls).
- Bearer tokens are HMAC-SHA256 with a user-supplied secret.
- File integrity is verified end-to-end with SHA-256 unless `--no-verify`
  is passed.

## Out of scope

- Vulnerabilities in third-party crates — please report upstream and CC us.
- Denial-of-service that requires already having authenticated network
  access.
- Self-inflicted misconfiguration (e.g. running `aerosync receive` as
  root on a public IP without `--auth-token`).
