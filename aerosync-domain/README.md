# aerosync-domain

> **Internal crate.** API may break in v0.4 without notice. End users
> consume these types via the root [`aerosync`](https://crates.io/crates/aerosync)
> crate's `pub use` re-exports.

The pure business-logic layer of [AeroSync](https://github.com/TechVerseOdyssey/AeroSync) —
file-transfer types, the `Receipt` state machine, the `TransferSession`
aggregate root, and the `Storage` trait abstractions consumed by
`aerosync-infra`. No networking, no filesystem, no transport-specific
code lives here.

## Status

`v0.3.0-rc1` skeleton — Phase 1 of the
[v0.3.0 refactor plan](https://github.com/TechVerseOdyssey/AeroSync/blob/main/docs/v0.3.0-refactor-plan.md)
ships the crate boundary; subsequent micro-PRs migrate file contents
in via `git mv` so blame survives.

See also: `docs/v0.3.0-frozen-api.md` for the public-API contract this
refactor must respect.

## License

MIT — same as the root `aerosync` crate.
