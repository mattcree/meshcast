#!/usr/bin/env bash
# Remove a Meshcast desktop install created by install.sh (Linux / macOS).
#   --purge   also delete config and links (~/.config/meshcast)
set -euo pipefail

PURGE=0
[ "${1:-}" = "--purge" ] && PURGE=1

DATA_HOME="${XDG_DATA_HOME:-$HOME/.local/share}"
INSTALL_DIR="$DATA_HOME/meshcast"
BIN_DIR="$HOME/.local/bin"
APPS_DIR="$DATA_HOME/applications"
AUTOSTART_DIR="${XDG_CONFIG_HOME:-$HOME/.config}/autostart"
if [ "$(uname -s)" = "Darwin" ] && [ -d "$HOME/Library/Application Support/meshcast" ]; then
    CONFIG_DIR="${MESHCAST_CONFIG_DIR:-$HOME/Library/Application Support/meshcast}"
else
    CONFIG_DIR="${MESHCAST_CONFIG_DIR:-${XDG_CONFIG_HOME:-$HOME/.config}/meshcast}"
fi

for pidfile in .app-pid .daemon-pid; do
    if [ -f "$CONFIG_DIR/$pidfile" ]; then
        pid="$(cat "$CONFIG_DIR/$pidfile" 2>/dev/null || true)"
        if [ -n "$pid" ]; then kill "$pid" 2>/dev/null || true; fi
    fi
done
pkill -f "meshcast-tray.py" 2>/dev/null || true

rm -f "$BIN_DIR/meshcast" "$BIN_DIR/meshcast-app"
rm -f "$APPS_DIR/meshcast.desktop" "$APPS_DIR/meshcast-watch.desktop" "$AUTOSTART_DIR/meshcast-tray.desktop"
if command -v update-desktop-database >/dev/null 2>&1; then
    update-desktop-database "$APPS_DIR" 2>/dev/null || true
fi
rm -rf "$INSTALL_DIR"

if [ "$PURGE" = 1 ]; then
    rm -rf "$CONFIG_DIR"
    echo "Removed Meshcast and its configuration."
else
    rm -f "$CONFIG_DIR"/.tray-state "$CONFIG_DIR"/.tray-cmd "$CONFIG_DIR"/.app-pid "$CONFIG_DIR"/.daemon-pid
    echo "Removed Meshcast. Config kept at $CONFIG_DIR (use --purge to delete it)."
fi
