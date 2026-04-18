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

The MSRV is **Rust 1.82**. CI verifies this on every PR.

## Project layout

| Crate                | Purpose                                                  |
| -------------------- | -------------------------------------------------------- |
| `aerosync`           | The user-facing CLI binary.                              |
| `aerosync-core`      | Transfer engine, resume store, mDNS discovery, receiver. |
| `aerosync-protocols` | HTTP / QUIC / S3 / FTP transports + auto-negotiation.    |
| `aerosync-mcp`       | MCP server exposing AeroSync as 8 tools for AI agents.   |
| `aerosync-ui`        | (Scaffold) GUI module — currently a placeholder.         |

## Running the test suite

```bash
cargo test --workspace --all-targets    # everything
cargo test -p aerosync-core             # one crate
cargo test -p aerosync-protocols --test pipeline   # one integration test
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
