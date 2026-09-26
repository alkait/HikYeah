#!/usr/bin/env bash
# HikYeah curl|bash installer (Linux x86_64, macOS Apple Silicon and Intel).
#
# Usage:
#   /bin/bash -c "$(curl -fsSL https://github.com/alkait/HikYeah/releases/latest/download/install.sh)"
#
# Linux: installs the latest release into ~/.local/share/hikyeah — binary,
# bundled FFmpeg libraries and ffmpeg side by side (the app prefers the
# ffmpeg next to its executable) — symlinks ~/.local/bin/hikyeah, and adds a
# desktop entry. A running HikYeah keeps its old binary until relaunched
# (Linux replaces files by inode).
#
# macOS: installs HikYeah.app into /Applications, the HikViewer way. The app
# is ad-hoc signed (no Apple Developer account), and macOS refuses to launch
# such an app once a browser has tagged it com.apple.quarantine; curl'd files
# carry no such xattr, so this script can drop a launchable bundle in place.
# The app links Homebrew's FFmpeg 8 libraries: `brew install ffmpeg@8`.
#
# Both verify SHA-256 against the .sha256 shipped with the same release, and
# both are idempotent: re-run to update. The in-app updater runs exactly this
# script with HIKYEAH_INAPP=1, which leaves quitting and relaunching to the
# app (it restarts itself onto the new files).
#
# The release tag is resolved once up front so archive and checksum come from
# the same release (right after a publish, `releases/latest/download/<asset>`
# can briefly serve adjacent releases). Pin one with $HIKYEAH_ASSET_URL (a
# file:// URL works for local testing); skip verification (not recommended)
# with HIKYEAH_SKIP_VERIFY=1.

set -euo pipefail

REPO="alkait/HikYeah"

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

# ---- platform --------------------------------------------------------------

OS="$(uname -s)"
case "$OS" in
  Linux)
    [[ "$(uname -m)" == "x86_64" ]] || die "only x86_64 Linux builds are published (got $(uname -m))"
    ASSET_SUFFIX="linux-x86_64.tar.gz"
    SHA256="sha256sum"
    DATA_DIR="${XDG_DATA_HOME:-$HOME/.local/share}/hikyeah"
    BIN_DIR="$HOME/.local/bin"
    APPS_DIR="${XDG_DATA_HOME:-$HOME/.local/share}/applications"
    ;;
  Darwin)
    case "$(uname -m)" in
      arm64) ASSET_SUFFIX="macos-arm64.app.zip" ;;
      x86_64) ASSET_SUFFIX="macos-x86_64.app.zip" ;;
      *) die "unsupported macOS architecture: $(uname -m)" ;;
    esac
    SHA256="shasum -a 256"
    DEST="/Applications/HikYeah.app"
    # /Applications is admin-writable without sudo on a stock Mac; if not,
    # say so instead of spraying sudo prompts.
    [[ -w /Applications ]] || die "/Applications is not writable by the current user"
    ;;
  *) die "unsupported OS: $OS (Linux x86_64 and macOS arm64 are published)" ;;
esac
command -v curl >/dev/null 2>&1 || die "curl is required"

TMP="$(mktemp -d -t hikyeah-install.XXXXXX)"
trap 'rm -rf "$TMP"' EXIT

# ---- resolve latest release tag -----------------------------------------

# JSON parsed without jq (not guaranteed installed): the API returns one
# minified line, so grep -o pulls the "tag_name" pair and sed peels the value.
if [[ -n "${HIKYEAH_ASSET_URL:-}" ]]; then
  ASSET_URL="$HIKYEAH_ASSET_URL"
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
  ASSET_URL="https://github.com/${REPO}/releases/download/${TAG}/hikyeah-${TAG}-${ASSET_SUFFIX}"
  ok "latest release: $TAG"
fi
ASSET="$(basename "$ASSET_URL")"

# ---- download + verify ---------------------------------------------------

step "Downloading $ASSET"
curl -fL --retry 3 --retry-delay 2 -o "$TMP/$ASSET" "$ASSET_URL" \
  || die "failed to download $ASSET_URL"

if [[ "${HIKYEAH_SKIP_VERIFY:-0}" == "1" ]]; then
  warn "skipping checksum verification (HIKYEAH_SKIP_VERIFY=1)"
else
  step "Verifying SHA-256"
  curl -fsL --retry 2 -o "$TMP/$ASSET.sha256" "$ASSET_URL.sha256" \
    || die "failed to download $ASSET_URL.sha256 — re-run with HIKYEAH_SKIP_VERIFY=1 to bypass (not recommended)"
  (cd "$TMP" && $SHA256 -c --status "$ASSET.sha256") \
    || die "SHA-256 mismatch for $ASSET — refusing to install a corrupted download"
  ok "checksum matches"
fi

# ---- extract + install ---------------------------------------------------

mkdir -p "$TMP/extracted"
if [[ "$OS" == "Linux" ]]; then
  step "Installing to $DATA_DIR"
  tar xzf "$TMP/$ASSET" -C "$TMP/extracted"
  # The archive stages everything inside a versioned folder — find the binary
  # instead of hard-coding the folder name and breaking on the next version.
  SRC_DIR="$(find "$TMP/extracted" -maxdepth 2 -type f -name hikyeah -printf '%h' -quit)"
  [[ -n "$SRC_DIR" ]] || die "hikyeah binary not found inside the archive — release may be malformed"

  # Stage next to the destination, then swap — never leaves a half-installed dir.
  mkdir -p "$(dirname "$DATA_DIR")"
  rm -rf "$DATA_DIR.new"
  cp -r "$SRC_DIR" "$DATA_DIR.new"
  rm -rf "$DATA_DIR"
  mv "$DATA_DIR.new" "$DATA_DIR"

  mkdir -p "$BIN_DIR"
  ln -sfn "$DATA_DIR/hikyeah" "$BIN_DIR/hikyeah"

  mkdir -p "$APPS_DIR"
  cat > "$APPS_DIR/hikyeah.desktop" <<DESKTOP
[Desktop Entry]
Type=Application
Name=HikYeah
Comment=Hikvision camera viewer
Exec=$DATA_DIR/hikyeah
Terminal=false
Categories=AudioVideo;Video;
DESKTOP

  echo
  ok "HikYeah $TAG installed — run 'hikyeah' or launch it from your app menu"
  case ":$PATH:" in
    *":$BIN_DIR:"*) ;;
    *) warn "$BIN_DIR is not on your PATH — add it, or run $DATA_DIR/hikyeah" ;;
  esac
else
  step "Installing to $DEST"
  unzip -q "$TMP/$ASSET" -d "$TMP/extracted"
  SRC_APP="$(find "$TMP/extracted" -maxdepth 3 -type d -name HikYeah.app -print -quit)"
  [[ -n "$SRC_APP" ]] || die "HikYeah.app not found inside the archive — release may be malformed"

  # From a terminal, quit the running copy and relaunch it at the end; from
  # the in-app updater the app itself restarts onto the new bundle.
  INAPP="${HIKYEAH_INAPP:-0}"
  if [[ "$INAPP" != "1" ]] && pgrep -f "$DEST/Contents/MacOS/hikyeah" >/dev/null 2>&1; then
    step "Quitting running HikYeah"
    osascript -e 'tell application "HikYeah" to quit' 2>/dev/null || true
    sleep 1
  fi

  rm -rf "$DEST"
  # ditto keeps the bundle's signature metadata intact; cp -R may not.
  ditto "$SRC_APP" "$DEST"
  # Defensive: the zip itself never carries quarantine, but strip anyway.
  xattr -dr com.apple.quarantine "$DEST" 2>/dev/null || true

  echo
  ok "HikYeah $TAG installed at $DEST"
  # Homebrew lives in /opt/homebrew on Apple Silicon, /usr/local on Intel.
  if [[ ! -d /opt/homebrew/opt/ffmpeg@8/lib && ! -d /usr/local/opt/ffmpeg@8/lib ]]; then
    warn "Homebrew's ffmpeg@8 is missing — HikYeah links its libraries:"
    warn "  brew install ffmpeg@8"
  fi
  if [[ "$INAPP" != "1" ]]; then
    step "Launching HikYeah"
    open "$DEST" 2>/dev/null || true
  fi
fi
printf '%sUpdates:%s Settings → Check for updates, or re-run this installer.\n' "$_DIM" "$_RESET"
