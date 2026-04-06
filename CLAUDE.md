# CLAUDE.md

## Project

Meshcast — a Discord-integrated P2P screen streaming tool built on top of **iroh-live**, not from scratch. See SPEC.md for the full design.

## Critical Context

**iroh-live already exists** and handles the entire capture → encode → transport → decode → render pipeline. Do NOT build a custom streaming protocol, frame packetiser, encoder abstraction, or screen capture layer. The project scope is:

1. A Discord bot (post stream tickets as rich embeds)
2. A thin CLI wrapper (URI scheme handler + convenience around `irl`)
3. Optionally a Tauri GUI later

## iroh-live Reference

- **Repo**: https://github.com/n0-computer/iroh-live
- **CLI binary**: `irl` (in `iroh-live-cli` crate)
- **Key commands**: `irl publish --video screen`, `irl play <TICKET>`, `irl room`, `irl relay`
- **High-level API crate**: `iroh-live` — provides `Live`, `LocalBroadcast`, `LiveTicket`, capture sources
- **Transport**: MoQ (Media over QUIC) via `iroh-moq` + `moq-rs`
- **Codecs**: H.264 (openh264), AV1 (rav1e/rav1d), Opus — all software, with VA-API HW accel optional
- **Screen capture**: `xcap` crate + PipeWire on Linux, ScreenCaptureKit on macOS
- **Status**: Experimental but actively developed by n0 (the iroh company)
- **Windows**: Viewer/playback should work. Capture/publish is mostly missing. Audio (callme) works on all platforms.
- **Note**: Uses unreleased crate versions — build from git, copy their `[patch.crates-io]` section

### iroh-live API (publisher example from their docs)
```rust
let live = Live::from_env().await?.with_router().spawn();
let broadcast = LocalBroadcast::new();
// For screen capture, use the screen capture source instead of camera
let camera = CameraCapturer::new()?;
broadcast.video().set_source(camera, VideoCodec::H264, [VideoPreset::P720])?;
let audio = AudioBackend::default();
let mic = audio.default_input().await?;
broadcast.audio().set(mic, AudioCodec::Opus, [AudioPreset::Hq])?;
live.publish("hello", &broadcast).await?;
let ticket = LiveTicket::new(live.endpoint().addr(), "hello");
println!("{ticket}");
```

## Workspace Layout

```
meshcast/
├── CLAUDE.md
├── SPEC.md
├── Cargo.toml              # workspace root
├── crates/
│   ├── meshcast-bot/        # Discord bot — slash commands, embeds
│   │   ├── src/
│   │   │   └── main.rs
│   │   └── Cargo.toml
│   └── meshcast-cli/        # Wrapper CLI — URI handler, ticket management
│       ├── src/
│       │   └── main.rs
│       └── Cargo.toml
├── meshcast.desktop         # Linux URI scheme handler
└── scripts/
    └── install-uri.sh       # Registers meshcast:// URI scheme
```

## Tech Stack

- **Streaming**: `iroh-live` (git dependency) — does all the hard work
- **Discord bot**: `poise` (ergonomic framework on top of serenity)
- **CLI**: `clap` for argument parsing
- **Async runtime**: `tokio` (required by both iroh and serenity)
- **Logging**: `tracing`

## meshcast-bot Design

The bot has one slash command. A stream *is* a room — no separate concepts.

```
/stream [title]
  → User starts streaming from the meshcast app (or provides a ticket)
  → Bot posts embed with:
    - Title (default: "{user}'s Stream")
    - "Watch" button → meshcast://watch/<ticket>
    - Streamer's avatar
    - Timestamp footer
  → If another participant wants to publish into the same room, they can
  → /stream stop → updates embed to "Stream ended"
```

The bot needs: `SEND_MESSAGES`, `EMBED_LINKS` permissions. No voice permissions — streaming is entirely outside Discord.

## meshcast-cli Design

Two subcommands:

```bash
# Start streaming (wraps irl publish / irl room)
meshcast stream [--title "My Stream"] [--post-to-discord <webhook-url>]
  → Launches irl publish --video screen
  → Captures the ticket from stdout
  → Optionally POSTs ticket to bot's webhook endpoint
  → Prints ticket + URI

# Watch a stream (wraps irl play, also handles URI scheme)
meshcast watch <ticket-or-uri>
  → Parses meshcast://watch/<ticket> URIs
  → Launches irl play <ticket>
```

## Build Prerequisites (Bluefin / Fedora)

iroh-live has native dependencies for screen capture and encoding:

```bash
# In a toolbox (recommended for Bluefin)
toolbox create meshcast && toolbox enter meshcast

# iroh-live build deps
sudo dnf install pipewire-devel libspa-devel clang-devel libva-devel nasm pkg-config
```

## Code Style

- Use `anyhow` for error handling (this is a thin application layer, not a library)
- Use `tracing` for logging
- Keep it simple — the bot is < 200 lines, the CLI wrapper is < 200 lines
- Don't over-abstract. This is glue code.

## What NOT to Do

- **Don't build a streaming protocol.** iroh-live + MoQ handles this.
- **Don't build an encoder/decoder.** iroh-live wraps openh264/rav1e/opus.
- **Don't build a screen capture abstraction.** iroh-live wraps xcap + PipeWire.
- **Don't build NAT traversal.** iroh handles this.
- **Don't add WebRTC.** The whole point is using iroh/QUIC instead.
- **Don't try to stream through Discord's voice infrastructure.** Bot video streaming is not officially supported. We stream outside Discord and use Discord only for discovery.
- **Don't create separate /stream and /room commands.** A stream is a room. One command.

## Phase 1: Just Test iroh-live First

Before writing any code, validate that iroh-live actually works:

```bash
# Terminal 1: publish screen
irl publish --video screen

# Terminal 2 (different machine): watch
irl play <TICKET>
```

Check: Does PipeWire capture work? Is latency acceptable? Is text readable? Does it work across NATs? Can a Windows friend run `irl play`?

**Only proceed to building meshcast-bot and meshcast-cli once Phase 1 passes.**
