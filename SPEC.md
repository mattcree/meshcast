# Meshcast — P2P Screen Streaming for Discord via iroh-live

## Problem

Discord's screen sharing is unreliable on Linux (PipeWire/Wayland issues, Electron capture bugs), capped at 720p30 without Nitro, and routes all video through Discord's servers adding unnecessary latency. There is no good way to use Discord as the social/discovery layer while streaming via a better transport.

## Solution

A thin wrapper around **iroh-live** (which already handles capture, encoding, transport, and rendering) that adds Discord bot integration for session discovery. The heavy lifting is done — iroh-live already provides:

- **Screen capture**: PipeWire (Wayland/Linux), X11, Apple ScreenCaptureKit
- **Camera capture**: V4L2, AVFoundation, nokhwa
- **Video encoding**: H.264 (openh264 software), AV1 (rav1e), with VA-API hardware acceleration
- **Audio**: Opus encoding/decoding
- **Transport**: Media over QUIC (MoQ) via iroh — P2P with NAT traversal, relay fallback
- **Adaptive bitrate**: multiple renditions (e.g. 360p + 720p), on-demand switching
- **Rendering**: wgpu-based frame display
- **CLI**: `irl publish --video screen` / `irl play <TICKET>` / `irl room`
- **Browser playback**: WebTransport relay for browser-based viewers
- **Android**: bidirectional video calling demo

The transport uses MoQ, where each video rendition and audio track is an independent QUIC stream — dropped video packets never block audio delivery.

### Platform support note

iroh (networking) is fully cross-platform. iroh-live's *viewer/playback* path (decode + render) should work everywhere since it's software decode + wgpu. However, iroh-live's *capture/publish* side has gaps on Windows — the platform-specific screen capture APIs (DXGI Desktop Duplication) aren't wired up yet. Audio-only streaming (callme) is confirmed working on Linux, macOS, and Windows. The viewer side is the important part for cross-platform — your friends just need to watch, and that should work on any OS.

## Key Insight: Don't Reinvent iroh-live

The original design spec proposed a custom protocol (video on QUIC datagrams, audio on reliable streams, custom packet format with frame chunking). **iroh-live already solves all of this** using MoQ (Media over QUIC), a proper IETF-track media transport protocol. Their implementation handles:

- Frame packetisation and chunking
- Independent stream priorities (audio never blocked by video)
- Adaptive bitrate with rendition switching
- Hardware encoder detection and fallback
- Cross-platform capture abstraction
- Software codecs (H.264 via openh264, AV1 via rav1e/rav1d, Opus) on every platform Rust targets

Building a custom protocol on raw iroh datagrams would be reimplementing moq-rs poorly. Use the existing stack.

### iroh-live workspace structure (for reference)

- **`iroh-moq`** — Adapters to create/accept moq-lite sessions over iroh
- **`moq-media`** — Codec implementations (H.264, AV1, Opus), VA-API/V4L2/VideoToolbox HW accel, wgpu rendering, cross-platform screen/camera capture
- **`iroh-live`** — High-level API: `Live`, `LocalBroadcast`, capture sources, publish/subscribe
- **`iroh-live-cli`** — The `irl` binary: publish, play, call, room, relay subcommands

### iroh-live API example (from their docs)

```rust
// Publisher
let live = Live::from_env().await?.with_router().spawn();
let broadcast = LocalBroadcast::new();
let camera = CameraCapturer::new()?;
broadcast.video().set_source(camera, VideoCodec::H264, [VideoPreset::P720])?;
let audio = AudioBackend::default();
let mic = audio.default_input().await?;
broadcast.audio().set(mic, AudioCodec::Opus, [AudioPreset::Hq])?;
live.publish("hello", &broadcast).await?;
let ticket = LiveTicket::new(live.endpoint().addr(), "hello");
println!("{ticket}");
```

For screen capture specifically: `--video screen` flag on the CLI, or programmatically via the screen capture source APIs.

## What We Actually Need to Build

### 1. Discord Bot (`meshcast-bot`)
A simple bot that bridges iroh-live tickets into Discord's social layer.

**Single slash command:**
- `/stream [title]` — User provides a ticket (or starts the stream from the app which auto-posts). Posts a rich embed in the channel with:
  - Title (default: "{user}'s Stream")
  - "Watch (Native)" button → `meshcast://watch/<TICKET>` (opens native app via URI scheme)
  - "Watch (Browser)" button → relay URL (if relay is running)
  - Streamer's name + avatar
  - "Started streaming" footer with timestamp
- When the streamer stops (disconnects or runs `/stream stop`), the embed updates to "Stream ended"

A stream *is* a room. If someone else in the channel also wants to share their screen, they just publish into the same room. The underlying iroh-live room concept supports multiple publishers natively — there's no need for separate `/stream` and `/room` commands.

**Tech**: `poise` (ergonomic Discord framework built on serenity). Lightweight, stateless — the bot just formats tickets into embeds.

### 2. Wrapper CLI / App (`meshcast`)
A thin layer over `irl` that:
- Runs `irl publish --video screen` (or `irl room`) under the hood
- Registers a custom URI scheme (`meshcast://watch/<TICKET>`)
- Auto-copies the ticket and optionally pushes it to the Discord bot via a simple HTTP call or webhook
- Provides a streamlined UX: `meshcast stream` → captures screen, prints ticket, optionally posts to Discord channel

This could be a shell script wrapping `irl` for v1. Or a small Rust binary that depends on iroh-live as a library.

### 3. Custom URI Handler
Platform registration so clicking `meshcast://watch/<TICKET>` in Discord opens `irl play <TICKET>` automatically.

**Linux** (primary target):
```ini
# ~/.local/share/applications/meshcast.desktop
[Desktop Entry]
Name=Meshcast
Exec=meshcast watch %u
Type=Application
MimeType=x-scheme-handler/meshcast;
NoDisplay=true
```
Then: `xdg-mime default meshcast.desktop x-scheme-handler/meshcast`

**macOS**: `Info.plist` URL scheme registration
**Windows**: Registry key under `HKEY_CLASSES_ROOT\meshcast`

### 4. Optional: Tauri GUI
If the CLI flow feels too rough, wrap it in a Tauri app with:
- "Start Streaming" button (screen picker → publish → show ticket/QR)
- "Watch" input (paste ticket → play)
- System tray with quick actions
- Integration with Discord bot (auto-post ticket to channel)

## Architecture

```
┌─────────────────────────────────────────────────────┐
│              iroh-live (existing, maintained by n0)  │
│                                                     │
│  capture → encode → MoQ/iroh → decode → render      │
│  (PipeWire)  (H.264/AV1)  (P2P QUIC)  (wgpu)      │
│  (X11)       (VA-API HW)                            │
│  (SCKit)     (Opus)                                  │
│                                                     │
│  Features: adaptive bitrate, multi-rendition,        │
│  relay fallback, browser WebTransport, rooms         │
└──────────────────┬──────────────────────────────────┘
                   │ ticket string (contains endpoint ID + broadcast name)
        ┌──────────┴──────────┐
        │                     │
┌───────▼───────┐    ┌───────▼────────┐
│  meshcast     │    │  meshcast-bot  │
│  (wrapper)    │    │  (Discord)     │
│               │    │                │
│ - URI scheme  │    │ - /stream cmd  │
│ - ticket mgmt │    │ - rich embed   │
│ - auto-post   │    │ - watch link   │
└───────────────┘    └────────────────┘
```

## Phased Build Plan

### Phase 1: Validate iroh-live works for your use case (~1 evening)

This is the most important phase. If iroh-live doesn't work well, everything else is moot.

1. Clone iroh-live, build the `irl` CLI on Bluefin (in a toolbox if needed)
2. Run `irl publish --video screen` on your machine
3. Run `irl play <TICKET>` on another machine (or have a friend run it) over the internet
4. Measure: latency (is it perceptibly better than Discord?), quality (is text readable?), stability
5. Test `irl room` for multi-participant
6. Test with `--video-presets 720p,1080p` for adaptive bitrate
7. Check PipeWire screen capture works on your Bluefin/GNOME setup
8. Have a Windows friend test `irl play` (viewer should work even if publish doesn't)

**If this works well** → Phase 2. You've validated the core and saved months of work.
**If it doesn't** → file issues upstream, or fork and fix specific problems. Still better than building from scratch.

Build deps for Bluefin:
```bash
# In a toolbox container (recommended for immutable distro)
toolbox create meshcast
toolbox enter meshcast
sudo dnf install pipewire-devel libspa-devel clang-devel libva-devel nasm pkg-config
git clone https://github.com/n0-computer/iroh-live.git
cd iroh-live
cargo build --release -p iroh-live-cli
# Binary at target/release/irl
```

### Phase 2: Discord bot (~1-2 evenings)
1. Create Discord application + bot in developer portal
2. Implement `/stream` slash command with `poise`
3. Bot formats ticket into a rich embed with a clickable "Watch" button
4. Deploy bot on your Proxmox homelab (new LXC, tiny resource footprint)

### Phase 3: URI scheme + wrapper (~1 evening)
1. Create `.desktop` file for `meshcast://` URI scheme on Linux
2. Write small wrapper script/binary that parses the URI and launches `irl play`
3. Test end-to-end: `/stream` in Discord → click button → native viewer opens and connects

### Phase 4: Polish (if needed)
- Tauri GUI if CLI flow is too rough for friends
- Self-hosted iroh-live relay on homelab for browser fallback
- Desktop audio capture (check iroh-live's current support — may need PipeWire monitor source)
- System tray integration
- Auto-start on login

## Risks & Open Questions

| Risk | Impact | Mitigation |
|------|--------|------------|
| iroh-live is "experimental / work in progress" | API churn, possible bugs | Actively developed by n0. Pin to a working commit. Community is small but responsive. |
| PipeWire screen capture portal permissions | Same headaches as OBS on Wayland | iroh-live uses `xcap` crate. Test in Phase 1. On GNOME/Bluefin portals should work. |
| Desktop audio capture may not work out of the box | No system audio in stream | PipeWire monitor source should work. May need to configure `--audio` flag or patch. |
| iroh-live uses unreleased crate versions | Need `[patch.crates-io]` in Cargo.toml | Documented in their repo. Build from git source, not crates.io. |
| Discord may not allow custom URI schemes in embed buttons | Can't click-to-watch from Discord | Fallback: bot posts the ticket as text, user copies and pastes. Or use a web URL that redirects. |
| Multi-viewer bandwidth scales linearly on streamer's upload | Can't stream to 10+ people on typical residential upload | iroh-live relay server can fan out. Deploy on homelab or cheap VPS. |
| Windows publish/capture not yet supported in iroh-live | Windows friends can't stream (but can watch) | Viewer path should work. Publish support will come upstream. |

## What NOT to Build

- A custom QUIC datagram protocol for video — MoQ already handles this properly
- A custom frame packetiser — moq-rs does this
- A custom encoder abstraction — iroh-live's `moq-media` / `rusty-codecs` already wraps ffmpeg + openh264 + rav1e
- A custom screen capture layer — iroh-live already wraps xcap + PipeWire + ScreenCaptureKit
- A custom NAT traversal system — iroh does this
- Separate `/stream` and `/room` commands — a stream is a room, one command handles both

## Success Criteria

The project is "done" when:
1. You can type `/stream` in your Discord server
2. The bot posts a card with a "Watch" button
3. Friends click it, your app opens, they see your screen in high quality with low latency
4. It works reliably on your Bluefin system without paying for Discord Nitro
5. Total custom code written is < 500 lines (because iroh-live does the hard work)

## Links

- iroh-live: https://github.com/n0-computer/iroh-live
- iroh (networking): https://github.com/n0-computer/iroh
- iroh docs: https://docs.iroh.computer
- moq-rs (MoQ implementation): https://github.com/moq-dev/moq
- poise (Discord bot framework): https://github.com/serenity-rs/poise
- Discord Embedded App SDK (if browser Activity route is pursued later): https://github.com/discord/embedded-app-sdk
