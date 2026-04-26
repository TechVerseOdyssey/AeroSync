#!/usr/bin/env bash
# AeroSync one-line installer.
#
#   curl -fsSL https://raw.githubusercontent.com/TechVerseOdyssey/AeroSync/master/install.sh | bash
#
# Environment variables:
#   AEROSYNC_VERSION   pin to a specific tag, e.g. "v0.2.0" (default: latest)
#   AEROSYNC_PREFIX    install prefix (default: $HOME/.local/bin if writable, else /usr/local/bin via sudo)
#   AEROSYNC_REPO      override "owner/name" (default: TechVerseOdyssey/AeroSync)
#
# Supported: macOS x86_64 + arm64, Linux x86_64 + arm64 (musl prebuilts)
#
set -euo pipefail

REPO="${AEROSYNC_REPO:-TechVerseOdyssey/AeroSync}"
VERSION="${AEROSYNC_VERSION:-latest}"
PREFIX="${AEROSYNC_PREFIX:-}"

err()  { printf '\033[31merror:\033[0m %s\n' "$*" >&2; exit 1; }
info() { printf '\033[36m==>\033[0m %s\n' "$*"; }

# ── 1. detect platform ─────────────────────────────────────────────────────
uname_s=$(uname -s)
uname_m=$(uname -m)

case "${uname_s}-${uname_m}" in
  Darwin-arm64)  TARGET="aarch64-apple-darwin" ;;
  Darwin-x86_64) TARGET="x86_64-apple-darwin" ;;
  Linux-x86_64)  TARGET="x86_64-unknown-linux-musl" ;;
  Linux-aarch64) TARGET="aarch64-unknown-linux-musl" ;;
  Linux-arm64)   TARGET="aarch64-unknown-linux-musl" ;;
  *) err "unsupported platform: ${uname_s}-${uname_m} (one-line installer has prebuilts for: macOS x86_64/arm64, Linux x86_64/aarch64). Try: cargo install --locked aerosync aerosync-mcp" ;;
esac
info "platform = ${TARGET}"

# ── 2. resolve version + URL ───────────────────────────────────────────────
if [ "${VERSION}" = "latest" ]; then
  info "resolving latest release tag…"
  REL_JSON=$(curl -fsSL "https://api.github.com/repos/${REPO}/releases/latest")
  if command -v python3 >/dev/null 2>&1; then
    VERSION=$(printf '%s' "${REL_JSON}" | python3 -c "import json,sys; print(json.load(sys.stdin).get('tag_name',''))")
  else
    VERSION=$(printf '%s' "${REL_JSON}" | grep -m1 '"tag_name"' | sed -E 's/.*"tag_name"[[:space:]]*:[[:space:]]*"([^"]+)".*/\1/')
  fi
  [ -n "${VERSION}" ] || err "could not resolve latest release; pin AEROSYNC_VERSION manually (e.g. v0.3.0)."
fi
info "version = ${VERSION}"

VERSION_NO_V="${VERSION#v}"
ARCHIVE="aerosync-${VERSION_NO_V}-${TARGET}.tar.gz"
URL="https://github.com/${REPO}/releases/download/${VERSION}/${ARCHIVE}"
SUM_URL="${URL}.sha256"

# ── 3. download + verify ────────────────────────────────────────────────────
TMP=$(mktemp -d)
trap 'rm -rf "${TMP}"' EXIT

info "downloading ${ARCHIVE}"
curl -fsSL --output "${TMP}/${ARCHIVE}"        "${URL}"
curl -fsSL --output "${TMP}/${ARCHIVE}.sha256" "${SUM_URL}"

info "verifying SHA-256…"
if command -v shasum >/dev/null 2>&1; then
  (cd "${TMP}" && shasum -a 256 -c "${ARCHIVE}.sha256")
elif command -v sha256sum >/dev/null 2>&1; then
  EXPECTED=$(awk '{print $1}' "${TMP}/${ARCHIVE}.sha256")
  ACTUAL=$(sha256sum "${TMP}/${ARCHIVE}" | awk '{print $1}')
  [ "${EXPECTED}" = "${ACTUAL}" ] || err "checksum mismatch (expected ${EXPECTED}, got ${ACTUAL})"
else
  err "neither shasum nor sha256sum found; cannot verify download."
fi

info "extracting…"
tar -xzf "${TMP}/${ARCHIVE}" -C "${TMP}"
EXTRACTED="${TMP}/aerosync-${VERSION_NO_V}-${TARGET}"
[ -d "${EXTRACTED}" ] || err "expected directory missing after extract: ${EXTRACTED} (wrong archive layout or version?)"
[ -f "${EXTRACTED}/aerosync" ] || err "aerosync binary not found in ${EXTRACTED}"

# ── 4. choose install prefix ────────────────────────────────────────────────
if [ -z "${PREFIX}" ]; then
  if [ -w "${HOME}/.local/bin" ] || mkdir -p "${HOME}/.local/bin" 2>/dev/null; then
    PREFIX="${HOME}/.local/bin"
  else
    PREFIX="/usr/local/bin"
  fi
fi
info "install prefix = ${PREFIX}"

install_one() {
  local bin="$1"
  if [ -w "${PREFIX}" ]; then
    install -m 0755 "${EXTRACTED}/${bin}" "${PREFIX}/${bin}"
  else
    info "elevating with sudo to write ${PREFIX}/${bin}"
    sudo install -m 0755 "${EXTRACTED}/${bin}" "${PREFIX}/${bin}"
  fi
}
install_one "aerosync"
install_one "aerosync-mcp"

# ── 5. friendly closing message ─────────────────────────────────────────────
echo
info "installed to ${PREFIX}"
"${PREFIX}/aerosync" --version || true
echo
echo "Make sure ${PREFIX} is on your PATH. For example:"
echo "  echo 'export PATH=\"${PREFIX}:\$PATH\"' >> ~/.bashrc"
echo
echo "Next steps:"
echo "  • aerosync --help"
echo "  • Use as MCP server with Claude Desktop:"
echo "      \"mcpServers\": { \"aerosync\": { \"command\": \"${PREFIX}/aerosync-mcp\" } }"
