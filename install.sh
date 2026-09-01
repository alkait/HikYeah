#!/usr/bin/env bash
# HikYeah curl|bash installer (Linux x86_64).
#
# Usage:
#   /bin/bash -c "$(curl -fsSL https://github.com/alkait/HikYeah/releases/latest/download/install.sh)"
#
# Installs the latest release into ~/.local/share/hikyeah — binary and bundled
# ffmpeg side by side (the app prefers the ffmpeg next to its executable) —
# symlinks ~/.local/bin/hikyeah, and adds a desktop entry. Verifies SHA-256
# against the .sha256 shipped with the same release. Idempotent: re-run to
# update; a running HikYeah keeps its old binary until relaunched (Linux
# replaces files by inode). Modeled on HikViewer's installer, minus the macOS
# quarantine dance — no such thing on Linux.
#
# The release tag is resolved once up front so tarball and checksum come from
# the same release (right after a publish, `releases/latest/download/<asset>`
# can briefly serve adjacent releases). Pin one with $HIKYEAH_TARBALL_URL;
# skip verification (not recommended) with HIKYEAH_SKIP_VERIFY=1.

set -euo pipefail

REPO="alkait/HikYeah"
ASSET_SUFFIX="linux-x86_64.tar.gz"
DATA_DIR="${XDG_DATA_HOME:-$HOME/.local/share}/hikyeah"
BIN_DIR="$HOME/.local/bin"
APPS_DIR="${XDG_DATA_HOME:-$HOME/.local/share}/applications"

# ---- tiny output helpers -------------------------------------------------

if [[ -t 1 ]]; then
  _BOLD=$'\033[1m'; _DIM=$'\033[2m'; _RED=$'\033[0;31m'
  _GREEN=$'\033[0;32m'; _YELLOW=$'\033[0;33m'; _RESET=$'\033[0m'
else
  _BOLD=''; _DIM=''; _RED=''; _GREEN=''; _YELLOW=''; _RESET=''
fi

step() { printf '%s==>%s %s\n' "$_BOLD" "$_RESET" "$*"; }
ok()   { printf '%s✓%s %s\n'   "$_GREEN" "$_RESET" "$*"; }
warn() { printf '%s!%s %s\n'   "$_YELLOW" "$_RESET" "$*"; }
die()  { printf '%serror:%s %s\n' "$_RED" "$_RESET" "$*" >&2; exit 1; }

# ---- sanity checks -------------------------------------------------------

[[ "$(uname -s)" == "Linux" ]] || die "this installer is Linux-only (got $(uname -s))"
[[ "$(uname -m)" == "x86_64" ]] || die "only x86_64 builds are published (got $(uname -m))"
command -v curl >/dev/null 2>&1 || die "curl is required"

TMP="$(mktemp -d -t hikyeah-install.XXXXXX)"
trap 'rm -rf "$TMP"' EXIT

# ---- resolve latest release tag -----------------------------------------

# JSON parsed without jq (not guaranteed installed): the API returns one
# minified line, so grep -o pulls the "tag_name" pair and sed peels the value.
if [[ -n "${HIKYEAH_TARBALL_URL:-}" ]]; then
  TAR_URL="$HIKYEAH_TARBALL_URL"
  TAG="(pinned)"
else
  step "Resolving latest release"
  TAG="$(curl -fsL --retry 2 \
    -H 'Accept: application/vnd.github+json' \
    "https://api.github.com/repos/${REPO}/releases/latest" \
    | grep -o '"tag_name"[[:space:]]*:[[:space:]]*"[^"]*"' \
    | head -1 \
    | sed -E 's/.*:[[:space:]]*"([^"]*)"/\1/')" \
    || die "could not reach api.github.com to resolve latest release"
  [[ -n "$TAG" && "$TAG" != "latest" ]] || die "could not parse latest release tag"
  TAR_URL="https://github.com/${REPO}/releases/download/${TAG}/hikyeah-${TAG}-${ASSET_SUFFIX}"
  ok "latest release: $TAG"
fi
ASSET="$(basename "$TAR_URL")"

# ---- download + verify ---------------------------------------------------

step "Downloading $ASSET"
curl -fL --retry 3 --retry-delay 2 -o "$TMP/$ASSET" "$TAR_URL" \
  || die "failed to download $TAR_URL"

if [[ "${HIKYEAH_SKIP_VERIFY:-0}" == "1" ]]; then
  warn "skipping checksum verification (HIKYEAH_SKIP_VERIFY=1)"
else
  step "Verifying SHA-256"
  curl -fsL --retry 2 -o "$TMP/$ASSET.sha256" "$TAR_URL.sha256" \
    || die "failed to download $TAR_URL.sha256 — re-run with HIKYEAH_SKIP_VERIFY=1 to bypass (not recommended)"
  (cd "$TMP" && sha256sum -c --status "$ASSET.sha256") \
    || die "SHA-256 mismatch for $ASSET — refusing to install a corrupted download"
  ok "checksum matches"
fi

# ---- extract + install ---------------------------------------------------

step "Installing to $DATA_DIR"
mkdir -p "$TMP/extracted"
tar xzf "$TMP/$ASSET" -C "$TMP/extracted"
# The tarball stages everything inside a versioned folder — find the binary
# instead of hard-coding the folder name and breaking on the next version.
SRC_DIR="$(find "$TMP/extracted" -maxdepth 2 -type f -name hikyeah -printf '%h' -quit)"
[[ -n "$SRC_DIR" ]] || die "hikyeah binary not found inside the tarball — release may be malformed"

# Stage next to the destination, then swap — never leaves a half-installed dir.
mkdir -p "$(dirname "$DATA_DIR")"
rm -rf "$DATA_DIR.new"
cp -r "$SRC_DIR" "$DATA_DIR.new"
rm -rf "$DATA_DIR"
mv "$DATA_DIR.new" "$DATA_DIR"

mkdir -p "$BIN_DIR"
ln -sfn "$DATA_DIR/hikyeah" "$BIN_DIR/hikyeah"

mkdir -p "$APPS_DIR"
cat > "$APPS_DIR/hikyeah.desktop" <<EOF
[Desktop Entry]
Type=Application
Name=HikYeah
Comment=Hikvision camera viewer
Exec=$DATA_DIR/hikyeah
Terminal=false
Categories=AudioVideo;Video;
EOF

echo
ok "HikYeah $TAG installed — run 'hikyeah' or launch it from your app menu"
case ":$PATH:" in
  *":$BIN_DIR:"*) ;;
  *) warn "$BIN_DIR is not on your PATH — add it, or run $DATA_DIR/hikyeah" ;;
esac
printf '%sUpdates:%s re-run this installer.\n' "$_DIM" "$_RESET"
