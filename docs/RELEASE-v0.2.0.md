# AeroSync v0.2.0 — Release runbook

This is the per-release operator handoff for the v0.2.0 cut. The
in-repo work was prepared by the w9 release-prep subagent on
**2026-04-18** and stops at `git tag` (no remote pushes, no
crates.io publish, no PyPI publish, no GitHub Release creation).
Everything below is the manual sequence the maintainer runs to
ship v0.2.0 to users.

For the per-channel reference (Python wheel matrix, PyPI Trusted
Publisher one-time setup) see also
[`docs/python/RELEASE-CHECKLIST.md`](python/RELEASE-CHECKLIST.md).

---

## Status (as prepared by w9)

- All v0.2.0 work landed locally on `master`, **13 commits ahead
  of `origin/master`** (the 10 feature/CI commits from
  w5–w8 + 3 release-prep commits from w9: version bump, publish
  prerequisites, this runbook).
- All gates green: `cargo check --workspace`, `cargo test
  --workspace` (544 passing, 3 ignored), `cargo clippy
  --all-targets --workspace -- -D warnings`, `cargo fmt --check`,
  `cargo deny --all-features check`, `pytest aerosync-py/tests/`
  (90 passing, 3 documented skips), `mypy --strict
  aerosync-py/python/aerosync`, `ruff check aerosync-py/`,
  `python -c "import yaml; yaml.safe_load(open('.github/workflows/python-release.yml'))"`.
- `cargo publish --dry-run -p aerosync-proto --allow-dirty`:
  PASS (Packaged 8 files, 36.8 KiB; Verified clean). The
  `aerosync` and `aerosync-mcp` dry-runs cannot complete pre-
  release — see "Known dry-run limitation" below.
- CHANGELOG.md `## [v0.2.0]` heading dated **2026-04-18**.
- Versions bumped to `0.2.0` in:
  - `Cargo.toml` (workspace.package.version + the two internal
    workspace.dependencies version reqs)
  - `aerosync-mcp/Cargo.toml` (inherits from workspace)
  - `aerosync-proto/Cargo.toml` (inherits from workspace; also
    flipped from `publish = false` to publishable)
  - `aerosync-py/Cargo.toml` (was already at 0.2.0 from w5)
  - `aerosync-py/pyproject.toml` (was already at 0.2.0 from w5)
- Local annotated tag `v0.2.0` created on `master`.
- **Tag NOT pushed; remote NOT updated; no crates.io / PyPI
  publish has happened.**

### Known dry-run limitation

`cargo publish --dry-run -p aerosync` and `cargo publish --dry-run
-p aerosync-mcp` cannot be fully exercised pre-release: cargo's
package-prepare step performs an index lookup for `aerosync-proto`
BEFORE either packaging or verification, and `aerosync-proto` is
not yet on crates.io. This is a documented cargo limitation, not
a manifest defect. Both crates pass `cargo package --list` (the
tarball would be well-formed), and the workspace itself compiles
clean — they will dry-run-pass once `aerosync-proto` is on the
index, between steps 2a and 2b below. Re-run the dry-runs at that
point if you want belt-and-suspenders validation.

---

## What you (the maintainer) need to do

### 1 · Push commits + tag

```bash
cd /Users/cainliu/codes/AGI/AeroSync

# Sanity-check what you're about to push.
git log --oneline origin/master..HEAD
git tag -l v0.2.0 -n10

# Push the master branch first, then the tag. The tag push
# triggers BOTH the Rust release workflow (.github/workflows/
# release.yml -> 5 platform binaries -> GitHub Release) AND the
# Python wheel-matrix build (.github/workflows/python-release.yml
# -> wheels for the 6 supported platforms + sdist).
git push origin master
git push origin v0.2.0
```

### 2 · Publish Rust crates to crates.io

Order matters: `aerosync-proto` has zero internal deps, then
`aerosync` (depends on `aerosync-proto`), then `aerosync-mcp`
(depends on both `aerosync` and `aerosync-proto`).

```bash
# One-time auth, if you've never published from this machine:
cargo login   # paste a crates.io API token

# 2a. aerosync-proto first.
cargo publish -p aerosync-proto

# Wait ~30s for crates.io's index to refresh, OR poll
# https://crates.io/crates/aerosync-proto until v0.2.0 is listed.

# 2b. aerosync next.
cargo publish -p aerosync

# Wait ~30s again.

# 2c. aerosync-mcp last.
cargo publish -p aerosync-mcp
```

Verify on crates.io after each:
- <https://crates.io/crates/aerosync-proto>
- <https://crates.io/crates/aerosync>
- <https://crates.io/crates/aerosync-mcp>

If you want extra safety, run
`cargo publish --dry-run -p aerosync --allow-dirty` between 2a
and 2b (it'll succeed once `aerosync-proto` is on the index),
and the same for `aerosync-mcp` between 2b and 2c.

### 3 · Publish the Python wheel via the GitHub workflow

The wheel matrix (`.github/workflows/python-release.yml`) starts
automatically when the `v0.2.0` tag is pushed in step 1. It
builds wheels for: macOS x86_64 + aarch64, Linux glibc x86_64 +
aarch64 (manylinux_2_17), Linux musl x86_64 (musllinux_1_1),
Windows x86_64. abi3-py39 collapses 30 wheels (5 Pythons x 6
platforms) down to 6.

Verify in **Actions -> Python Release**:
- All 6 `wheels-*` matrix jobs green.
- `sdist` job green.
- Tag-push alone does NOT publish to PyPI — the publish job
  fires when the GitHub Release is **published** (step 4) OR on
  a `workflow_dispatch` with `target=pypi`.

**First-release prerequisite.** If this is the very first PyPI
release for `aerosync` and the Trusted Publisher hasn't been set
up yet, the publish step will fail. Run through
[`docs/python/RELEASE-CHECKLIST.md`](python/RELEASE-CHECKLIST.md)
sections "1 · Reserve the `aerosync` name" through "4 · Create
the GitHub Environments" first — they require admin access to
PyPI / TestPyPI / GitHub repo settings and were intentionally
left manual.

### 4 · Create the GitHub Release

The Rust release workflow (`.github/workflows/release.yml`)
already attaches the 5 platform binaries to the Release that the
tag push creates. To finalize:

```bash
# Option A: gh CLI, generates release notes from CHANGELOG.
gh release create v0.2.0 \
    --title "AeroSync v0.2.0 — Receipt protocol + Python SDK + Metadata envelope" \
    --notes-file <(awk '/^## \[v0.2.0\]/,/^## \[/{ if(/^## \[/ && !/^## \[v0.2.0\]/) exit; print }' CHANGELOG.md)
```

Option B: edit in the GitHub Releases UI, paste the v0.2.0
section of CHANGELOG.md into the body, click Publish.

**Publishing the Release is what triggers the
`publish -> PyPI` job** in `python-release.yml`. If you set
required reviewers on the `pypi` environment per the one-time
checklist, you'll get a "review pending" notification — approve
it. Watch the job until the wheels + sdist are live at
<https://pypi.org/project/aerosync/>.

### 5 · Verify install (real PyPI / crates.io, not local dev)

```bash
# Rust
cargo install aerosync             # installs from crates.io
aerosync --version                  # should print 0.2.0
aerosync-mcp --version              # also installable: cargo install aerosync-mcp

# Python (after the wheel CI completes and PyPI publish lands)
python -m venv /tmp/aerosync-pypi
source /tmp/aerosync-pypi/bin/activate
pip install aerosync
python -c "import aerosync; print(aerosync.version())"   # 0.2.0
```

### 6 · Smoke-test the Python killer demo

Run the exact code from `README.md`'s "Python SDK quickstart"
section in two terminal windows (one receiver, one sender) on
real installed wheels (not local `maturin develop`). Verify the
end-to-end send + receive completes and the `Receipt.processed()`
returns the expected `Outcome`.

Honest expectation: per the v0.2.0 known limitations
(`w3c-quic-receipt-wiring`), cross-process QUIC senders /
receivers must use the HTTP SSE control plane to observe
receipt state. Same-process / Python-in-process flows work
out of the box. The demo in README walks the SSE path; it will
work on real wheels.

---

## Known limitations shipped with v0.2.0

(Full list lives in CHANGELOG.md under "Known limitations
(deferred to v0.2.1)". Highlights for the release announcement:)

- **`w2c-resume-sqlite`** — `ResumeStore` is JSONL-based;
  in-flight receipts may be lost on crash. SQLite + WAL
  migration is the v0.2.1 trigger.
- **`w3c-quic-receipt-wiring`** — QUIC bidi receipt stream
  codec exists (`src/protocols/quic_receipt.rs`) but is not
  auto-opened by the live transport. Cross-process QUIC
  sender/receiver pairs must use the HTTP SSE control plane
  (`/v1/receipts/:id/events`) to observe receipt state.
  Same-process flows are unaffected. v0.2.1.
- `HistoryStore::query` is `O(N)` linear scan over the JSONL
  file. SQLite + JSON1 indices in v0.2.1.
- `expires_at` is a hint only — no enforcement in v0.2.
- Windows-on-ARM and musllinux-aarch64 wheels are deferred.

---

## Rollback procedure (if v0.2.0 has a critical bug)

```bash
# crates.io: yank the published versions. Yanking does NOT
# delete; it marks them as un-installable for new projects but
# preserves them for existing Cargo.lock files.
cargo yank --version 0.2.0 aerosync-mcp
cargo yank --version 0.2.0 aerosync
cargo yank --version 0.2.0 aerosync-proto

# PyPI: cannot delete a published version. File a yank request
# via the PyPI UI:
# https://pypi.org/manage/project/aerosync/release/0.2.0/

# GitHub Release: edit -> "Mark as pre-release" or delete.
# The git tag itself is immutable in users' clones; the new
# fix ships as v0.2.1.

# Then prepare v0.2.1 with the fix, following this same
# runbook from step 1.
```

---

## What was deliberately NOT done by w9

These are scope-bound exclusions, not oversights. If you want
any of them done before shipping, do them now (re-run the gates
afterwards) — the rest of this runbook is unaffected.

- No `git push` of any kind. The maintainer pushes.
- No actual `cargo publish` (only `--dry-run` for
  `aerosync-proto`).
- No actual PyPI / TestPyPI upload.
- No GitHub Release created or workflow dispatched.
- RFC documents (`docs/rfcs/RFC-001`, `RFC-002`, `RFC-003`)
  untouched — they are frozen for v0.2.0.
- `w2c-resume-sqlite` and `w3c-quic-receipt-wiring`
  intentionally remain deferred to v0.2.1.

---

Generated by the w9 release-prep subagent. Date: 2026-04-18.
