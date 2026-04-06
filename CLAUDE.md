# CLAUDE.md

## Project

Meshcast — a Discord-integrated P2P screen streaming tool built on top of **iroh-live**, not from scratch. See SPEC.md for the full design.

## Critical Context

**iroh-live already exists** and handles the entire capture → encode → transport → decode → render pipeline. Do NOT build a custom streaming protocol, frame packetiser, encoder abstraction, or screen capture layer. The project scope is:

1. A Discord bot (post stream tickets as rich embeds with Watch button)
2. A desktop app (tray icon + config UI + daemon that responds to bot signals)
3. A CLI for headless/manual use

## Architecture

```
Discord         Bot (homelab)              Desktop App / CLI Daemon
  │                │                            │
  │ /link          │                            │
  ├───────────────►│ generate gossip topic      │
  │◄───token───────┤                            │
  │                │                            │
  │   (user pastes token into app or CLI)       │
  │                │◄────gossip connected───────►│
  │                │                            │
  │ /stream        │                            │
  ├───────────────►│──StartStream──────────────►│ captures screen
  │                │◄─StreamReady{ticket}───────┤
  │◄──embed+Watch──┤                            │
  │                │                            │
  │ click Watch    │                            │
  ├───────────────►│──WatchStream{ticket}──────►│ opens viewer
  │                │◄─ViewerUpdate{count}───────┤
  │                │                            │
  │ /stream stop   │──StopStream───────────────►│
  │◄──"Ended"──────│◄─StreamStopped────────────┤
```

All signaling uses **iroh-gossip** over QUIC with NAT traversal. Discord buttons are interactive (not URL-based — Discord rejects custom URI schemes).

## Workspace Layout

```
meshcast/
├── CLAUDE.md
├── SPEC.md
├── Cargo.toml              # workspace root (4 crates)
├── crates/
│   ├── meshcast-signal/     # Shared types: Signal enum, PairToken, LinkState, AppConfig, SignalNode
│   ├── meshcast-bot/        # Discord bot — /link, /stream, Watch button handler
│   ├── meshcast-cli/        # CLI — stream, watch, link, daemon, unlink
│   └── meshcast-app/        # Desktop app — egui config UI + merged daemon
├── meshcast.desktop         # Linux URI scheme handler (fallback)
└── scripts/
    └── install-uri.sh
```

## Tech Stack

- **Streaming**: `iroh-live` (git dependency) — capture, encode, transport, decode, render
- **Signaling**: `iroh-gossip` — bot-to-app communication
- **Discord bot**: `poise` 0.6 (on serenity 0.12)
- **Desktop app**: `eframe`/`egui` + `tray-icon`
- **CLI**: `clap` 4
- **Config**: TOML (`~/.config/meshcast/config.toml`)
- **Async runtime**: `tokio`
- **Logging**: `tracing`

## meshcast-signal (shared crate)

- `Signal` enum: `StartStream`, `StreamReady`, `StopStream`, `StreamStopped`, `WatchStream`, `ViewerUpdate`, `Ping`, `Pong`
- `PairToken`: gossip topic + endpoint address, base32-encoded with `meshcast1` prefix
- `LinkState`: persisted topic/secret_key/peer_id for reconnection
- `AppConfig`: video quality/fps/codec, audio toggle, link config — stored as TOML
- `BotLinkStore`: bot's persisted links + secret key
- `SignalNode`: lightweight iroh Endpoint + Gossip + Router wrapper

## meshcast-bot

Single binary, runs on homelab. Slash commands:
- `/link` — generates pairing token, subscribes to gossip topic
- `/stream [title] [ticket] [stop]` — signals linked app to start capture, or accepts manual ticket
- Watch button (interactive) — sends `WatchStream` to viewer's linked app via gossip

Persists links to `~/.config/meshcast-bot/state.json`. Restores on startup.

## meshcast-cli

Five subcommands:
- `meshcast stream [--name] [--no-audio] [--quality]` — direct screen capture + publish
- `meshcast watch <ticket-or-uri>` — egui viewer with software H.264 decode
- `meshcast link <token>` — pair with Discord bot
- `meshcast daemon` — long-running gossip listener, responds to bot signals
- `meshcast unlink` — remove pairing

## meshcast-app

Desktop application (egui window). Merges daemon + config UI:
- Status indicator (streaming/connected/linked/unlinked)
- Link token input for pairing
- Quality selector (360p/720p/1080p), FPS (30/60), audio toggle
- Viewer count display
- Background gossip listener handles all signals

## Build Prerequisites (Bluefin / Fedora)

```bash
# In a toolbox (required for Bluefin)
toolbox create meshcast && toolbox enter meshcast

sudo dnf install -y \
  pipewire-devel clang-devel libva-devel nasm pkg-config cmake \
  alsa-lib-devel mesa-libgbm-devel mesa-vulkan-drivers \
  libxkbcommon libxkbcommon-devel gtk3-devel atk-devel

# Build
cargo build --workspace --release
```

## Key Dependencies (pinned)

- `iroh` pinned to rev `8af8370` (main branch had hickory API breakage)
- `iroh-gossip` on branch `deps/iroh-main`
- `iroh-live` on branch `main`
- Full `[patch.crates-io]` section in workspace Cargo.toml

## Code Style

- `anyhow` for error handling
- `tracing` for logging
- Keep it simple — this is glue code
- Don't over-abstract

## What NOT to Do

- Don't build a streaming protocol — iroh-live + MoQ handles this
- Don't build an encoder/decoder — iroh-live wraps openh264/rav1e/opus
- Don't build a screen capture abstraction — iroh-live wraps xcap + PipeWire
- Don't use URL buttons in Discord — they reject custom URI schemes
- Don't create separate /stream and /room commands — a stream is a room
