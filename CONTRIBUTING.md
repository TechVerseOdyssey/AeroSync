# Contributing to AeroSync

Thank you for considering a contribution! AeroSync is a small, focused
project — issues and PRs are very welcome.

## Quick start

```bash
git clone https://github.com/TechVerseOdyssey/AeroSync.git
cd AeroSync
cargo build --workspace --all-targets
cargo test  --workspace --all-targets
```

The MSRV is **Rust 1.89**. CI verifies this on every PR.

## Project layout

| Crate          | Purpose                                                                                                                                                                                                                            |
| -------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `aerosync`     | Library + `aerosync` CLI binary. The library is split into two sub-modules: `aerosync::core` (transfer engine, resume store, mDNS, auth, file receiver) and `aerosync::protocols` (HTTP / QUIC / S3 / FTP + the auto-adapter).     |
| `aerosync-mcp` | Thin binary wrapping the `aerosync` library and exposing it as an MCP server (8 tools) over stdio. Depends on `aerosync` only.                                                                                                     |

## Running the test suite

```bash
cargo test --workspace --all-targets             # everything
cargo test -p aerosync                           # the lib + CLI crate
cargo test -p aerosync --test protocols_pipeline # one integration test
cargo test -p aerosync-mcp                       # the MCP server crate
```

There are 41+ tests in `aerosync-mcp` alone — please add coverage when
you add or change behaviour.

## Style

- `cargo fmt --all` before committing (CI enforces).
- `cargo clippy --workspace --all-targets -- -D warnings` must pass.
- Avoid adding new dependencies without a clear reason; mention them in
  the PR description.
- Prefer small, focused commits with an [imperative subject line](https://cbea.ms/git-commit/).

## Commit message convention

We loosely follow [Conventional Commits](https://www.conventionalcommits.org):

```
<type>(<scope>): short summary

Optional longer body explaining *why*.
```

Types we use: `feat`, `fix`, `refactor`, `test`, `docs`, `chore`,
`i18n`, `ci`, `build`, `perf`.

## Pull requests

1. Open an issue first for non-trivial changes — saves you rework.
2. Branch from `master`.
3. Make sure `cargo test --workspace` is green locally.
4. PR description should answer: **what** changes, **why**, and **how it
   was tested**.
5. The PR template will guide you through this.

## Reporting security issues

Please **do not** open a public issue. See [`SECURITY.md`](SECURITY.md).

## License

By contributing, you agree that your work is licensed under the
[MIT License](LICENSE).
