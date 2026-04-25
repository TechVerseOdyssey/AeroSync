#!/usr/bin/env bash
# Unified Rust quality gate for local and CI usage.
set -euo pipefail

repo_root="$(git rev-parse --show-toplevel)"
cd "$repo_root"

if [ "$#" -eq 0 ]; then
  set -- fmt clippy test
fi

for stage in "$@"; do
  case "$stage" in
    fmt)
      echo "[gate] cargo fmt --all -- --check"
      cargo fmt --all -- --check
      ;;
    clippy)
      echo "[gate] cargo clippy --workspace --all-targets -- -D warnings"
      cargo clippy --workspace --all-targets -- -D warnings
      ;;
    test)
      echo "[gate] cargo test --workspace --all-targets"
      cargo test --workspace --all-targets
      ;;
    *)
      echo "Unknown gate stage: $stage" >&2
      echo "Usage: ./scripts/rust-gate.sh [fmt] [clippy] [test]" >&2
      exit 2
      ;;
  esac
done
