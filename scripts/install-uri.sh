#!/bin/bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
DESKTOP_FILE="$SCRIPT_DIR/../meshcast.desktop"

if [ ! -f "$DESKTOP_FILE" ]; then
    echo "Error: meshcast.desktop not found at $DESKTOP_FILE"
    exit 1
fi

mkdir -p ~/.local/share/applications
cp "$DESKTOP_FILE" ~/.local/share/applications/meshcast.desktop
xdg-mime default meshcast.desktop x-scheme-handler/meshcast

echo "Registered meshcast:// URI scheme."
echo "Make sure 'meshcast' is in your PATH (e.g. ~/.local/bin/meshcast)."
