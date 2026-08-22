#!/usr/bin/env bash
# Meshcast Discord bot — one-command deploy for a Linux server
# (Proxmox LXC, VPS, Raspberry Pi, your homelab box…).
#
#   curl -fsSL https://raw.githubusercontent.com/mattcree/meshcast/main/scripts/deploy-bot.sh \
#     | bash -s -- YOUR_DISCORD_TOKEN
#
# or:  DISCORD_TOKEN=... ./deploy-bot.sh
#
# Options:
#   --version vX.Y.Z   install a specific release (default: latest)
#   --from-source      build with cargo instead of downloading the release binary
#                      (automatic on CPUs without a prebuilt binary, e.g. arm64)
#   --update           re-download/rebuild and restart (token unchanged)
#   --uninstall        stop and remove the service (keeps state dir)
#
# What it does:
#   * installs the meshcast-bot binary (prebuilt from GitHub Releases, or builds it)
#   * stores the token in a 0600 env file, never in the unit file
#   * installs and starts a systemd service (system-wide if root, else a
#     user service with lingering enabled)
#   * the bot keeps its identity + links in its state dir, so restarts and
#     updates don't require users to /link again
set -euo pipefail

REPO="${MESHCAST_REPO:-mattcree/meshcast}"
VERSION="${MESHCAST_VERSION:-latest}"
FROM_SOURCE=0
UPDATE=0
UNINSTALL=0
TOKEN="${DISCORD_TOKEN:-}"

while [ $# -gt 0 ]; do
    case "$1" in
        --version) VERSION="$2"; shift 2 ;;
        --from-source) FROM_SOURCE=1; shift ;;
        --update) UPDATE=1; shift ;;
        --uninstall) UNINSTALL=1; shift ;;
        -h|--help) sed -n '2,24p' "$0"; exit 0 ;;
        --*) echo "Unknown option: $1" >&2; exit 2 ;;
        *) TOKEN="$1"; shift ;;
    esac
done

info() { printf '\033[1;34m==>\033[0m %s\n' "$*"; }
warn() { printf '\033[1;33mwarning:\033[0m %s\n' "$*" >&2; }
die()  { printf '\033[1;31merror:\033[0m %s\n' "$*" >&2; exit 1; }

# --- Where things go -------------------------------------------------------

if [ "$(id -u)" = 0 ]; then
    SCOPE=system
    BIN_DIR=/usr/local/bin
    STATE_DIR=/var/lib/meshcast-bot
    ENV_FILE=/etc/meshcast-bot.env
    UNIT_DIR=/etc/systemd/system
    SYSTEMCTL=(systemctl)
    SERVICE_USER=meshcast-bot
else
    SCOPE=user
    BIN_DIR="$HOME/.local/bin"
    STATE_DIR="${XDG_CONFIG_HOME:-$HOME/.config}/meshcast-bot"
    ENV_FILE="$STATE_DIR/discord.env"
    UNIT_DIR="${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user"
    SYSTEMCTL=(systemctl --user)
    SERVICE_USER=""
fi
UNIT="$UNIT_DIR/meshcast-bot.service"
SRC_DIR="${MESHCAST_SRC_DIR:-$HOME/meshcast}"

command -v systemctl >/dev/null 2>&1 || die "systemd is required (no systemctl found)."

# --- Uninstall -------------------------------------------------------------

if [ "$UNINSTALL" = 1 ]; then
    info "Stopping and removing meshcast-bot service"
    "${SYSTEMCTL[@]}" disable --now meshcast-bot.service 2>/dev/null || true
    rm -f "$UNIT" "$BIN_DIR/meshcast-bot"
    "${SYSTEMCTL[@]}" daemon-reload
    echo "Removed. Token file ($ENV_FILE) and state ($STATE_DIR) were kept; delete them manually if you want."
    exit 0
fi

# --- Migrate a pre-0.5 user-scope install (root ran the old script) --------

OLD_STATE_DIR="${XDG_CONFIG_HOME:-$HOME/.config}/meshcast-bot"
OLD_UNIT="${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user/meshcast-bot.service"
if [ "$SCOPE" = system ] && [ -f "$OLD_UNIT" ]; then
    info "Found an older user-scope install — migrating it to the system service"
    systemctl --user disable --now meshcast-bot.service 2>/dev/null || true
    rm -f "$OLD_UNIT"
    systemctl --user daemon-reload 2>/dev/null || true
    mkdir -p "$STATE_DIR"
    if [ -f "$OLD_STATE_DIR/state.json" ] && [ ! -f "$STATE_DIR/state.json" ]; then
        cp "$OLD_STATE_DIR/state.json" "$STATE_DIR/state.json"
        info "Migrated bot identity and links from $OLD_STATE_DIR/state.json (users won't need to /link again)"
    fi
    if [ -z "$TOKEN" ] && [ ! -f "$ENV_FILE" ] && [ -f "$OLD_STATE_DIR/discord.env" ]; then
        cp "$OLD_STATE_DIR/discord.env" "$ENV_FILE"
        info "Reused token from $OLD_STATE_DIR/discord.env"
    fi
fi

# --- Token -----------------------------------------------------------------

if [ "$UPDATE" = 1 ] && [ -z "$TOKEN" ]; then
    [ -f "$ENV_FILE" ] || die "No existing token file at $ENV_FILE; pass the token."
else
    [ -n "$TOKEN" ] || die "Usage: $0 <DISCORD_TOKEN>   (or set DISCORD_TOKEN). Get one at https://discord.com/developers/applications — see docs/DISCORD-SETUP.md"
    case "$TOKEN" in
        *" "*|*$'\t'*) die "Token contains whitespace — did you paste the whole thing?" ;;
    esac
fi

# --- Install binary --------------------------------------------------------

ARCH="$(uname -m)"
ASSET=""
case "$ARCH" in
    x86_64|amd64) ASSET="meshcast-bot-linux-x86_64.tar.gz" ;;
    *) warn "No prebuilt bot binary for $ARCH — building from source."; FROM_SOURCE=1 ;;
esac

mkdir -p "$BIN_DIR"
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

if [ "$FROM_SOURCE" = 0 ]; then
    command -v curl >/dev/null 2>&1 || die "curl is required."
    if [ "$VERSION" = "latest" ]; then
        BASE="https://github.com/$REPO/releases/latest/download"
    else
        BASE="https://github.com/$REPO/releases/download/$VERSION"
    fi
    info "Downloading $ASSET ($VERSION)…"
    if curl -fsSL --retry 3 -o "$WORK/$ASSET" "$BASE/$ASSET"; then
        if curl -fsSL --retry 3 -o "$WORK/SHA256SUMS" "$BASE/SHA256SUMS" 2>/dev/null; then
            EXPECTED="$(grep " $ASSET\$" "$WORK/SHA256SUMS" | awk '{print $1}')"
            ACTUAL="$(sha256sum "$WORK/$ASSET" | awk '{print $1}')"
            [ -z "$EXPECTED" ] || [ "$EXPECTED" = "$ACTUAL" ] || die "Checksum mismatch for $ASSET"
            info "Checksum OK"
        fi
        tar xzf "$WORK/$ASSET" -C "$WORK"
        install -m 0755 "$WORK/meshcast-bot/meshcast-bot" "$BIN_DIR/meshcast-bot"
    else
        warn "Download failed (no release for $VERSION?). Falling back to building from source."
        FROM_SOURCE=1
    fi
fi

if [ "$FROM_SOURCE" = 1 ]; then
    info "Building meshcast-bot from source (this takes a few minutes the first time)…"
    if command -v dnf >/dev/null 2>&1; then
        sudo dnf install -y gcc gcc-c++ git pkg-config cmake clang-devel
    elif command -v apt-get >/dev/null 2>&1; then
        sudo apt-get update -qq && sudo apt-get install -y build-essential git pkg-config cmake clang libclang-dev
    elif command -v pacman >/dev/null 2>&1; then
        sudo pacman -S --noconfirm --needed base-devel git pkg-config cmake clang
    else
        warn "Unknown package manager; make sure gcc, git, pkg-config, cmake and clang are installed."
    fi
    if ! command -v cargo >/dev/null 2>&1; then
        info "Installing Rust…"
        curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
        # shellcheck disable=SC1091
        source "$HOME/.cargo/env"
    fi
    if [ -d "$SRC_DIR/.git" ]; then
        git -C "$SRC_DIR" fetch --tags --prune origin
    else
        git clone "https://github.com/$REPO.git" "$SRC_DIR"
    fi
    if [ "$VERSION" = "latest" ]; then
        git -C "$SRC_DIR" checkout -q main && git -C "$SRC_DIR" pull --ff-only
    else
        git -C "$SRC_DIR" checkout -q "$VERSION"
    fi
    (cd "$SRC_DIR" && cargo build --release --locked -p meshcast-bot)
    install -m 0755 "$SRC_DIR/target/release/meshcast-bot" "$BIN_DIR/meshcast-bot"
fi
info "Installed $BIN_DIR/meshcast-bot"

# --- Token file + state dir -----------------------------------------------

if [ "$SCOPE" = system ]; then
    id -u "$SERVICE_USER" >/dev/null 2>&1 || useradd --system --home-dir "$STATE_DIR" --shell /usr/sbin/nologin "$SERVICE_USER"
    mkdir -p "$STATE_DIR"
    chown "$SERVICE_USER:$SERVICE_USER" "$STATE_DIR"
    chmod 0700 "$STATE_DIR"
else
    mkdir -p "$STATE_DIR"
    chmod 0700 "$STATE_DIR"
fi

if [ -n "$TOKEN" ]; then
    umask 077
    printf 'DISCORD_TOKEN=%s\n' "$TOKEN" > "$ENV_FILE"
    umask 022
    chmod 0600 "$ENV_FILE"
    [ "$SCOPE" = system ] && chown root:root "$ENV_FILE"
    info "Stored token in $ENV_FILE (mode 600)"
fi

# --- systemd unit ----------------------------------------------------------

mkdir -p "$UNIT_DIR"
if [ "$SCOPE" = system ]; then
    USER_LINE="User=$SERVICE_USER"
    WANTED_BY="multi-user.target"
    PROTECT_HOME="true"
else
    USER_LINE=""
    WANTED_BY="default.target"
    PROTECT_HOME="read-only"
fi

cat > "$UNIT" <<EOF
[Unit]
Description=Meshcast Discord Bot
Documentation=https://github.com/$REPO
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
EnvironmentFile=$ENV_FILE
Environment=RUST_LOG=meshcast_bot=info,iroh=warn
Environment=MESHCAST_BOT_STATE_DIR=$STATE_DIR
ExecStart=$BIN_DIR/meshcast-bot
Restart=always
RestartSec=10
$USER_LINE
NoNewPrivileges=true
PrivateTmp=true
ProtectSystem=full
ProtectHome=$PROTECT_HOME
ReadWritePaths=$STATE_DIR

[Install]
WantedBy=$WANTED_BY
EOF
info "Wrote $UNIT"

if [ "$SCOPE" = user ]; then
    loginctl enable-linger "$(whoami)" 2>/dev/null || warn "Couldn't enable lingering; the bot will stop when you log out."
fi

"${SYSTEMCTL[@]}" daemon-reload
"${SYSTEMCTL[@]}" enable meshcast-bot.service >/dev/null
"${SYSTEMCTL[@]}" restart meshcast-bot.service

sleep 2
if "${SYSTEMCTL[@]}" is-active --quiet meshcast-bot.service; then
    echo
    info "meshcast-bot is running."
else
    warn "meshcast-bot is not running. Check the logs:"
fi

if [ "$SCOPE" = system ]; then
    LOGS="journalctl -u meshcast-bot -f"
    STATUS="systemctl status meshcast-bot"
else
    LOGS="journalctl --user -u meshcast-bot -f"
    STATUS="systemctl --user status meshcast-bot"
fi
cat <<EOF

  Status:  $STATUS
  Logs:    $LOGS
  Update:  curl -fsSL https://raw.githubusercontent.com/$REPO/main/scripts/deploy-bot.sh | bash -s -- --update

Next: invite the bot to your server (docs/DISCORD-SETUP.md), then run /link in Discord.
EOF
