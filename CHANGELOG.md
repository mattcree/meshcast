# Changelog

All notable changes to Meshcast. Format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/); versions follow [SemVer](https://semver.org/) (0.x: minor bumps may change protocol or config).

## [Unreleased]

### Added
- **Remote control** (`docs/REMOTE-CONTROL.md`): a viewer can request control of the streamer's mouse and keyboard from the stream card; the streamer approves per request in the app, can revoke from card/app/tray, and all held keys are released on revoke/disconnect. Off by default (checkbox in the consent dialog). Input travels over a dedicated iroh protocol (`meshcast/control/1`) with a one-time token. Linux uses the xdg-desktop-portal RemoteDesktop session (combined with screen-cast, so pointer mapping is exact — needs our iroh-live fork's `PipeWireScreenCapturer::from_portal_stream`); macOS/Windows use `enigo`. Viewer: F8 pause, Esc Esc release, on-screen banner.
- New signals (`ControlRequest/Granted/Denied/Token/Revoked`, `RevokeControl`, `ControlAvailable`), IPC commands `grant`/`deny`/`revoke`, `approve:control`, config `[control] allow_requests`.

### Changed
- `iroh-live`/`moq-media` now come from `github.com/mattcree/iroh-live` (branch `meshcast-0.5`: upstream `edd9bcc` + the one constructor above).

## [0.5.0] - 2026-08-22

Hardening and "ready to install" release. Bot and app from this version remain compatible with 0.4.x peers for steady-state signals; **pairing requires both sides ≥ 0.5.0** (new topic derivation — existing links keep working).

### Added
- **Stop from Discord**: the stream card has a **Stop** button (streamer only); `/stream` while live offers Stop. (`/stream stop` was documented in 0.4 but didn't exist.)
- `/unlink` command; `meshcast unlink --name` to remove one link; `meshcast status`.
- Immediate feedback in Discord when the streamer declines, the app times out, capture fails or a stream is already running (`StreamFailed` signal) — no more waiting 30 s for a generic timeout.
- Unlinked viewers clicking **Watch** get install instructions plus the ticket for `meshcast watch`.
- Daemon subscribes to **all** linked bots (previously only the first).
- `meshcast watch` shows an error window instead of silently exiting when it can't connect; window title includes the stream name; 30 s connect timeout.
- Desktop notifications on macOS (`osascript`); Linux unchanged.
- App: starts the daemon if it isn't running (all platforms), "Daemon not running" state, audio toggle in the window.
- Tray (Linux): works on Ubuntu (Ayatana AppIndicator), honours `XDG_CONFIG_HOME`/`MESHCAST_CONFIG_DIR`, `--show` flag, restarts the daemon if it dies, no longer assumes a toolbox.
- **CI** workflow: fmt, clippy (`-D warnings`), tests, shellcheck, and `cargo check` on macOS/Windows.
- **Release** workflow now also ships the bot (`meshcast-bot-linux-x86_64.tar.gz`), a container image (`ghcr.io/mattcree/meshcast-bot`), `SHA256SUMS`, and the desktop archives include the tray script, desktop files and installers.
- Installers: `scripts/install.sh` (Linux/macOS, no root, verifies checksums, registers launcher + `meshcast://` + autostart), `scripts/install.ps1` (Windows, registers `meshcast://`), `scripts/uninstall.sh`; `deploy-bot.sh` now uses the prebuilt binary (falls back to source), supports system or user systemd scope, `--update`, `--uninstall`.
- Docs: `docs/DESIGN.md`, `docs/DISCORD-SETUP.md`, `CONTRIBUTING.md`, `BACKLOG.md`, `SECURITY.md`, rewritten `README.md`, `LICENSE` file.
- 18 unit tests in `meshcast-signal` (wire format, pairing codes, config migration, IPC, process helpers).

### Changed
- Pairing topic is derived with BLAKE3 (`derive_key`) instead of `std::hash::DefaultHasher`, whose output is not guaranteed stable across Rust versions.
- Bot state stores a minimal `BotLink { topic }` per user (reads old entries fine).
- Config/state files are written atomically with `0600`; config dir honours `MESHCAST_CONFIG_DIR` and the platform config dir on macOS/Windows.
- Viewer windows are tracked as child processes and reaped; the limit (5) now applies to *open* windows, not lifetime clicks.
- Daemon handles SIGTERM (systemd stop, tray Quit, `timeout`) and tells the bot the stream stopped before exiting.
- Daemon no longer blocks at startup waiting for an offline bot.
- Workspace version is now shared (`0.5.0`), release profile strips + thin-LTO.
- `custom_id`s no longer carry the stream title (Discord's 100-char limit); setup state lives in the bot.

### Fixed
- `meshcast-app` failed to compile on macOS/Windows (`PathBuf::and_then`).
- Title longer than ~85 chars broke the Start button (custom_id limit).
- Watch silently stopped working after five clicks per daemon lifetime.
- A second `/stream` while live would start a second capture and orphan the first.
- Daemon ignored the audio toggle.
- Viewer set in the bot wasn't cleared when a stream ended.
- Bot **Stop** waits for the app to confirm and otherwise forces the card to "ended" (a crashed daemon no longer leaves a dead Live card); a late `StreamReady` with nobody waiting makes the bot tell the app to stop, so the two can't disagree.
- Linking/unlinking/reloading no longer restarts the daemon session — links are added/removed in place and a live stream survives unrelated link changes; settings changes in the window take effect immediately; `meshcast link` tells a running daemon to reconnect.
- Liveness checks (daemon/app/tray) verify the PID really is a Meshcast process, so a stale PID file after a crash or reboot can't block start-up.
- Linux tray: single instance (menu launch while the autostarted tray runs just opens the window), daemon respawn limited to once per minute.
- `install.sh` replaces binaries atomically (no more `Text file busy` when upgrading while a stream or viewer is open); `deploy-bot.sh` run as root migrates a pre-0.5 user-scope install (identity, links, token) instead of starting a second bot.
- macOS/Windows: keep using `~/.config/meshcast` if a pre-0.5 config exists there.

### Removed
- Legacy `meshcast1…` pairing tokens and the `link.json` file (0.1–0.2 era).
- `scripts/install-uri.sh` and the root `meshcast.desktop` (superseded by `install.sh` + `packaging/linux/`).

## [0.4.0] - 2026-04-07
- Daemon decoupled from the window; tray via Python AppIndicator; multi-server UI; consent dialog; desktop notifications.

## [0.3.x] - 2026-04-07
- Short pairing codes (PIN), unified link storage; CI fixes for Linux deps.

## [0.2.0] - 2026-04-07
- Two-step stream start (config card → consent), stream-ended updates, viewer "Stream ended", security review fixes, settings persisted, 60 fps.

## [0.1.0] - 2026-04-06
- First working end-to-end: bot `/link` + `/stream`, gossip signalling, egui viewer, GitHub Releases.

[Unreleased]: https://github.com/mattcree/meshcast/compare/v0.5.0...HEAD
[0.5.0]: https://github.com/mattcree/meshcast/compare/v0.4.0...v0.5.0
[0.4.0]: https://github.com/mattcree/meshcast/compare/v0.3.1...v0.4.0
[0.3.x]: https://github.com/mattcree/meshcast/compare/v0.2.0...v0.3.1
[0.2.0]: https://github.com/mattcree/meshcast/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/mattcree/meshcast/releases/tag/v0.1.0
