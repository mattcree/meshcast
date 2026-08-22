# Meshcast

[![CI](https://github.com/mattcree/meshcast/actions/workflows/ci.yml/badge.svg)](https://github.com/mattcree/meshcast/actions/workflows/ci.yml)
[![Release](https://img.shields.io/github/v/release/mattcree/meshcast)](https://github.com/mattcree/meshcast/releases/latest)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

**High-quality, low-latency screen sharing for your Discord server — no Nitro, no 720p cap, and your video never touches Discord's servers.**

Meshcast is a small Discord bot plus a desktop app. You type `/stream`, your friends click **Watch**, and the video flows peer-to-peer over [iroh](https://iroh.computer) using [MoQ (Media over QUIC)](https://datatracker.ietf.org/wg/moq/about/). Discord stays what it's good at — the place where people are — and the heavy lifting is done by [iroh-live](https://github.com/n0-computer/iroh-live).

```
You (streamer)                    Discord                       Friends (viewers)
     │  /stream ───────────────────►│                                 │
     │◄── bot asks your app ────────│                                 │
     │  approve → screen capture    │                                 │
     │  ticket ────────────────────►│ posts card with [Watch] [Stop]  │
     │                              │────────────────────────────────►│
     │                              │◄──────── clicks Watch ──────────│
     │                              │─── bot tells their app ────────►│ viewer opens
     │◄════════════ P2P video over QUIC (never via Discord) ═════════►│
```

## Why

| | Discord screen share | Meshcast |
|---|---|---|
| Quality | 720p30 (1080p60 needs Nitro) | up to 1080p60, free |
| Path | via Discord's media servers | direct peer-to-peer (relay fallback only for NAT traversal) |
| Linux | Electron + PipeWire bugs | native PipeWire capture |
| Setup | none | install app once, `/link` once |
| Viewers | unlimited | 2–4 comfortably on home upload; more with a relay ([design notes](docs/DESIGN.md#scaling-to-more-viewers)) |

## Quick start

There are two roles: **one person runs the bot** (on any always-on Linux box — a homelab VM, a Proxmox LXC, a VPS, a Raspberry Pi) and **everyone who wants to stream or watch installs the desktop app**.

### 1. Run the bot (server admin, once)

1. Create a Discord application and bot token, and invite it to your server — 5 minutes, fully documented in **[docs/DISCORD-SETUP.md](docs/DISCORD-SETUP.md)**.
2. On your server, pick one:

   **One-liner (systemd service, prebuilt binary, no Rust needed):**
   ```bash
   curl -fsSL https://raw.githubusercontent.com/mattcree/meshcast/main/scripts/deploy-bot.sh | bash -s -- YOUR_DISCORD_TOKEN
   ```

   **Docker / Compose:**
   ```bash
   docker run -d --name meshcast-bot --restart unless-stopped \
     -e DISCORD_TOKEN=YOUR_DISCORD_TOKEN -v meshcast-bot:/data \
     ghcr.io/mattcree/meshcast-bot:latest
   ```
   (or grab [`packaging/bot/docker-compose.yml`](packaging/bot/docker-compose.yml))

   **Plain binary:** download `meshcast-bot-linux-x86_64.tar.gz` from [Releases](https://github.com/mattcree/meshcast/releases/latest) and run `DISCORD_TOKEN=… ./meshcast-bot`.

The bot needs only outbound network access (no ports to forward) and about 50 MB of RAM. It registers `/link`, `/unlink` and `/stream` automatically.

### 2. Install the app (everyone who streams or watches)

**Linux / macOS:**
```bash
curl -fsSL https://raw.githubusercontent.com/mattcree/meshcast/main/scripts/install.sh | bash
```
Installs to `~/.local/share/meshcast`, registers the app launcher, `meshcast://` links and a login autostart for the tray. No root. Uninstall with `~/.local/share/meshcast/uninstall.sh`.

**Windows (viewer only — see [platform support](#platform-support)):**
```powershell
irm https://raw.githubusercontent.com/mattcree/meshcast/main/scripts/install.ps1 | iex
```

Or download an archive from [Releases](https://github.com/mattcree/meshcast/releases/latest) and run `meshcast-app` yourself.

### 3. Link (once per person)

1. In Discord, type **`/link`** — the bot replies (only to you) with a pairing code.
2. Paste it into the Meshcast window and click **Link**.

That's it; the link survives restarts on both sides.

### 4. Stream and watch

- **Stream:** type **`/stream`** (optionally `/stream title:Game Night`), pick quality/FPS, click **Start**. Your Meshcast window pops up asking you to confirm (and on Wayland, GNOME asks which screen/window to share). The bot posts a card in the channel.
- **Watch:** click **Watch** on the card. If you're linked, the viewer opens by itself. If you're not, the bot tells you how to install and gives you the stream ticket for `meshcast watch <ticket>`.
- **Stop:** click **Stop** on the card, press **Stop Stream** in the app or tray, or run `/stream` again. The card updates to "Stream ended" and viewers see "Stream ended".

## What's running on your machine

| Process | Role |
|---|---|
| `meshcast daemon` | Long-lived. Keeps the gossip link to the bot, runs screen capture when you approve, launches viewer windows when you click Watch. |
| `meshcast-app` | The window: status, pairing, the "Share screen?" consent prompt, settings. Thin; closing it never stops a stream. Starts the daemon if needed. |
| tray icon | Linux: `meshcast-tray.py` (starts the daemon at login, shows live/connected state, quick Stop/Quit). macOS/Windows: built into `meshcast-app`. |
| `meshcast watch <ticket>` | One viewer window per stream you watch. |

Config lives in `~/.config/meshcast/config.toml` (Linux), `~/Library/Application Support/meshcast/` (macOS) or `%APPDATA%\meshcast\` (Windows):

```toml
[video]
quality = "720p"   # 360p, 720p, 1080p  (per-stream choice in /stream overrides this)
fps = 30           # 30 or 60
codec = "h264"

[audio]
enabled = true     # microphone. Desktop audio capture is on the backlog.

[[links]]          # one per Discord bot you've paired with (managed by the app)
name = "My Server"
```

### CLI

Everything the app does is also available from the `meshcast` binary (headless boxes, scripting, or just preference):

```bash
meshcast link <CODE>                      # pair with a bot (code from /link)
meshcast daemon                           # run the background daemon in the foreground
meshcast status                           # daemon / bot / stream state
meshcast stream --quality 1080p --fps 60  # manual stream, prints a ticket (no Discord needed)
meshcast watch <TICKET | meshcast://…>    # open the viewer
meshcast unlink [--name "My Server"]
```

## Platform support

| | Stream (capture) | Watch | Notes |
|---|---|---|---|
| **Linux** (Wayland & X11) | ✅ | ✅ | Primary target. PipeWire portal picker on Wayland. Tray needs `python3-gobject` + AppIndicator (on GNOME, the *AppIndicator and KStatusNotifierItem Support* extension). |
| **macOS** | ✅ (ScreenCaptureKit, untested by us) | ✅ | Binaries are unsigned: `install.sh` clears the quarantine flag. No `meshcast://` handler yet (needs an `.app` bundle — on the [backlog](BACKLOG.md)). |
| **Windows** | ❌ (not yet in iroh-live) | ✅ | `install.ps1` registers `meshcast://`. |

Viewers don't need anything besides the app; decoding is software H.264 and rendering is wgpu.

## Troubleshooting

- **"Your Meshcast app didn't respond"** — the daemon isn't running or isn't connected. Open the Meshcast window: the status pill should say *Connected*. If it says *Daemon not running*, click *Start daemon*; if *Waiting for bot…*, the bot is down or unreachable.
- **Tray icon missing on GNOME** — install the AppIndicator extension. The app and daemon work without the tray.
- **Watch does nothing** — you need to be linked (`/link`). The bot's reply tells you what's wrong; the daemon also shows the error in the window.
- **Logs** — run `meshcast daemon` in a terminal to see them (`RUST_LOG=debug` for more). Bot: `journalctl -u meshcast-bot -f` (or `--user`).
- **Choppy video** — drop to 720p or 30 fps in `/stream`; each viewer costs roughly the stream's bitrate in upload (≈5–10 Mbit/s at 1080p).

## Building from source

Meshcast is a Rust workspace. The bot builds anywhere with a C toolchain + cmake. The desktop crates need PipeWire, libva, ALSA, Vulkan/GBM and XKB headers on Linux.

```bash
# Fedora / Bluefin (inside a toolbox on immutable distros — the *binaries* run fine on the host)
sudo dnf install -y pipewire-devel clang-devel libva-devel nasm pkg-config cmake \
  alsa-lib-devel mesa-libgbm-devel mesa-vulkan-drivers libxkbcommon-devel gtk3-devel atk-devel

# Debian / Ubuntu
sudo apt install -y libpipewire-0.3-dev libspa-0.2-dev libclang-dev libva-dev nasm pkg-config \
  cmake libasound2-dev libgbm-dev libvulkan-dev libxkbcommon-dev libgtk-3-dev libatk1.0-dev

cargo build --release --workspace
# → target/release/{meshcast,meshcast-app,meshcast-bot}
```

See [CONTRIBUTING.md](CONTRIBUTING.md) for the dev loop, lint/test commands and the release process.

## Documentation

- [docs/DESIGN.md](docs/DESIGN.md) — architecture, the Discord integration and why it looks the way it does, trust model, scaling
- [docs/DISCORD-SETUP.md](docs/DISCORD-SETUP.md) — creating the Discord application and inviting the bot
- [CONTRIBUTING.md](CONTRIBUTING.md) — dev setup, standards, releasing
- [BACKLOG.md](BACKLOG.md) — what's next
- [CHANGELOG.md](CHANGELOG.md)
- [SECURITY.md](SECURITY.md)

## License

MIT — see [LICENSE](LICENSE). Built on [iroh-live](https://github.com/n0-computer/iroh-live), [iroh](https://github.com/n0-computer/iroh), [iroh-gossip](https://github.com/n0-computer/iroh-gossip), [poise](https://github.com/serenity-rs/poise)/[serenity](https://github.com/serenity-rs/serenity) and [egui](https://github.com/emilk/egui).
