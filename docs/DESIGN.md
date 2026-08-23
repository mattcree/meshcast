# Meshcast design

This document explains what Meshcast is, the shape of the system, and — because the whole point is "Discord integration" — exactly what Discord does and doesn't let an app do, and why the design lands where it does. Read this before changing the protocol, the bot UX or the process model.

## 1. Goal and non-goals

**Goal.** Let a group of friends on a Discord server share screens at good quality and low latency, with the least possible friction: no Nitro, one-time install, one command to go live, one click to watch.

**Non-goals.**
- Reimplementing any part of the media pipeline. Capture, encoding, transport (MoQ over iroh/QUIC), decoding and rendering are all [iroh-live](https://github.com/n0-computer/iroh-live). Meshcast is glue.
- Being a general-purpose Discord bot. It has three commands.
- Replacing Discord voice. Audio today is microphone-only and secondary; desktop audio is on the backlog.

## 2. What Discord actually allows

This is the part that constrains everything. Checked against the Discord developer docs (August 2026).

| Capability | Available? | Consequence for Meshcast |
|---|---|---|
| Slash commands, ephemeral replies | Yes | `/link` can hand out a secret pairing code that only the requester sees. |
| Message components: buttons, select menus | Yes (`custom_id` ≤ 100 chars, label ≤ 80, 5 buttons/row) | The stream card gets **Watch** / **Stop** buttons; the setup card gets quality/FPS selects. State must live on the bot, not in `custom_id`. |
| Link buttons | Only `http(s)://` (and `discord://`) | A `meshcast://watch/<ticket>` button is rejected. Meshcast routes **Watch** through the bot (see §4.4) and additionally offers **Open in app**: an https GitHub Pages page (`docs/watch/`) that bounces to `meshcast://watch/<ticket>` with the ticket in the URL fragment (never sent to a server). |
| Embedding a web app *inside* Discord (Activities / Embedded App SDK) | Yes, but: runs in a sandboxed iframe behind Discord's proxy, which **only allows WebSockets** — no WebRTC, no WebTransport, no UDP. | An in-Discord viewer can't speak iroh/MoQ/QUIC. A pure-WebSocket relay + WebCodecs player would be a new media path, i.e. the thing we said we wouldn't build. Not viable today; revisit if Discord enables WebTransport. |
| Bot-initiated messages in a channel | Yes, with `Send Messages` + `Embed Links` in that channel | The bot posts the public stream card and later edits it to "ended". |
| Editing the original ephemeral reply after an interaction | Yes (interaction token valid 15 min) | The "Requesting… approve in your app" message can be updated with the outcome. |
| User-installable apps (install to *your account*, use in any server/DM) | Yes | Optional for admins who want one hosted bot usable everywhere; replies are constrained by the invoking user's permissions. Documented in DISCORD-SETUP. |
| Custom protocol handlers from Discord's client | No | The desktop client will not launch `meshcast://` from a message; only a browser will, via an https page. |
| Privileged intents | Not needed | Meshcast runs with non-privileged intents only. |

Conclusion: the right integration surface is **a bot with slash commands and components**, and the right "open the viewer" mechanism is **a side channel to a desktop daemon**, because Discord itself can't launch a native app. That's the design.

## 3. Architecture

```
          Discord (social layer)                         iroh network (data layer)
 ┌──────────────────────────────────┐       ┌──────────────────────────────────────────┐
 │ /link  /stream  [Watch] [Stop]   │       │  iroh-gossip topics (one per user↔bot)   │
 └───────────────┬──────────────────┘       │  iroh-live MoQ broadcasts (one per stream)│
                 │ Discord gateway/REST      └───────────┬────────────────┬─────────────┘
         ┌───────▼────────┐   gossip (QUIC, NAT-traversed) │                │
         │  meshcast-bot  │◄───────────────────────────────┤                │
         │ (any Linux box)│                                │                │
         └────────────────┘                                │                │
                                                 ┌─────────▼──────┐  ┌──────▼─────────┐
                                                 │ meshcast daemon│  │ meshcast daemon│
                                                 │  (streamer PC) │  │  (viewer PC)   │
                                                 │ capture+publish│══►│ `meshcast watch`│
                                                 └──┬──────────┬──┘  └──┬─────────────┘
                                      state/cmd files│          │       │
                                       ┌─────────────▼─┐  ┌─────▼────┐  │
                                       │ meshcast-app  │  │ tray     │  …
                                       │ (window)      │  │ (Linux py│
                                       └───────────────┘  │ /built-in)│
                                                          └──────────┘
```

### 3.1 Crates

| Crate | Binary | Responsibility |
|---|---|---|
| `meshcast-signal` | (lib) | The contract: `Signal`/`PairSignal` wire types, pairing code + topic derivation, `AppConfig`, bot link store, file IPC (`ipc`), process helpers (`process`), `SignalNode` (iroh endpoint + gossip + router). Fully unit-tested. |
| `meshcast-bot` | `meshcast-bot` | Discord commands/components; one gossip topic per linked user; posts and edits stream cards. Stateless apart from `state.json` (its iroh identity + per-user topics). |
| `meshcast-cli` | `meshcast` | `daemon` (the brain on a user's machine), `watch` (viewer window), plus manual `stream`/`link`/`unlink`/`status`. |
| `meshcast-app` | `meshcast-app` | Thin egui window. Reads the daemon's state file, writes commands. Owns the tray on macOS/Windows. |

### 3.2 Why the daemon is separate from the window

Early versions ran networking inside the GUI process; closing the window killed the stream, and the GUI's event loop and the tokio runtime fought over the main thread. Now:

- `meshcast daemon` is the only process that touches the network. It's long-lived and headless.
- `meshcast-app` is disposable. It can be closed and reopened freely; it starts the daemon if one isn't running.
- On Linux the tray is a Python/GTK script because GTK (AppIndicator) and winit (egui) both need the main thread and can't share a process, and because on immutable distros the tray must run on the host session's D-Bus while the Rust binaries may have been built in a container. State is exchanged through two tiny files (see 3.3), so the tray stays ~250 lines and has no Rust dependency.

### 3.3 Local IPC: files, deliberately

Daemon → GUI/tray: `~/.config/meshcast/.tray-state` (JSON `DaemonState`), rewritten atomically (temp + rename) on every change.
GUI/tray → daemon: `~/.config/meshcast/.tray-cmd`, one command (`stop`, `approve` / `approve:control`, `reject`, `reload`, `link:<code>`, `grant`, `deny`, `revoke`), consumed and deleted by the daemon on a 250 ms tick.
Liveness: `.daemon-pid` / `.app-pid`, checked with `kill(pid, 0)` (Unix) / `OpenProcess` (Windows).

This is not elegant, but it is trivially debuggable (`cat` the file), works identically on three OSes and from a Python script, needs no socket permissions, and the command volume is a handful of events per hour. A local socket would buy nothing users can feel. If the command set grows (e.g. per-viewer control) this should become a Unix socket/named pipe; noted in the backlog.

## 4. Protocols

### 4.1 Identity and transport

Every process that talks to the network is an iroh endpoint with an ed25519 identity. The bot persists its key (`state.json`), so its endpoint ID is stable and can be embedded in pairing codes. The daemon persists a key per link (`config.toml`) and uses the first one as its identity. All traffic is QUIC with TLS 1.3 between endpoint keys; NAT traversal and relay fallback come from iroh's n0 defaults (`presets::N0`).

Signalling uses **iroh-gossip**: each bot↔user link is a private gossip topic identified by a random 32-byte `TopicId`. Knowing the topic is the credential.

### 4.2 Pairing (`/link`)

```
user      Discord         bot                                   daemon
 │ /link ───►│             │                                       │
 │           │──────────►  │ pin = 8 chars (40 bit), topic = rand  │
 │           │             │ subscribe(pairing_topic = BLAKE3(pin))│
 │◄ code ────│◄────────────│ code = base32(bot_id)-…-pin           │
 │ paste code into app ───────────────────────────────────────────►│
 │           │             │◄── subscribe(pairing_topic, [bot_id]) ─┤
 │           │             │◄── PairRequest{pin} ───────────────────┤
 │           │             │ check pin (one-shot, 10 min TTL)       │
 │           │             │─── PairAccepted{topic, server_name} ──►│ save link
 │           │             │ subscribe(topic); persist BotLink      │ subscribe(topic,[bot_id])
```

- The pairing topic is derived with `blake3::derive_key("meshcast pairing topic v1", PIN)`. It used to be std's `DefaultHasher`, which is not guaranteed stable across Rust versions — bot and app built with different toolchains could silently never meet. Pinned by a unit test.
- The PIN is one-shot and expires in 10 minutes. An attacker who guesses a live PIN *and* knows the bot's endpoint ID within that window could hijack that pairing; 40 bits over a 10-minute window against a rate-limited gossip join is acceptable for a friends-server tool. The bot's ID is not secret (it's in every code), so the PIN is the only secret — hence 8 chars, not 4.
- The real topic travels only inside the pairing topic, over QUIC/TLS.

### 4.3 Steady state (`Signal`)

`Signal` is a `postcard`-encoded enum. **Variant order is the wire format**: append, never reorder (there is a test pinning indices).

| Direction | Signal | When |
|---|---|---|
| bot → app | `StartStream{title, quality, fps, server}` | user clicked Start on the setup card |
| app → bot | `StreamReady{ticket}` | user approved, capture running |
| app → bot | `StreamFailed{reason}` | user declined, timed out (90 s), capture failed, or already streaming |
| bot → app | `StopStream` | Stop button / `/stream` again |
| app → bot | `StreamStopped` | stream ended for any reason (also on daemon shutdown, SIGTERM included) |
| bot → app | `WatchStream{ticket}` | viewer clicked Watch |
| bot → app | `ViewerUpdate{count}` | viewer count changed (to the streamer) |
| both | `Ping`/`Pong` | reserved |
| bot → app | `ControlRequest{request_id, viewer}` | a viewer clicked Request control |
| app → bot | `ControlGranted{request_id, token, addr}` / `ControlDenied{request_id, reason}` | streamer's decision (or timeout / unavailable) |
| bot → app | `ControlToken{ticket, token, addr, streamer}` | to the *viewer's* daemon: go and connect |
| app → bot | `ControlRevoked` / bot → app `RevokeControl` | control ended / streamer pressed Revoke on the card |
| app → bot | `ControlAvailable{available}` | sent just before `StreamReady`; whether the card gets a Request control button |

Remote control itself (input events) does not go over gossip — see `docs/REMOTE-CONTROL.md` for the direct control channel.

The daemon only accepts messages delivered *directly* (neighbours scope) by the bot's endpoint ID for that link; the bot only accepts messages delivered directly by the app endpoint it paired with (recorded at pairing; links made before 0.7 accept any sender on the topic until re-paired). Swarm-relayed messages are ignored on both sides, so knowing a topic is not enough to impersonate either party. All signals are idempotent or safely ignorable; duplicates do no harm. The daemon queues state-bearing signals while the bot is unreachable and flushes them on reconnect, and re-joins the bot with backoff (iroh-gossip does not retry bootstrap on its own).

### 4.4 Streaming and watching

- On approve, the daemon creates a fresh iroh-live `Live` endpoint, sets the screen capturer as the H.264 source (openh264, preset from quality, custom framerate rendition for 60 fps), optionally the default audio input as Opus, publishes the broadcast and returns the `LiveTicket` (endpoint address + broadcast name). The ticket is the only thing the bot needs.
- The bot posts the card with `Watch` (`custom_id = watch:<streamer_id>`) and `Stop` (`stop:<streamer_id>`). Keying by streamer means two people can stream in one channel and the lookup is exact.
- Watch → bot sends `WatchStream{ticket}` to the *viewer's* daemon → daemon spawns `meshcast watch <ticket>` detached. The daemon keeps the `Child` handles, reaps them, and caps concurrently open viewer windows at 5 (the original code counted up and never down, so Watch silently died after five clicks).
- Viewers without a link get an ephemeral message with the install link and the raw ticket, so nobody is stuck.

Each viewer is a separate QUIC session to the streamer, so upload scales linearly with viewers (§6).

## 5. Trust model and security posture

- **Who can make my screen stream?** Only a bot I've paired with, and only after I click *Share Screen* in the app (plus the OS portal picker on Wayland). Requests expire after 90 s. Stream requests show the server name so a request from an unexpected server is obvious.
- **Who can make my machine open a viewer?** Anyone who can click Watch in a channel where a paired bot posted a card. The viewer only ever connects to the ticket's endpoint and renders video; tickets are validated for character set and length before spawning, and viewer windows are capped at 5.
- **Bot compromise** = ability to request streams (still gated by consent), open viewers for linked users, and — for a stream where the streamer clicked Allow on a forged control request — drive the desktop until revoked (see `docs/REMOTE-CONTROL.md`). Unlink in the app cuts this. Pairing PINs are checked only against the one pairing topic they belong to and the topic closes after 3 wrong attempts, so a `/link` holder can't probe other users' codes.
- **Addresses**: stream tickets and control grants carry relay-only addresses, so a channel full of strangers doesn't learn the streamer's home IP; peers that actually connect still learn each other's addresses (inherent to P2P).
- **Secrets on disk**: `config.toml` (link topics + daemon keys), bot `state.json` (bot key + topics), bot token env file. All written `0600`, atomically. Never log them.
- **Discord-side abuse**: titles are sanitised and capped (80 chars); all bot state is per-user in memory; no privileged intents; no message content access.
- Known gap: stream tickets are visible to anyone who can ask the bot for them (unlinked-viewer fallback), so "private" streams are as private as the channel. A role-gated Watch is on the backlog.

## 6. Scaling to more viewers

P2P means every viewer costs the streamer one copy of the stream. iroh-live's default bitrate is `pixels × 0.07 × (30 + (fps−30)/2)`, so per viewer (video + 128 kbit/s audio, ~10 % transport/keyframe overhead):

| Preset | ≈ upload / viewer | Viewers on a 10 Mbit/s upload |
|---|---|---|
| 360p30 | ~0.6 Mbit/s | many |
| **720p30 (default)** | ~2.5 Mbit/s | 3–4 |
| 720p60 | ~4 Mbit/s | 2 |
| 1080p30 | ~5 Mbit/s | 1–2 |
| 1080p60 | ~7–8 Mbit/s | 1 |

So 1080p60 is effectively a one-viewer preset on a typical home upload; a friends-server audience of 4–8 wants 720p30. Encoding is hardware (VAAPI on Linux, VideoToolbox on macOS) when a usable device is present — `video.codec = auto` (default; also `h264` to force software, `h264-vaapi`/`h264-vtb` to force hardware) — otherwise software openh264, which is ~1–1.5 CPU cores at 1080p and marginal at 1080p60.

Beyond a handful of viewers the answer is a fan-out relay (iroh-live ships `irl relay`): the streamer publishes once to the relay, viewers subscribe to the relay, and upload stays constant. The relay needs its own upstream bandwidth (a VPS, not the homelab on the same home connection). Integrating "publish via relay" into `/stream` is a backlog item; nothing in the protocol needs to change (the ticket would point at the relay).

## 7. Failure modes and what happens

| Failure | Behaviour |
|---|---|
| Bot offline when daemon starts | Daemon subscribes without blocking, shows *Waiting for bot…*, connects when it appears. |
| Daemon offline when `/stream` clicked | Bot edits the ephemeral message: "Your Meshcast app didn't respond…" after 100 s; no public card is posted. |
| User ignores consent prompt | Daemon sends `StreamFailed("No response…")` after 90 s; bot reports it. |
| Daemon killed (SIGTERM/Ctrl-C) mid-stream | Sends `StreamStopped` first → card becomes "Stream ended". SIGKILL/power loss: card stays live until the streamer runs `/stream` → Stop (bot still knows it). |
| Bot restarts mid-stream | Bot forgets the card (in-memory). Stream keeps running; `/stream` offers a fresh start. Persisting active cards is on the backlog. |
| Link lost / `/unlink` | Bot drops the topic; app shows *Waiting for bot…*. `/link` again. |
| Two streams requested | Daemon rejects `StartStream` while live (`StreamFailed("Already streaming")`); bot also refuses in `/stream` and offers Stop. |
| Watch with 5 viewer windows open | Daemon ignores and records an error visible in the app. |

## 8. Alternatives considered

- **`meshcast://` URL handler as the primary Watch path.** Simplest mentally, but Discord can't put it on a button, the desktop client never launches custom schemes, and macOS needs an `.app` bundle to register a scheme. Kept as a *secondary* path (installed on Linux/Windows) plus the copy-paste ticket; an https redirect page (GitHub Pages) to bridge it is on the backlog.
- **Web viewer / Discord Activity.** Blocked by the WebSocket-only proxy (§2). Would also need a public relay with TLS. Revisit if WebTransport lands in Activities.
- **Webhook/HTTP from app to bot instead of gossip.** Requires the bot to be publicly reachable (port forwarding, TLS, auth). Gossip over iroh gives NAT traversal, encryption and identity for free and needs zero inbound ports on the homelab.
- **One shared public bot hosted by the project.** Zero setup for admins, but a hosting commitment and a central point that knows everyone's topics. The design supports it (user-installable app + one bot), but the default remains self-host.
- **Tray icon in the Rust app on Linux.** Tried; GTK/winit main-thread conflict and container D-Bus isolation made it fragile. Python script is ugly but robust.
- **Local socket IPC.** See 3.3.

## 9. Platform matrix (current)

| | Linux | macOS | Windows |
|---|---|---|---|
| Capture | PipeWire portal / X11 (iroh-live) | ScreenCaptureKit (iroh-live, untested here) | not in iroh-live yet |
| Viewer | ✅ | ✅ | ✅ |
| Tray | Python AppIndicator (host) | built-in `tray-icon` | built-in `tray-icon` |
| `meshcast://` | `.desktop` handler | ✗ (needs `.app`) | registry (HKCU) |
| Autostart | `~/.config/autostart` | ✗ (backlog: LaunchAgent) | Startup shortcut |
| Install | `install.sh` | `install.sh` | `install.ps1` |

## 10. Pointers

- Protocol and config types: `crates/meshcast-signal/src/lib.rs`
- File IPC: `crates/meshcast-signal/src/ipc.rs`
- Daemon state machine: `crates/meshcast-cli/src/main.rs` (`Session`)
- Bot interaction handlers: `crates/meshcast-bot/src/main.rs` (`on_*`)
- Discord facts: https://docs.discord.com/developers/components/reference, https://docs.discord.com/developers/activities/development-guides/networking, https://docs.discord.com/developers/resources/application
