#!/usr/bin/env bash
# Meshcast desktop installer (Linux and macOS).
#
#   curl -fsSL https://raw.githubusercontent.com/mattcree/meshcast/main/scripts/install.sh | bash
#
# Options (pass after `bash -s --`, or as arguments when running the script directly):
#   --version vX.Y.Z   install a specific release (default: latest)
#   --no-autostart     don't start the tray at login (Linux)
#   --no-launch        don't open Meshcast when the install finishes
#   --from-dir DIR     install from an already-extracted release directory
#                      (this is what the release tarball's own install.sh does)
#
# What it does:
#   * downloads the release for your OS/CPU and verifies its SHA-256
#   * installs to ~/.local/share/meshcast and symlinks into ~/.local/bin
#   * Linux: registers the app launcher, the meshcast:// URL handler and
#     (optionally) a login autostart entry for the tray
#   * macOS: removes the quarantine flag so the binaries run
#
# Nothing needs root. Uninstall with: ~/.local/share/meshcast/uninstall.sh
set -euo pipefail

REPO="${MESHCAST_REPO:-mattcree/meshcast}"
VERSION="${MESHCAST_VERSION:-latest}"
AUTOSTART=1
LAUNCH=1
FROM_DIR=""

while [ $# -gt 0 ]; do
    case "$1" in
        --version) VERSION="$2"; shift 2 ;;
        --no-autostart) AUTOSTART=0; shift ;;
        --no-launch) LAUNCH=0; shift ;;
        --from-dir) FROM_DIR="$2"; shift 2 ;;
        -h|--help) sed -n '2,22p' "$0"; exit 0 ;;
        *) echo "Unknown option: $1" >&2; exit 2 ;;
    esac
done

info()  { printf '\033[1;34m==>\033[0m %s\n' "$*"; }
warn()  { printf '\033[1;33mwarning:\033[0m %s\n' "$*" >&2; }
die()   { printf '\033[1;31merror:\033[0m %s\n' "$*" >&2; exit 1; }

OS="$(uname -s)"
ARCH="$(uname -m)"
case "$OS" in
    Linux)
        case "$ARCH" in
            x86_64|amd64) ASSET="meshcast-linux-x86_64.tar.gz" ;;
            *) die "No prebuilt Linux build for $ARCH yet — see README 'Building from source'." ;;
        esac ;;
    Darwin)
        case "$ARCH" in
            arm64|aarch64) ASSET="meshcast-macos-aarch64.tar.gz" ;;
            x86_64) ASSET="meshcast-macos-x86_64.tar.gz" ;;
            *) die "Unsupported macOS architecture: $ARCH" ;;
        esac ;;
    *) die "Unsupported OS: $OS (Windows: use install.ps1)" ;;
esac

DATA_HOME="${XDG_DATA_HOME:-$HOME/.local/share}"
INSTALL_DIR="$DATA_HOME/meshcast"
BIN_DIR="$HOME/.local/bin"
APPS_DIR="${XDG_DATA_HOME:-$HOME/.local/share}/applications"
AUTOSTART_DIR="${XDG_CONFIG_HOME:-$HOME/.config}/autostart"

need() { command -v "$1" >/dev/null 2>&1 || die "'$1' is required but not installed."; }

# --- Fetch -----------------------------------------------------------------

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

if [ -n "$FROM_DIR" ]; then
    SRC_DIR="$FROM_DIR"
    info "Installing from $SRC_DIR"
else
    need curl; need tar
    if [ "$VERSION" = "latest" ]; then
        BASE="https://github.com/$REPO/releases/latest/download"
    else
        BASE="https://github.com/$REPO/releases/download/$VERSION"
    fi
    info "Downloading $ASSET ($VERSION)…"
    curl -fsSL --retry 3 -o "$WORK/$ASSET" "$BASE/$ASSET" \
        || die "Download failed. Check https://github.com/$REPO/releases"
    if curl -fsSL --retry 3 -o "$WORK/SHA256SUMS" "$BASE/SHA256SUMS" 2>/dev/null; then
        EXPECTED="$(grep " $ASSET\$" "$WORK/SHA256SUMS" | awk '{print $1}')"
        if command -v sha256sum >/dev/null 2>&1; then
            ACTUAL="$(sha256sum "$WORK/$ASSET" | awk '{print $1}')"
        else
            ACTUAL="$(shasum -a 256 "$WORK/$ASSET" | awk '{print $1}')"
        fi
        if [ -n "$EXPECTED" ] && [ "$EXPECTED" != "$ACTUAL" ]; then
            die "Checksum mismatch for $ASSET (expected $EXPECTED, got $ACTUAL)"
        fi
        info "Checksum OK"
    else
        warn "No SHA256SUMS published for this release; skipping verification."
    fi
    tar xzf "$WORK/$ASSET" -C "$WORK"
    SRC_DIR="$WORK/meshcast"
fi

[ -x "$SRC_DIR/meshcast" ] || die "Release archive is missing the 'meshcast' binary."

# --- Install files ---------------------------------------------------------

info "Installing to $INSTALL_DIR"
mkdir -p "$INSTALL_DIR" "$BIN_DIR"
# Stop a running daemon/app so the binaries can be replaced safely.
CONFIG_DIR="${MESHCAST_CONFIG_DIR:-${XDG_CONFIG_HOME:-$HOME/.config}/meshcast}"
for pidfile in .app-pid .daemon-pid; do
    if [ -f "$CONFIG_DIR/$pidfile" ]; then
        pid="$(cat "$CONFIG_DIR/$pidfile" 2>/dev/null || true)"
        if [ -n "$pid" ] && kill -0 "$pid" 2>/dev/null; then
            info "Stopping running Meshcast process ($pid)…"
            kill "$pid" 2>/dev/null || true
            sleep 1
        fi
    fi
done
pkill -f "meshcast-tray.py" 2>/dev/null || true

cp "$SRC_DIR/meshcast" "$SRC_DIR/meshcast-app" "$INSTALL_DIR/"
chmod +x "$INSTALL_DIR/meshcast" "$INSTALL_DIR/meshcast-app"
for f in meshcast-tray.py uninstall.sh install.sh README.md LICENSE; do
    [ -f "$SRC_DIR/$f" ] && cp "$SRC_DIR/$f" "$INSTALL_DIR/"
done
[ -f "$INSTALL_DIR/meshcast-tray.py" ] && chmod +x "$INSTALL_DIR/meshcast-tray.py"
[ -f "$INSTALL_DIR/uninstall.sh" ] && chmod +x "$INSTALL_DIR/uninstall.sh"

ln -sf "$INSTALL_DIR/meshcast" "$BIN_DIR/meshcast"
ln -sf "$INSTALL_DIR/meshcast-app" "$BIN_DIR/meshcast-app"

if [ "$OS" = "Darwin" ]; then
    xattr -dr com.apple.quarantine "$INSTALL_DIR" 2>/dev/null || true
fi

# --- Desktop integration (Linux) -----------------------------------------

if [ "$OS" = "Linux" ]; then
    mkdir -p "$APPS_DIR"
    render() { sed "s|@INSTALL_DIR@|$INSTALL_DIR|g" "$1" > "$2"; }
    if [ -f "$SRC_DIR/meshcast.desktop" ]; then
        render "$SRC_DIR/meshcast.desktop" "$APPS_DIR/meshcast.desktop"
        render "$SRC_DIR/meshcast-watch.desktop" "$APPS_DIR/meshcast-watch.desktop"
        if [ "$AUTOSTART" = 1 ]; then
            mkdir -p "$AUTOSTART_DIR"
            render "$SRC_DIR/meshcast-tray-autostart.desktop" "$AUTOSTART_DIR/meshcast-tray.desktop"
            info "Tray will start at login (remove $AUTOSTART_DIR/meshcast-tray.desktop to disable)"
        fi
        if command -v update-desktop-database >/dev/null 2>&1; then
            update-desktop-database "$APPS_DIR" 2>/dev/null || true
        fi
        if command -v xdg-mime >/dev/null 2>&1; then
            xdg-mime default meshcast-watch.desktop x-scheme-handler/meshcast 2>/dev/null || true
        fi
        info "Registered app launcher and meshcast:// links"
    fi

    # Tray prerequisites (GTK3 + AppIndicator bindings for Python)
    if ! python3 -c 'import gi; gi.require_version("Gtk","3.0")' 2>/dev/null; then
        warn "python3-gobject (GTK 3 bindings) not found — the tray icon won't run."
        warn "  Fedora/Bluefin:  sudo dnf install python3-gobject libappindicator-gtk3"
        warn "  Ubuntu/Debian:   sudo apt install python3-gi gir1.2-ayatanaappindicator3-0.1"
        warn "  Arch:            sudo pacman -S python-gobject libappindicator-gtk3"
    elif ! python3 - <<'PY' 2>/dev/null
import gi
try:
    gi.require_version("AyatanaAppIndicator3", "0.1")
except ValueError:
    gi.require_version("AppIndicator3", "0.1")
PY
    then
        warn "AppIndicator bindings not found — the tray icon won't run (the app still works)."
        warn "  Fedora/Bluefin:  sudo dnf install libappindicator-gtk3"
        warn "  Ubuntu/Debian:   sudo apt install gir1.2-ayatanaappindicator3-0.1"
    fi
    if [ "${XDG_CURRENT_DESKTOP:-}" = "GNOME" ] || [ "${XDG_CURRENT_DESKTOP:-}" = "ubuntu:GNOME" ]; then
        info "GNOME: install the 'AppIndicator and KStatusNotifierItem Support' extension to see the tray icon."
    fi
fi

# --- Done ------------------------------------------------------------------

case ":$PATH:" in
    *":$BIN_DIR:"*) ;;
    *) warn "$BIN_DIR is not on your PATH. Add it to use 'meshcast' from a terminal." ;;
esac

echo
info "Meshcast installed."
echo "  Next: in Discord type /link, then paste the code into the Meshcast window."
echo

if [ "$LAUNCH" = 1 ]; then
    if [ "$OS" = "Linux" ] && { [ -n "${DISPLAY:-}" ] || [ -n "${WAYLAND_DISPLAY:-}" ]; }; then
        if [ -x "$INSTALL_DIR/meshcast-tray.py" ] && python3 -c 'import gi; gi.require_version("Gtk","3.0")' 2>/dev/null; then
            info "Starting Meshcast…"
            nohup "$INSTALL_DIR/meshcast-tray.py" --show >/dev/null 2>&1 &
        else
            info "Starting Meshcast window…"
            nohup "$INSTALL_DIR/meshcast-app" >/dev/null 2>&1 &
        fi
    elif [ "$OS" = "Darwin" ]; then
        info "Starting Meshcast…"
        nohup "$INSTALL_DIR/meshcast-app" >/dev/null 2>&1 &
    fi
fi
