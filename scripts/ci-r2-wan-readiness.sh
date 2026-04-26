#!/usr/bin/env bash
# Targeted R2 (RFC-004) gate for CI: the minimal feature slice that compiles
# `AutoAdapter` + `punch_signaling` + `wan::rendezvous` together.
#
# - The main "Test" job still runs the full `cargo test --workspace` matrix; this
#   job is **not** a second copy of the whole tree — it exists so R2 does not
#   silently rot when default features or optional deps change.
# - We use two filters (substring match, see `cargo test --help`):
#   1) `r2_`  — adapter R2-tagged cases, `punch_signaling` timeout tests, `error_advice` R2 tests
#   2) `peer_at_destination_without`  — the NO_TOKEN path (name has no `r2_` substring)
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

CARGO_TEST=(cargo test -p aerosync --lib --no-default-features --features http,quic,wan-rendezvous)

echo "==> R2: tests matching 'r2_' (adapter, punch_signaling, error advice)"
"${CARGO_TEST[@]}" r2_ --verbose

echo "==> R2: [R2_NO_TOKEN] path (peer@… without r2_ in the function name)"
"${CARGO_TEST[@]}" peer_at_destination_without --verbose
