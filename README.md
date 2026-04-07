# Meshcast

P2P screen streaming for Discord, powered by [iroh-live](https://github.com/n0-computer/iroh-live).

Stream your screen to friends in Discord without Nitro, without 720p caps, and without routing video through Discord's servers. Meshcast uses iroh's peer-to-peer QUIC transport for low-latency, high-quality streaming while Discord handles the social layer.

## How it works

```
You (streamer)                    Discord                     Friends (viewers)
     |                              |                              |
     |  Meshcast app running        |                              |
     |  /stream in Discord -------->|                              |
     |<---- bot signals your app    |                              |
     |  Screen capture starts       |                              |
     |  Ticket sent back to bot --->|  Embed with Watch button     |
     |                              |----------------------------->|
     |                              |  Friend clicks Watch         |
     |                              |<-----------------------------|
     |                              |  Bot signals friend's app    |
     |<===== P2P video stream ============================>|
     |  Direct QUIC, no Discord     |                      Viewer opens
```

Your video never touches Discord's servers. It flows directly between peers using [MoQ (Media over QUIC)](https://datatracker.ietf.org/wg/moq/about/) via iroh's NAT-traversing network.

## Features

- **No Nitro required** — stream at 1080p60 without paying Discord
- **P2P streaming** — low latency, no server bottleneck
- **Discord integration** — `/stream` starts capture, Watch button opens viewer
- **One-click watching** — linked friends just click Watch in Discord
- **Desktop app** — system tray, quality/FPS settings, dark theme UI
- **Cross-platform viewer** — Linux, macOS, Windows (streamer requires Linux for now)
- **NAT traversal** — iroh handles hole-punching and relay fallback automatically

## Quick start

### 1. Set up the Discord bot

Create a Discord application at [discord.com/developers](https://discord.com/developers/applications):

1. **New Application** → name it "Meshcast"
2. **Bot** tab → copy the bot token
3. **OAuth2** → URL Generator → select `bot` scope with permissions: `Send Messages`, `Embed Links`
4. Use the generated URL to invite the bot to your server

### 2. Run the bot

The bot runs on any machine (your PC, a homelab server, a VPS). It just needs to stay online.

```bash
# Download from GitHub Releases (or build from source)
DISCORD_TOKEN="your-bot-token-here" ./meshcast-bot
```

The bot will:
- Connect to Discord and register `/link` and `/stream` slash commands
- Start a signal node on iroh's network for communicating with desktop apps
- Persist linked users across restarts

### 3. Install the desktop app

Download `meshcast-app` from [GitHub Releases](https://github.com/mattcree/meshcast/releases/latest) for your platform.

Or build from source (see [Building](#building) below).

### 4. Link your app to the bot

1. Type `/link` in any Discord channel where the bot is present
2. The bot replies with a **pairing code** (visible only to you)
3. Open the Meshcast app, paste the code, click **Connect**
4. Done — you're linked. This persists across restarts.

### 5. Start streaming

1. Type `/stream` in Discord (optionally: `/stream title:"Game Night"`)
2. Your Meshcast app starts screen capture (PipeWire screen picker appears on Linux)
3. The bot posts an embed with a **Watch** button
4. Friends who are linked click Watch — the viewer opens automatically on their machine

### 6. Stop streaming

Type `/stream stop:True` in Discord, or click **Stop Stream** in the app.

## For viewers

Viewers need to:

1. Download the Meshcast app
2. Type `/link` in Discord to get a pairing code
3. Paste the code in the app
4. Click **Watch** on any stream embed — the viewer opens automatically

This is a one-time setup. After linking, watching is always one click.

## Architecture

Meshcast is four Rust crates:

| Crate | Purpose |
|-------|---------|
| `meshcast-signal` | Shared types, config, gossip protocol |
| `meshcast-bot` | Discord bot — slash commands, embeds, viewer tracking |
| `meshcast-app` | Desktop app — tray icon, config UI, daemon |
| `meshcast-cli` | CLI — for headless/scripted use |

**Signaling** between the bot and desktop apps uses [iroh-gossip](https://github.com/n0-computer/iroh-gossip) over QUIC. The gossip topic ID acts as the shared secret for the link.

**Streaming** uses [iroh-live](https://github.com/n0-computer/iroh-live) which provides:
- Screen capture (PipeWire on Linux, ScreenCaptureKit on macOS)
- H.264 encoding (openh264, software)
- MoQ transport (Media over QUIC, P2P with relay fallback)
- Software H.264 decoding + wgpu rendering in the viewer

## Settings

The app stores config at `~/.config/meshcast/config.toml`:

```toml
[video]
quality = "1080p"   # 360p, 720p, or 1080p
fps = 60            # 30 or 60
codec = "h264"

[audio]
enabled = true
```

Change settings in the app UI — they persist across restarts.

## CLI usage

For headless servers or scripting, the CLI provides the same functionality:

```bash
# Link to the bot
meshcast link <PAIRING-CODE>

# Run as a background daemon (responds to bot signals)
meshcast daemon

# Stream directly (manual, without Discord)
meshcast stream --quality 1080p

# Watch a stream by ticket
meshcast watch <TICKET>

# Remove the bot link
meshcast unlink
```

## Building

### Prerequisites (Linux / Fedora / Bluefin)

Meshcast has native dependencies for screen capture, video encoding, and GPU rendering. On immutable distros like Bluefin, use a toolbox:

```bash
toolbox create meshcast
toolbox enter meshcast

sudo dnf install -y \
  pipewire-devel clang-devel libva-devel nasm pkg-config cmake \
  alsa-lib-devel mesa-libgbm-devel mesa-vulkan-drivers \
  libxkbcommon libxkbcommon-devel gtk3-devel atk-devel \
  libappindicator-gtk3 libxdo-devel
```

### Build

```bash
git clone https://github.com/mattcree/meshcast.git
cd meshcast
cargo build --release --workspace
```

Binaries are in `target/release/`:
- `meshcast-bot` — the Discord bot
- `meshcast-app` — the desktop application
- `meshcast` — the CLI

### Running from toolbox (Bluefin)

Since the binaries link against libraries in the toolbox:

```bash
toolbox run -c meshcast ./target/release/meshcast-app
toolbox run -c meshcast DISCORD_TOKEN="..." ./target/release/meshcast-bot
```

## Deploying the bot

### One-command deploy (Linux server / Proxmox LXC)

SSH into your server and run:

```bash
curl -sSL https://raw.githubusercontent.com/mattcree/meshcast/main/scripts/deploy-bot.sh | bash -s -- "YOUR_DISCORD_TOKEN"
```

Or clone and run locally:

```bash
git clone https://github.com/mattcree/meshcast.git
cd meshcast
./scripts/deploy-bot.sh "YOUR_DISCORD_TOKEN"
```

This will:
1. Install Rust and build dependencies
2. Build `meshcast-bot`
3. Create and start a systemd user service
4. Enable auto-restart on failure

The bot has **no screen capture or GPU dependencies** — it runs on any minimal Linux server.

### Proxmox LXC setup

1. Create a Fedora container: **Datacenter → Create CT → Template: fedora-43-standard**
2. Give it 1 CPU, 512MB RAM, 8GB disk (the bot is lightweight)
3. Start the container and open a console
4. Run the deploy script above

### Managing the bot

```bash
# Check status
systemctl --user status meshcast-bot

# View logs
journalctl --user -u meshcast-bot -f

# Restart
systemctl --user restart meshcast-bot

# Update to latest version
cd ~/meshcast && git pull && cargo build --release -p meshcast-bot
cp target/release/meshcast-bot ~/.local/bin/
systemctl --user restart meshcast-bot
```

## Relay server (for 5+ viewers)

By default, your stream goes directly from your machine to each viewer (P2P). This works great for 2-4 viewers but scales linearly with your upload bandwidth.

For larger audiences, deploy a **relay server** — it receives one copy of your stream and fans it out to all viewers. Your upload stays constant regardless of viewer count.

### When you need a relay

| Viewers | Upload needed (1080p) | Relay needed? |
|---------|----------------------|---------------|
| 1-2 | ~10 Mbps | No |
| 3-4 | ~20 Mbps | Maybe |
| 5-10 | ~50 Mbps | Yes |
| 10+ | ~100+ Mbps | Definitely |

### Setting up a relay

The relay needs to be on a machine with its **own** internet connection (not your homelab on the same network). A cheap VPS works:

1. Get a VPS (~$5/mo: Hetzner, Oracle Cloud free tier, DigitalOcean)

2. Build iroh-live's relay:
```bash
# On the VPS
git clone https://github.com/n0-computer/iroh-live.git
cd iroh-live
cargo build --release -p iroh-live-cli
./target/release/irl relay
```

3. The relay prints its endpoint address. Configure meshcast to use it by passing `--relay <address>` when streaming.

### How the relay works

```
Without relay:               With relay:
You ──→ Viewer 1             You ──→ Relay ──→ Viewer 1
You ──→ Viewer 2                          ──→ Viewer 2  
You ──→ Viewer 3                          ──→ Viewer 3
(3x your upload)             (1x your upload)
```

The relay is just a fan-out server — it doesn't process or re-encode the video. Relay support in Meshcast is planned but not yet integrated into the app UI.

## Platform support

| | Streamer (capture + publish) | Viewer (watch) |
|---|---|---|
| **Linux (X11/Wayland)** | Full support | Full support |
| **macOS** | Should work (untested) | Should work (untested) |
| **Windows** | Not yet (iroh-live limitation) | Should work (untested) |

The viewer path (decode + render) is cross-platform since it uses software H.264 decode and wgpu. The capture path depends on platform-specific screen capture APIs.

## How it compares to Discord screen share

| | Discord | Meshcast |
|---|---|---|
| **Quality** | 720p30 (1080p60 with Nitro) | Up to 1080p60, free |
| **Latency** | Routed through Discord servers | P2P direct, lower latency |
| **Transport** | WebRTC via Discord | MoQ over QUIC via iroh |
| **NAT traversal** | Discord servers | iroh relay + hole punching |
| **Linux support** | Buggy (Electron + PipeWire issues) | Native PipeWire capture |
| **Setup** | Zero | One-time app install + link |
| **Multi-viewer** | Built-in | P2P (relay for 5+ viewers) |

## Tech stack

- [iroh-live](https://github.com/n0-computer/iroh-live) — capture, encode, transport, decode, render
- [iroh](https://github.com/n0-computer/iroh) — P2P networking with NAT traversal
- [iroh-gossip](https://github.com/n0-computer/iroh-gossip) — signaling between bot and apps
- [poise](https://github.com/serenity-rs/poise) — Discord bot framework
- [egui](https://github.com/emilk/egui) / [eframe](https://docs.rs/eframe) — desktop UI
- [tray-icon](https://github.com/tauri-apps/tray-icon) — system tray

## License

MIT
