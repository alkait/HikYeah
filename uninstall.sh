#!/usr/bin/env bash
# HikYeah uninstaller (Linux).
#
# Usage:
#   /bin/bash -c "$(curl -fsSL https://github.com/alkait/HikYeah/releases/latest/download/uninstall.sh)"
#
# Removes everything install.sh wrote — the app dir, symlink, desktop entry —
# plus session state and caches. Asks separately whether to also delete the
# camera config (config.json, reusable across machines) and this machine's
# prefs (prefs.json); both default to keeping the file.

set -euo pipefail

DATA_DIR="${XDG_DATA_HOME:-$HOME/.local/share}/hikyeah"
DESKTOP="${XDG_DATA_HOME:-$HOME/.local/share}/applications/hikyeah.desktop"
CONF_DIR="${XDG_CONFIG_HOME:-$HOME/.config}/hikviewer"
CACHE_DIR="${XDG_CACHE_HOME:-$HOME/.cache}/hikviewer"

if [[ -t 1 ]]; then
  _BOLD=$'\033[1m'; _GREEN=$'\033[0;32m'; _RESET=$'\033[0m'
else
  _BOLD=''; _GREEN=''; _RESET=''
fi
step() { printf '%s==>%s %s\n' "$_BOLD" "$_RESET" "$*"; }
ok()   { printf '%s✓%s %s\n'   "$_GREEN" "$_RESET" "$*"; }

# Prompt on the terminal even when the script itself arrives on stdin
# (curl | bash); with no terminal at all, answer no — never delete configs
# without a human saying so.
ask() {
  local reply=n
  if [[ -t 0 ]]; then
    read -r -p "$1 [y/N] " reply
  elif [[ -r /dev/tty ]]; then
    { read -r -p "$1 [y/N] " reply < /dev/tty; } 2>/dev/null || reply=n
  fi
  [[ "$reply" =~ ^[Yy]$ ]]
}

[[ "$(uname -s)" == "Linux" ]] || { echo "Linux-only uninstaller" >&2; exit 1; }

step "Removing HikYeah"
rm -rf "$DATA_DIR"
rm -f "$HOME/.local/bin/hikyeah" "$DESKTOP"
rm -rf "$CACHE_DIR"
rm -f "$CONF_DIR/state.json"
ok "app, symlink, desktop entry, caches, session state removed"

if [[ -f "$CONF_DIR/config.json" ]]; then
  if ask "Also delete the camera config ($CONF_DIR/config.json)?"; then
    rm -f "$CONF_DIR/config.json"
    ok "camera config deleted"
  else
    ok "camera config kept"
  fi
fi
if [[ -f "$CONF_DIR/prefs.json" ]]; then
  if ask "Also delete this machine's preferences ($CONF_DIR/prefs.json)?"; then
    rm -f "$CONF_DIR/prefs.json"
    ok "preferences deleted"
  else
    ok "preferences kept"
  fi
fi
rmdir "$CONF_DIR" 2>/dev/null || true

echo
ok "HikYeah uninstalled"
