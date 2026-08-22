# Contributing

Meshcast is glue code around iroh-live and Discord. Keep it small, obvious and boring. This file is the project's working agreement — for humans and for coding agents (see also `CLAUDE.md`).

## Dev setup

```bash
# Native deps (Fedora/Bluefin — use a toolbox on immutable distros; binaries run on the host)
toolbox create meshcast && toolbox enter meshcast
sudo dnf install -y pipewire-devel clang-devel libva-devel nasm pkg-config cmake \
  alsa-lib-devel mesa-libgbm-devel mesa-vulkan-drivers libxkbcommon-devel gtk3-devel atk-devel \
  python3-gobject libappindicator-gtk3 ShellCheck

# Debian/Ubuntu: see README "Building from source"

cargo build --workspace
```

Dev loop — all of these must pass before a commit:

```bash
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
shellcheck scripts/*.sh && python3 -m py_compile scripts/meshcast-tray.py
```

Optional but cheap cross-platform sanity check (needs `mingw64-gcc` + `rustup target add x86_64-pc-windows-gnu`):

```bash
cargo check -p meshcast-app -p meshcast-signal --target x86_64-pc-windows-gnu
```

CI (`.github/workflows/ci.yml`) runs exactly these on every push/PR, plus `cargo check --workspace` on macOS and Windows runners.

### Running things locally

```bash
# Bot (token from the Discord developer portal, see docs/DISCORD-SETUP.md)
DISCORD_TOKEN=... cargo run -p meshcast-bot

# Daemon + window, with logs in the terminal
cargo run -p meshcast-cli -- daemon
cargo run -p meshcast-app

# Use a throwaway config dir so you don't disturb your real links
MESHCAST_CONFIG_DIR=/tmp/mc-test cargo run -p meshcast-cli -- daemon
```

`RUST_LOG=debug` (or `meshcast=debug,iroh_gossip=debug`) for more detail.

## Code standards

- **Rust 2021, stable toolchain.** `rustfmt` defaults, `clippy -D warnings` clean, no `#[allow]` without a comment saying why.
- **Errors:** `anyhow` everywhere in binaries; add `.context("what we were doing")` at boundaries (I/O, network, parsing). No `unwrap()`/`expect()` outside tests unless the invariant is local and commented.
- **Logging:** `tracing`. `info` for state changes a user would care about, `warn` for recoverable problems, `error` for things that need attention, `debug` for chatter. Never log secrets (tokens, keys, topics, full tickets at `info`).
- **Async:** tokio multi-thread runtime. Don't hold a `std::sync::Mutex` guard across an `.await` (bind the lookup result to a local first — the compiler will tell you).
- **Wire format:** `Signal` and `PairSignal` are `postcard` enums — **append variants only, never reorder or remove**. There's a test pinning the indices; update it deliberately when you append.
- **Config/state files:** go through `meshcast_signal::write_private_file` (atomic, `0600`). Add new fields with `#[serde(default)]` so older files still load.
- **Process model:** networking only in the daemon. The window and tray stay thin clients of the state/command files. Don't put GTK in the Rust app on Linux (see `docs/DESIGN.md` §3.2).
- **Dependencies:** prefer what's already in `Cargo.lock`. iroh/iroh-gossip/iroh-live revisions are pinned together in `[workspace.dependencies]` — bump them together.
- **Scripts:** `bash` with `set -euo pipefail`, shellcheck-clean, no root required for desktop installs, never print secrets.
- **UX copy:** short, concrete, tells the user what to do next. Errors the bot shows are for the person in Discord, not for you.
- **Small commits.** One change per commit, build and test after each. UI/tray changes especially: one thing at a time — they're hard to bisect.

## Commits and PRs

- Imperative subject ≤ 72 chars, body explains *why* and anything non-obvious.
- Run the dev-loop commands above before pushing; CI must be green.
- Update `CHANGELOG.md` (Unreleased section) for anything user-visible.
- If you change the protocol, config format or install layout, say so in the PR and in `docs/DESIGN.md`.

## Releasing

1. Make sure `main` is green and `CHANGELOG.md` has a `## [X.Y.Z] - YYYY-MM-DD` section.
2. Bump `version` in the root `Cargo.toml` (`[workspace.package]`) and commit ("Release vX.Y.Z").
3. Tag and push: `git tag vX.Y.Z && git push origin main vX.Y.Z`.
4. The **Release** workflow builds Linux/macOS/Windows desktop archives, the Linux bot archive and the `ghcr.io/mattcree/meshcast-bot` image, writes `SHA256SUMS`, and publishes the GitHub Release with the changelog section as notes.
5. Update the homelab bot: `curl -fsSL …/deploy-bot.sh | bash -s -- --update` (or `docker compose pull && up -d`). Users' apps update with `install.sh` (re-run).

Compatibility rule: a new bot must keep working with the previous app release and vice versa for at least one version (append-only signals; tolerant config loading).

## Updating iroh / iroh-live

These are git dependencies pinned by revision because the iroh ecosystem moves fast and crates.io releases lag. To bump:

1. Pick a revision of iroh-live; read its `Cargo.toml` for the iroh / iroh-gossip revs it expects.
2. Set all three in `[workspace.dependencies]` **and** the matching entries in `[patch.crates-io]` (plus `iroh-base`, `iroh-relay`, `noq*`, `web-transport-*` as needed).
3. `cargo update -p iroh -p iroh-gossip -p iroh-live …`, build, fix API changes, run the smoke test (`meshcast daemon` against a running bot, `/stream`, Watch).
4. Mention the new revs in the changelog.

## Project docs map

| File | What |
|---|---|
| `README.md` | User-facing: install, use, troubleshoot |
| `docs/DESIGN.md` | Architecture, Discord constraints, protocol, trust model |
| `docs/DISCORD-SETUP.md` | Creating the Discord app and inviting the bot |
| `BACKLOG.md` | Prioritised work list — keep it current |
| `CHANGELOG.md` | Keep a Changelog format |
| `SECURITY.md` | Reporting and scope |
| `CLAUDE.md` | Orientation + rules for coding agents |
| `SPEC.md` | Historical original design; superseded by `docs/DESIGN.md` |
