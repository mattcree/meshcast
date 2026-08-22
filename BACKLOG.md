# Backlog

Single source of truth for "what's next". Keep it short and current: when you pick something up, say so in the PR; when it ships, move it to `CHANGELOG.md`. Issues on GitHub are welcome too — link them here.

Legend: **P1** needed for a smooth v1 experience · **P2** valuable · **P3** nice to have / research.

## Release readiness (v0.5.x)

- [ ] **P1 — Field-test v0.5.0 end to end** with the homelab bot updated (`deploy-bot.sh --update`): `/link` from a fresh config, `/stream` 720p30 and 1080p60, Watch from a second machine, Stop from Discord / app / tray, daemon SIGTERM mid-stream, bot restart mid-stream. Record results in the v0.5.1 changelog.
- [ ] **P1 — macOS smoke test** (capture via ScreenCaptureKit, tray, `install.sh`). We've never run it on a Mac; CI only proves it compiles.
- [ ] **P1 — Windows viewer smoke test** (`install.ps1`, `meshcast://` link, Watch).

## Install & distribution

- [ ] **P1 — macOS `.app` bundle** (Info.plist with `meshcast` URL scheme, LaunchAgent autostart, ad-hoc codesign; notarisation later). Unblocks `meshcast://` on macOS and removes the quarantine dance.
- [ ] **P2 — Linux packages**: `.deb`/`.rpm` via `cargo-deb`/`cargo-generate-rpm`, Flatpak (needs portal-only capture, verify PipeWire portal works sandboxed). AppImage as a stopgap.
- [ ] **P2 — Windows MSI** (WiX) with protocol registration; later: Windows capture once iroh-live supports it.
- [ ] **P2 — aarch64 Linux bot binary** (Raspberry Pi / ARM VPS) in the release matrix (cross or native runner) and multi-arch container image.
- [ ] **P2 — Self-update**: `meshcast update` (re-runs the installer logic) and a "new version available" hint in the app (GitHub releases API, opt-in).
- [ ] **P3 — Homebrew tap / AUR package.**

## Discord UX

- [ ] **P1 — https "Watch in Meshcast" link** (GitHub Pages page that redirects to `meshcast://watch/<ticket>` and shows install instructions). Gives unlinked viewers a one-click path where a URL handler is registered (Linux/Windows now, macOS after the `.app`). Design note: ticket goes in the URL fragment so it never reaches the server.
- [ ] **P2 — Persist active stream cards in the bot** (`state.json`) so a bot restart mid-stream still lets Stop / StreamStopped update the card.
- [ ] **P2 — Role/permission gating**: server setting for who may `/stream`; optional "viewers must have role X" (Watch checks member roles).
- [ ] **P2 — Stream card polish**: show viewer count live (edit on `ViewerUpdate`), elapsed time, "also streaming" when two people go live in one channel.
- [ ] **P3 — `/stream` in voice channels posts to the channel's text chat**; auto-stop when the streamer leaves the voice channel (needs `GUILD_VOICE_STATES`, non-privileged).
- [ ] **P3 — Localisation** of bot strings.

## Streaming quality & features

- [ ] **P1 — Desktop/system audio** (PipeWire monitor source on Linux, loopback on macOS) instead of/in addition to microphone; per-source toggles in the app and `/stream`.
- [ ] **P2 — Relay integration** for 5+ viewers: `publish via relay <addr>` in config and `/stream`, docs for running `irl relay` on a VPS, bandwidth guidance in the card.
- [ ] **P2 — Hardware encoding** (VA-API/VideoToolbox) when available; expose in settings. Check iroh-live feature flags.
- [ ] **P2 — Viewer window**: fullscreen (F/double-click), mute/volume, latency + bitrate overlay, "reconnecting…" instead of closing when the publisher blips.
- [ ] **P2 — Window/region capture choice remembered** (portal restore token on Wayland).
- [ ] **P3 — Multi-publisher rooms** ("a stream is a room"): let a second person `/stream join` into the same card; iroh-live rooms support it.
- [ ] **P3 — Camera overlay / picture-in-picture.**

## Robustness & engineering

- [ ] **P1 — Integration test at the gossip layer**: spin up a bot-side `SignalNode` and a daemon `Session` in-process (no Discord, no capture) and drive pairing + StartStream/StreamReady/Stop through real iroh-gossip. Makes protocol changes safe.
- [ ] **P2 — Older-glibc Linux builds**: Linux release binaries are built on Ubuntu 24.04 (glibc 2.39) because `cros-libva` needs libva ≥ 2.20. Options: build in a container with newer libva on an older base, or drop the `vaapi` default feature of iroh-live/moq-media (check whether the preset H.264 path actually uses VA-API hardware encode first — it may matter for 1080p60 CPU load).
- [ ] **P2 — Bump iroh / iroh-gossip / iroh-live** to current revisions (pinned April 2026); check upstream for Windows capture and relay API changes.
- [ ] **P2 — Replace file IPC with a local socket** only if the command set grows (see `docs/DESIGN.md` §3.3) — not before.
- [ ] **P2 — Log files** for the daemon/app (`~/.local/state/meshcast/*.log`, rotated) so users can attach them to bug reports without running from a terminal.
- [ ] **P2 — Crash-safe state**: if the daemon dies with a stream live, next start should notify the bot (`StreamStopped`) so the card isn't stale.
- [ ] **P3 — Rust tray on Linux** if/when a StatusNotifier crate works without GTK on the main thread (would remove the Python dependency).
- [ ] **P3 — Metrics**: viewer count / bitrate exposed via `meshcast status --json`.

## Done recently (see CHANGELOG)

- v0.5.0: stable pairing derivation, multi-link daemon, Stop from Discord, decline/failure feedback, viewer-window tracking, SIGTERM handling, CI, release automation (bot binary + container image), one-line installers for Linux/macOS/Windows, docs.
