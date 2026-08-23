# Remote control

Lets a viewer drive the streamer's mouse and keyboard, with the streamer's explicit, revocable consent. This document is the design; `docs/DESIGN.md` covers the rest of the system.

## Goals / non-goals

- **Goal:** "let me drive for a second" collaboration and helping a friend on their machine: pointer + buttons + scroll + keyboard, one controller at a time, low friction (Discord button → approve in the app), hard to misuse.
- **Non-goals (for now):** gaming-grade latency, gamepad, clipboard/file transfer, multi-controller, touch. Those are backlog items once this works.

## How it fits the architecture

Input is not media, so iroh-live is untouched. We add a small **control channel**: a second iroh protocol (`ALPN = meshcast/control/1`) served by the streamer's daemon on its existing signal endpoint. The viewer window dials it with a one-time token and streams input events. Everything is peer-to-peer QUIC, encrypted, NAT-traversed — no new infrastructure.

```
viewer window ──(control channel: QUIC stream, token)──► streamer daemon ──► OS input injection
      ▲                                                         ▲
      │ token via viewer daemon (file)             grant/revoke │ via app/tray (IPC)
      └──────────────── bot (Discord buttons + gossip signals) ─┘
```

### Consent & grant flow

1. Streamer opts in per stream: the app's consent dialog ("Share Screen") has **Allow remote-control requests** (remembered in config, default off). On Linux/Wayland this is what decides whether capture runs through a combined RemoteDesktop + ScreenCast portal session (one extra OS prompt that mentions remote control). Without it the card has no control button at all.
2. Viewer clicks **Request control** on the stream card. Bot → streamer daemon: `ControlRequest { request_id, viewer_name }`. The app shows "**X** wants to control your screen — Allow / Deny" (and a desktop notification). Requests expire after 90 s.
3. Streamer clicks Allow → daemon generates a 32-byte token, arms the control server for it, and sends `ControlGranted { request_id, token, addr }` (`addr` = the daemon's endpoint address, so the viewer can dial it) to the bot. Bot → viewer daemon: `ControlToken { ticket, token, addr }`. The bot edits the card: "🎮 X has control" and shows **Revoke control** (streamer only).
4. Viewer daemon writes the token to `<config>/control/<hash(ticket)>.json`. The viewer window for that ticket polls the file, connects, sends `Hello { token }`, gets `Welcome { width, height }` and shows a "You have control — F8 or Esc×2 to release" banner. Its egui pointer/key events are forwarded.
5. Revoke (streamer: app button, tray item, card button; or stream stops; or the viewer disconnects) → daemon closes the connection, **releases every pressed key and button**, sends `ControlRevoked` to the bot; the bot updates the card and tells the viewer window (its connection closes; banner goes away).
6. Deny / timeout → `ControlDenied { request_id, reason }` → bot tells the requester.

One controller at a time; a new grant replaces the previous one. The streamer keeps full local control throughout (their own input is untouched).

### Trust model

- Off by default, per stream; explicit approval of a named Discord user; OS-level portal consent on Wayland.
- Token (32 random bytes, base32) authorises exactly **one** connection: it is consumed by the first successful `Hello` (a second `Hello` with it is refused, and the live session is not disturbed); a new request is needed to reconnect. The control connection is QUIC/TLS; `Hello` must be the first message and arrive within 5 s. The streamer's log/notification names the connecting endpoint.
- Visible state everywhere: card badge, app banner, tray tooltip/menu, viewer banner. Revoke is one click in three places.
- Safety: all pressed keys/buttons released on any disconnect/revoke; event rate limited (~500 events/s, events beyond that dropped); a 10-minute idle auto-revoke; "mouse only for the first N seconds" is a backlog option.
- Trust boundary, honestly: the token transits the bot, so a **compromised bot host** that also forges a `ControlRequest` under a friendly name could use the token itself once the streamer clicks Allow (it would then hold control until revoked — visible in card/app/tray). The consent prompt therefore names the requester; binding the grant to the viewer's endpoint id (so the bot can't substitute itself) is the next step. Addresses in grants are relay-only, so a grant doesn't disclose the streamer's IPs.

### Wire protocol (control channel)

Length-prefixed (`u32` little-endian) `postcard` frames on one bidirectional QUIC stream. Append-only enum, like `Signal`.

```
viewer → streamer                         streamer → viewer
Hello { version, token }                  Welcome { width, height }
PointerMove { x, y }   // 0..1 of frame    Denied { reason }
PointerButton { button, pressed }         Revoked { reason }
Scroll { dx, dy }      // lines            Pong
Key { key, pressed }   // NamedKey enum
Text { text }          // typed characters
Release                // release everything we pressed (viewer focus lost)
Ping
```

`NamedKey` is a platform-neutral enum (letters/digits as `Text`; modifiers, arrows, Enter, Tab, Esc, Backspace, Delete, Home/End/PgUp/PgDn, F1–F12, Space). Streamer maps it to X11 keysyms (portal), enigo `Key` (macOS/X11/Windows).

### Injection backends

| Platform | Backend | Notes |
|---|---|---|
| Linux Wayland | xdg-desktop-portal **RemoteDesktop** (ashpd), session shared with ScreenCast | Absolute pointer in stream coordinates (`NotifyPointerMotionAbsolute`), `NotifyPointerButton` (evdev codes), `NotifyPointerAxisDiscrete`, `NotifyKeyboardKeysym`. GNOME & KDE. Requires the combined session → the iroh-live fork's `PipeWireScreenCapturer::from_portal_stream`. |
| Linux X11 | Same portal path where the desktop provides RemoteDesktop on X11 (GNOME does); otherwise control is unavailable and the stream runs without it (XTest backend is on the backlog). | |
| macOS | `enigo` (CGEvent) | Needs Accessibility permission (one-time OS prompt). Main display mapping. |
| Windows | `enigo` (SendInput) | Viewer-only platform today; injection compiles for when capture lands upstream. |

Coordinate mapping: viewer aspect-fits the frame itself and normalises pointer position within that exact rectangle (0..1); streamer multiplies by the captured surface size in *pixels* (the PipeWire-negotiated stream size for the portal — logical and pixel sizes differ on HiDPI — or the main display size for enigo).

### Components touched

- `meshcast-signal`: `control` module (messages, framing, `NamedKey`, token), new `Signal` variants (`ControlRequest`, `ControlGranted`, `ControlDenied`, `ControlToken`, `ControlRevoked`), `DaemonState.control_*`, IPC commands `grant`/`deny`/`revoke`, config `control.allow_requests`.
- `meshcast-cli` daemon: `control::server` (iroh `ProtocolHandler`), `control::inject` (portal / enigo backends), combined portal session for capture on Wayland when enabled, grant/revoke state in `Session`; viewer: token file watcher, `control::client`, input capture + banner.
- `meshcast-bot`: Request/Revoke buttons, signal handling, card badge.
- `meshcast-app`: allow-requests checkbox in consent dialog, control request prompt, "X has control — Revoke" banner.
- `meshcast-tray.py`: tooltip + **Revoke control** item.
- iroh-live fork (`mattcree/iroh-live`, branch `meshcast-0.5`): `from_portal_stream`.

### Phases

1. Protocol + daemon server + viewer client + pointer/scroll/keyboard on Linux (portal) and macOS/Windows (enigo), grant/revoke via app + bot, idle timeout, rate limits (releases exempt), hotkeys. ← shipped
3. Backlog: clipboard, multi-monitor, gamepad, "laser pointer only" mode, control for streams started from the CLI.
