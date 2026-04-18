# Release checklist

A concrete, runnable checklist for cutting a new AeroSync release. Most
of the work is automated (release workflow, multi-platform CI); what's
left below is the human glue.

## Per-release (every `vX.Y.Z`)

### 1. Code freeze

- [ ] `master` is green on all CI jobs (`test` × 3 OS, `fmt`, `clippy`,
      `msrv`, `audit`, `deny`).
- [ ] Run locally one more time:

      cargo fmt --all -- --check
      cargo clippy --workspace --all-targets -- -D warnings
      cargo test --workspace --all-targets

### 2. Bump version

- [ ] Update `version` in the root `[workspace.package]` of `Cargo.toml`.
- [ ] Move the `## [Unreleased]` block in `CHANGELOG.md` under a new
      `## [X.Y.Z] - YYYY-MM-DD` heading; add a fresh empty `Unreleased`
      section.
- [ ] Update the comparison links at the bottom of `CHANGELOG.md`.
- [ ] `cargo update -w` (refresh `Cargo.lock`).
- [ ] Commit: `chore(release): vX.Y.Z`.

### 3. Tag and push

```bash
git tag vX.Y.Z -m "vX.Y.Z"
git push origin master --tags
```

This triggers `.github/workflows/release.yml` which builds 5 archives,
uploads them to a GitHub Release, and publishes auto-generated notes.

### 4. crates.io publish (in dependency order)

> Requires a `cargo login` token with publish rights.

```bash
cargo publish -p aerosync-core
cargo publish -p aerosync-protocols   # depends on -core
cargo publish -p aerosync-mcp         # depends on -core + -protocols
cargo publish -p aerosync             # depends on all of the above
```

Wait ~30 s between each — crates.io needs a moment to index.

### 5. Homebrew tap update

> One-time setup: create the `TechVerseOdyssey/homebrew-aerosync` repo
> with a `Formula/aerosync.rb` based on `docs/install/homebrew.rb`.

For each release:

- [ ] Compute SHA-256 of each `*.tar.gz` from the release.
- [ ] Update `version`, the four `url` lines and the four `sha256` lines
      in the tap's `Formula/aerosync.rb`.
- [ ] Commit and push to the tap repo.

### 6. Smoke-test the published artifacts

```bash
# CLI
curl -fsSL https://raw.githubusercontent.com/TechVerseOdyssey/AeroSync/master/install.sh | bash
aerosync --version

# Cargo
cargo install --locked aerosync
aerosync --version

# Homebrew
brew tap TechVerseOdyssey/aerosync
brew install aerosync
aerosync --version
```

### 7. Announce

- [ ] Pin a release thread in GitHub Discussions.
- [ ] Post in the Anthropic MCP Directory submission (one-time per
      project; future versions auto-update).
- [ ] Post on r/rust (text post, not link), HN ("Show HN: AeroSync …"),
      and any relevant AI-agent communities (LangChain, LlamaIndex).

## One-time setup (already done or needs doing once)

- [x] LICENSE (MIT)
- [x] `[workspace.package]` metadata for crates.io
- [x] English README
- [x] CI: matrix test + fmt + clippy + msrv + audit + deny
- [x] Release workflow (multi-platform tag-triggered builds)
- [x] `install.sh`, Homebrew formula template, install docs
- [ ] **Reserve crates.io names** (publish v0.1.0 — see step 4 above).
- [ ] **Create the `homebrew-aerosync` tap repo.**
- [ ] **Submit to the Anthropic MCP Directory**:
      <https://github.com/modelcontextprotocol/servers> — open a PR that
      adds an entry pointing at this repo.
- [ ] **(Optional) Apply for a npm package wrapper** so JS-ecosystem
      users can `npx aerosync send …`. The wrapper just calls the
      installed binary or downloads it on first run.
- [ ] **(Optional) macOS notarization / Windows code signing** if
      Gatekeeper / SmartScreen warnings become a real adoption blocker.

## Post-mortem

After each release, spend 10 minutes:

- [ ] Update `docs/release-checklist.md` with any rough edges hit.
- [ ] Update `CHANGELOG.md` `Unreleased` section with anything you
      already know is coming next.
