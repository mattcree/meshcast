# CLAUDE.md — orientation for coding agents

Meshcast: a Discord bot + desktop app that lets friends screen-share peer-to-peer at high quality, built **on top of iroh-live** (which owns capture → encode → MoQ/QUIC transport → decode → render). Meshcast is glue: Discord UX, pairing/signalling, a small daemon, a thin window. Full design: `docs/DESIGN.md`. Working agreement: `CONTRIBUTING.md`. Work list: `BACKLOG.md`.

## The one-paragraph mental model

`meshcast-bot` (homelab) owns Discord. Each user's `meshcast daemon` (their PC) is paired to the bot over a private **iroh-gossip** topic. `/stream` → bot sends `StartStream` → daemon asks for consent in the window → starts iroh-live capture → sends back a ticket → bot posts a card with **Watch/Stop**. Watch → bot sends `WatchStream{ticket}` to the *viewer's* daemon → it spawns `meshcast watch <ticket>`. The window (`meshcast-app`) and the Linux tray (`scripts/meshcast-tray.py`) never touch the network; they read `.tray-state` and write `.tray-cmd` in the config dir.

## Layout

```
crates/meshcast-signal/   shared contract: Signal/PairSignal, PairCode, AppConfig, BotLinkStore,
                          ipc.rs (state/cmd files), process.rs (pids, detached spawn), SignalNode. Unit-tested.
crates/meshcast-bot/      poise/serenity bot: /link /unlink /stream, Watch/Stop handlers (on_*)
crates/meshcast-cli/      `meshcast`: daemon (Session state machine), watch (viewer), stream/link/unlink/status,
                          control.rs (control server/client), inject_portal.rs (Linux), inject_enigo.rs (mac/win)
crates/meshcast-app/      `meshcast-app`: egui window; tray module for macOS/Windows only
scripts/                  install.sh / install.ps1 / uninstall.sh (desktop), deploy-bot.sh (server), meshcast-tray.py
packaging/                linux/*.desktop templates, bot/{Dockerfile,docker-compose.yml}
.github/workflows/        ci.yml (fmt/clippy/test + mac/win check), release.yml (archives, bot, image, checksums)
docs/                     DESIGN.md, DISCORD-SETUP.md, REMOTE-CONTROL.md
```

## Commands

```bash
# Build env: Fedora/Bluefin → toolbox `meshcast` (has PipeWire/libva/etc. headers). Binaries run on the host.
toolbox run -c meshcast bash -lc 'export PATH=$HOME/.cargo/bin:$PATH; cargo build --workspace'
cargo fmt --all && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace
shellcheck scripts/*.sh && python3 -m py_compile scripts/meshcast-tray.py
cargo check -p meshcast-app -p meshcast-signal --target x86_64-pc-windows-gnu   # needs mingw64-gcc; catches cfg(not linux) breakage
MESHCAST_CONFIG_DIR=/tmp/mc cargo run -p meshcast-cli -- daemon               # throwaway config
```

CI runs the same. Releases: bump `[workspace.package].version`, changelog, `git tag vX.Y.Z`, push — see CONTRIBUTING "Releasing".

## Rules (learned the hard way)

- **Don't rebuild what iroh-live does**: no custom protocol, packetiser, encoder, capture layer. No URL buttons with custom schemes (Discord rejects them). No separate `/room` command — a stream is a room.
- **Wire format**: `Signal`/`PairSignal` are postcard enums — append variants only; a test pins the indices.
- **Networking only in the daemon.** Window/tray stay file-IPC clients. Don't put GTK into the Rust app on Linux (GTK + winit main-thread conflict; container D-Bus isolation) — the Python tray is deliberate.
- **Wayland**: no focus stealing / raise from another process; `Visible(false)` leaves a black window — use `Close`, re-launch to show.
- **Never hold a `std::sync::Mutex` guard across `.await`** (bind the lookup to a local first).
- **Secrets**: Discord token only via `DISCORD_TOKEN`; link topics/keys in `config.toml`/`state.json` (0600). Never commit, never log.
- **Small steps**: one change → build → test → commit. Especially UI/tray.
- **Docs are part of the change**: protocol/config/install changes → `docs/DESIGN.md` + `CHANGELOG.md`; new work → `BACKLOG.md`.
- Pinned git deps (iroh, iroh-gossip, iroh-live) live in `[workspace.dependencies]` + `[patch.crates-io]`; bump together. iroh-live comes from the `mattcree/iroh-live` fork (branch `meshcast-0.5`, one patch) — see CONTRIBUTING.
- Remote control: `ControlMsg`/`NamedKey` are append-only too; injection only through the `InjectCmd` channel; never arm the control server without a fresh token; every disconnect must `ReleaseAll`.

## Style

`anyhow` + `.context()`, `tracing`, no `unwrap` outside tests, rustfmt defaults, clippy clean. Keep it simple — this is glue code; don't over-abstract.
