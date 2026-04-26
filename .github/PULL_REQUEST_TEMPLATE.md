<!--
Thanks for the PR! A few quick checks before you hit the button:
-->

## What does this change?

<!-- 1-3 sentences. The "why" matters more than the "what". -->

## How was it tested?

- [ ] `cargo test --workspace --all-targets` is green locally
- [ ] `cargo clippy --workspace --all-targets -- -D warnings` is green
- [ ] `cargo fmt --all -- --check` is green
- [ ] Added or updated tests for the new behaviour
- [ ] Updated documentation (`README.md`, `docs/`, doc comments) where relevant
- [ ] (If the PR touches `wan::` / R2 / rendezvous / `peer@` / `punch_signaling` / R2 error tags) **CI → `R2 WAN readiness (targeted)`** is green; in the job log, confirm **`r2_`** and **`peer_at_destination_without`** steps both pass (see `scripts/ci-r2-wan-readiness.sh`)

## Type of change

- [ ] Bug fix
- [ ] New feature
- [ ] Refactor (no functional change)
- [ ] Documentation
- [ ] Build / CI / packaging

## Related issues

<!-- e.g. Closes #123 -->

## Anything reviewers should pay extra attention to?
