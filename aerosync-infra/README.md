# aerosync-infra

> **Internal crate.** API may break in v0.4 without notice. End users
> consume these symbols via the root [`aerosync`](https://crates.io/crates/aerosync)
> crate's `pub use` re-exports.

The infrastructure layer of [AeroSync](https://github.com/TechVerseOdyssey/AeroSync) —
concrete IO-bound impls of the trait abstractions defined in
`aerosync-domain`. Owns:

- The rustls crypto provider bootstrap (`ensure_rustls_provider_installed`)
- The audit-log JSONL appender
- JSON / JSONL persistence for resume state and history records
- TOML config loading

## Status

`v0.3.0-rc1` skeleton — see
[v0.3.0 refactor plan](https://github.com/TechVerseOdyssey/AeroSync/blob/main/docs/v0.3.0-refactor-plan.md)
Phase 1.

## License

MIT — same as the root `aerosync` crate.
