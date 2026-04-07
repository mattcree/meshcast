#!/bin/bash
set -euo pipefail

# Meshcast Bot Deployment Script
# Deploys the Discord bot as a systemd service.
#
# Usage:
#   ./deploy-bot.sh <DISCORD_TOKEN>
#
# Run this ON the machine where you want the bot to run
# (e.g. inside a Proxmox LXC, a VPS, or any Linux server).
#
# What it does:
#   1. Installs Rust (if not present)
#   2. Installs minimal build dependencies
#   3. Clones and builds meshcast-bot
#   4. Creates a systemd user service
#   5. Starts the bot

DISCORD_TOKEN="${1:?Usage: $0 <DISCORD_TOKEN>}"
REPO_URL="https://github.com/mattcree/meshcast.git"
INSTALL_DIR="$HOME/meshcast"
BIN_DIR="$HOME/.local/bin"
SERVICE_DIR="$HOME/.config/systemd/user"

echo "=== Meshcast Bot Deployment ==="
echo ""

# Detect package manager
if command -v dnf &>/dev/null; then
    PKG_INSTALL="sudo dnf install -y"
elif command -v apt-get &>/dev/null; then
    PKG_INSTALL="sudo apt-get install -y"
    sudo apt-get update -qq
elif command -v pacman &>/dev/null; then
    PKG_INSTALL="sudo pacman -S --noconfirm"
else
    echo "Error: No supported package manager found (dnf, apt, pacman)"
    exit 1
fi

# Install build dependencies
# The bot only needs Rust + C toolchain (no PipeWire, no GPU, no audio)
echo ">>> Installing build dependencies..."
if command -v dnf &>/dev/null; then
    $PKG_INSTALL gcc gcc-c++ pkg-config openssl-devel cmake clang-devel
elif command -v apt-get &>/dev/null; then
    $PKG_INSTALL build-essential pkg-config libssl-dev cmake clang
elif command -v pacman &>/dev/null; then
    $PKG_INSTALL base-devel pkg-config openssl cmake clang
fi

# Install Rust
if ! command -v cargo &>/dev/null; then
    echo ">>> Installing Rust..."
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
    source "$HOME/.cargo/env"
else
    echo ">>> Rust already installed ($(rustc --version))"
fi

# Clone or update repo
if [ -d "$INSTALL_DIR" ]; then
    echo ">>> Updating meshcast..."
    cd "$INSTALL_DIR"
    git pull --ff-only
else
    echo ">>> Cloning meshcast..."
    git clone "$REPO_URL" "$INSTALL_DIR"
    cd "$INSTALL_DIR"
fi

# Build only the bot (much faster than full workspace — no screen capture deps)
echo ">>> Building meshcast-bot (this may take a few minutes on first build)..."
cargo build --release -p meshcast-bot

# Install binary
mkdir -p "$BIN_DIR"
cp target/release/meshcast-bot "$BIN_DIR/meshcast-bot"
echo ">>> Installed meshcast-bot to $BIN_DIR/meshcast-bot"

# Store token in a credential file (not in the unit file)
CRED_DIR="$HOME/.config/meshcast-bot"
CRED_FILE="$CRED_DIR/discord.env"
mkdir -p "$CRED_DIR"
echo "DISCORD_TOKEN=${DISCORD_TOKEN}" > "$CRED_FILE"
chmod 600 "$CRED_FILE"
echo ">>> Stored Discord token in $CRED_FILE (mode 600)"

# Create systemd user service
mkdir -p "$SERVICE_DIR"
cat > "$SERVICE_DIR/meshcast-bot.service" << EOF
[Unit]
Description=Meshcast Discord Bot
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
EnvironmentFile=${CRED_FILE}
ExecStart=${BIN_DIR}/meshcast-bot
Restart=on-failure
RestartSec=10

[Install]
WantedBy=default.target
EOF

echo ">>> Created systemd service"

# Enable lingering so the service runs without an active login session
loginctl enable-linger "$(whoami)" 2>/dev/null || true

# Start the service
systemctl --user daemon-reload
systemctl --user enable meshcast-bot.service
systemctl --user restart meshcast-bot.service

echo ""
echo "=== Deployment complete ==="
echo ""
echo "The bot is running. Check status with:"
echo "  systemctl --user status meshcast-bot"
echo ""
echo "View logs with:"
echo "  journalctl --user -u meshcast-bot -f"
echo ""
echo "To update later:"
echo "  cd $INSTALL_DIR && git pull && cargo build --release -p meshcast-bot"
echo "  cp target/release/meshcast-bot $BIN_DIR/"
echo "  systemctl --user restart meshcast-bot"
