#!/usr/bin/env bash
# Build release binaries for the *current* host and package a tarball + SHA-256
# matching GitHub Release layout (install.sh / one-line installer compatible).
#
#   ./scripts/package-local-release.sh
#   AEROSYNC_VERSION_TAG=v0.3.0-rc1 ./scripts/package-local-release.sh   # override folder/archive name
#
# Output: dist/aerosync-<ver>-<rustc-host-triple>.tar.gz (+ .sha256)
# Does not run on Windows (use CI or build .zip manually there).
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

if ! command -v cargo >/dev/null 2>&1; then
  echo "error: cargo not found" >&2
  exit 1
fi

# Version: [workspace.package] in Cargo.toml (same for all workspace crates)
VER_CORE=$(awk '/^\[workspace.package\]/ {w=1} w && /^version = / { gsub(/"/, "", $3); print $3; exit }' "${ROOT}/Cargo.toml")
if [ -z "${VER_CORE}" ]; then
  echo "error: could not read workspace version from Cargo.toml" >&2
  exit 1
fi

# rustc host triple (must match a column in .github/workflows/release.yml for CI-built hosts)
TARGET=$(rustc -vV | sed -n 's/^host: //p')
if [ -z "${TARGET}" ]; then
  echo "error: could not read host triple" >&2
  exit 1
fi

TAG_NAME="${AEROSYNC_VERSION_TAG:-v${VER_CORE}}"
VERSION_NO_V="${TAG_NAME#v}"
STAGE="aerosync-${VERSION_NO_V}-${TARGET}"
OUT="dist"

echo "==> version (crate)  = ${VER_CORE}"
echo "==> archive prefix   = ${STAGE} (set AEROSYNC_VERSION_TAG to match your Git tag, e.g. v0.3.0)"
echo "==> target (host)    = ${TARGET}"
echo "==> building release: aerosync, aerosync-mcp"

mkdir -p "${OUT}"
cargo build --release -p aerosync -p aerosync-mcp

rm -rf "${OUT}/${STAGE}"
mkdir -p "${OUT}/${STAGE}"
cp "${ROOT}/target/release/aerosync" "${OUT}/${STAGE}/"
cp "${ROOT}/target/release/aerosync-mcp" "${OUT}/${STAGE}/"
cp "${ROOT}/README.md" "${ROOT}/LICENSE" "${ROOT}/CHANGELOG.md" "${OUT}/${STAGE}/" 2>/dev/null || true

ARCHIVE="${OUT}/${STAGE}.tar.gz"
( cd "${OUT}" && tar -czf "${STAGE}.tar.gz" "${STAGE}" )
( cd "${OUT}" && shasum -a 256 "${STAGE}.tar.gz" > "${STAGE}.tar.gz.sha256" )

echo "==> written:"
echo "    ${ARCHIVE}"
echo "    ${ARCHIVE}.sha256"
( cd "${OUT}" && shasum -a 256 -c "${STAGE}.tar.gz.sha256" )
echo
echo "Upload these two files to a GitHub Release (same tag as archive name) or run:"
echo "  gh release create ${TAG_NAME} --notes-file CHANGELOG.md ${ARCHIVE} ${ARCHIVE}.sha256"
