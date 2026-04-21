# Installing AeroSync

There are five supported channels. Pick whichever fits your environment.

## 1. One-line installer (macOS / Linux, x86_64 + arm64)

```bash
curl -fsSL https://raw.githubusercontent.com/TechVerseOdyssey/AeroSync/master/install.sh | bash
```

What it does:

1. Detects your OS and CPU.
2. Resolves the latest GitHub Release tag.
3. Downloads the matching archive **and** its `.sha256`, then verifies.
4. Installs `aerosync` and `aerosync-mcp` into `$HOME/.local/bin`
   (no sudo) or, if that's not writable, `/usr/local/bin` with `sudo`.

Pin a specific version or change the prefix:

```bash
AEROSYNC_VERSION=v0.2.0 AEROSYNC_PREFIX=/opt/aerosync/bin bash install.sh
```

## 2. Homebrew (macOS / Linux)

> Requires the tap repo `TechVerseOdyssey/homebrew-aerosync` to be
> published. See [`homebrew.rb`](install/homebrew.rb) for the formula
> template that is copied there on each release.

```bash
brew tap TechVerseOdyssey/aerosync
brew install aerosync
```

## 3. Cargo (any platform with Rust ≥ 1.89)

```bash
cargo install --locked aerosync          # CLI only
cargo install --locked aerosync-mcp      # MCP server
```

Builds from source — no prebuilt binary, no checksum verification, but
it works on any architecture supported by Rust.

## 4. Prebuilt archives (manual)

Grab a tarball/zip from
[GitHub Releases](https://github.com/TechVerseOdyssey/AeroSync/releases),
verify the `.sha256`, extract, and place `aerosync` / `aerosync-mcp` on
your `$PATH`.

Replace `<TAG>` with the release you want (e.g. `v0.3.0`) and
`<VER>` with the matching bare version (e.g. `0.3.0`):

```bash
curl -LO https://github.com/TechVerseOdyssey/AeroSync/releases/download/<TAG>/aerosync-<VER>-aarch64-apple-darwin.tar.gz
curl -LO https://github.com/TechVerseOdyssey/AeroSync/releases/download/<TAG>/aerosync-<VER>-aarch64-apple-darwin.tar.gz.sha256
shasum -a 256 -c aerosync-<VER>-aarch64-apple-darwin.tar.gz.sha256
tar -xzf aerosync-<VER>-aarch64-apple-darwin.tar.gz
sudo mv aerosync-<VER>-aarch64-apple-darwin/aerosync* /usr/local/bin/
```

## 5. From source

```bash
git clone https://github.com/TechVerseOdyssey/AeroSync.git
cd AeroSync
cargo build --release
# binaries: target/release/aerosync, target/release/aerosync-mcp
```

## Verifying the install

```bash
aerosync --version
aerosync-mcp --help    # prints the MCP usage banner
```

## Uninstalling

```bash
rm "$(command -v aerosync)" "$(command -v aerosync-mcp)"
rm -rf ~/.aerosync     # removes config and resume state
```

## Notes for AI agent integrations

After installing, register `aerosync-mcp` with your MCP-compatible
client. The minimum config (Claude Desktop / Claude Code / Cursor) is:

```jsonc
{
  "mcpServers": {
    "aerosync": {
      "command": "aerosync-mcp",
      "env": { "AEROSYNC_MCP_SECRET": "change-me" }
    }
  }
}
```

See [`mcp-integration.md`](mcp-integration.md) for the full tool list,
schemas and runtime environment variables.
