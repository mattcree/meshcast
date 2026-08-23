# Production-hardening review & plan (2026-08-23)

Six independent reviews (security/threat model, reliability & failure modes, operability/deploy, code quality & maintainability, product/UX, performance/scalability) were run against `main` at v0.6.0. This document condenses the findings and lays out the execution plan. Status boxes are kept current as work lands; details of each finding live in the review transcripts summarised here.

Legend: **P0** wrong/unsafe now · **P1** before wider rollout · **P2** defence in depth / polish · effort S/M/L.

## 1. Headline findings

### Security (threat model: malicious server member, compromised bot host, malicious viewer, network)
| # | Sev | Finding | Fix |
|---|---|---|---|
| S1 | P1 | **Pairing PIN check is a global, un-rate-limited oracle**: the bot compares a received PIN against *every* pending PIN regardless of which pairing topic it arrived on, and answers every guess. A `/link` holder can brute-force other users' PINs. | Compare only against this topic's PIN; abort the pairing task after 3 wrong attempts. |
| S2 | P1 | **Control token is bot-visible, reusable while armed, and unbound to the viewer**: a second `Hello` with the same token hijacks the live session; a compromised bot can drive the streamer after one Allow. Docs overstate. | Consume the token on first successful `Hello` (single use); refuse further Hellos until a new grant; record the controller's endpoint id; fix docs. |
| S3 | P1 | **Stream tickets and `ControlGranted.addr` embed the streamer's direct IPs** and are posted to the whole channel / sent to viewers. | Build tickets and grant addresses from a relay-only `EndpointAddr`; document that connected peers still learn each other's IPs. |
| S4 | P1 | **Release supply chain**: actions pinned by mutable tag; `contents: write`/`packages: write` granted to every job; installers print "Checksum OK" even when no line matched; unsigned checksums. | Pin actions by SHA; scope permissions per job; installers fail hard when SUMS/line missing; (later) cosign-sign SUMS. |
| S5 | P2 | Gossip authorship under-verified (`delivered_from` is last-hop; bot never checks author; ticket from app unvalidated). | Daemon rejects swarm-scope; bot records app endpoint id at pairing and checks; bot `validate_ticket` on `StreamReady`. |
| S6 | P2 | File IPC: config/control dirs created 0755; tray writes cmd with umask; `notify-send` gets unescaped names (leading `--`). | 0700 dirs, `--` separator, strip leading dashes. |
| S7 | P2 | Viewer-window spawn and control-event floods have no debounce; bot broadcast channel shared/unbounded. | Debounce `WatchStream`; coalesce events; per-user limits. |

### Reliability
| # | Sev | Finding | Fix |
|---|---|---|---|
| R1 | **P0** | **Gossip link never re-joins** after bot restart, partition, or daemon-start-while-bot-down — "Waiting for bot…" forever (iroh-gossip does not retry bootstrap). | Daemon tick: `join_peers(bot)` for links not connected, with backoff; mark slot inactive when the receiver task ends. |
| R2 | P1 | Signals sent while disconnected are silently dropped (`broadcast_neighbors` with no neighbours is `Ok`); bot's "couldn't reach your app" is dead code. | Per-link outbox flushed on `NeighborUp`; bot tracks per-user connectivity and answers immediately. |
| R3 | P1 | Bot restart mid-stream → stale Live card, `/stream` dead-ends at "Already streaming" with no Stop. | Persist active cards in `state.json`; `on_stop` sends `StopStream` even when unknown; failure reply offers Stop. |
| R4 | P1 | Card post fails (missing permission) → desktop keeps capturing, nothing tells the user. | On post failure send `StopStream` and say so. |
| R5 | P1 | Timeout budgets misaligned (bot 120 s < consent 90 s + portal 120 s); late-`StreamReady` race on `pending_starts`. | `START_TIMEOUT` ≥ 240 s; unregister before sending `StopStream` on timeout. |
| R6 | P1 | Daemon event loop blocks for up to 2 min inside capture start / pairing; commands overwrite each other meanwhile. | Spawn capture start and pairing as tasks with a select arm; `Reject`/`Stop` abort. |
| R7 | P1 | Bot leaks a gossip receiver task on every re-`/link` and on `/unlink` (old topic stays live; cross-machine state corruption). | Store `AbortHandle` with the sender; abort on replace/remove. |
| R8 | P2 | `.tray-cmd` races (tray non-atomic write; read-then-delete TOCTOU). | Tray temp+rename; don't delete empty. |
| R9 | P2 | Held keys may stay pressed on daemon exit (portal release tail detached). | Keep the inject task handle; await it (timeout) in `ActiveStream::stop`. |
| R10 | P2 | Multi-link: any linked bot can stop/revoke another bot's stream. | Check the link index matches the active stream. |
| R11 | P2 | `DaemonState.error` sticky/cleared indiscriminately. | `error_at`, age out after 30 s, don't clear on NeighborUp. |
| R12 | P2 | `endpoint.online()` blocks forever offline. | 15 s timeout, proceed, surface "No network". |

### Operability
| # | Sev | Finding | Fix |
|---|---|---|---|
| O1 | P1 | **No log files**: daemon/app/tray stderr → /dev/null in every real launch path; crashes invisible. | `tracing-appender` daily logs in the state dir + panic hook; `meshcast status` shows path/tail; tray shows last line. |
| O2 | P1 | **No version visibility** (bot never logs it; app/status show none; protocol has none). | Log version first; `--version` everywhere; app footer; `Signal::Hello{version,features}` on join; `bot_version` in `DaemonState`. |
| O3 | P1 | Installer never runs the binary; glibc ≥ 2.39 / runtime libs undocumented; desktop entry points at the tray even when python-gi is missing. | Run `meshcast --version` post-install and fail loudly; document runtime deps; fall back `Exec` to `meshcast-app`. |
| O4 | P2 | Default `RUST_LOG` filters drop `iroh_gossip`/`iroh_relay`/`meshcast_signal` entirely. | `warn,meshcast=info,meshcast_signal=info,iroh_live=info`. |
| O5 | P2 | Corrupt `config.toml` → defaults → first save wipes links. | Back up bad file, surface error, refuse to overwrite. |
| O6 | P2 | CI doesn't exercise release artefacts/installers; no `--locked`; Dockerfile only on tags. | `--locked`; release dry-run job with installer round-trip; build image in CI. |
| O7 | P2 | `deploy-bot.sh`: token on the command line; user-scope sandboxing may fail (`226/NAMESPACE`); weak post-start check; partial hardening. | Prompt for token; wait for "Bot is ready"; conditional sandboxing; more directives. |
| O8 | P2 | Upgrade kills a live stream silently; no version print; `--no-launch` leaves no daemon. | Refuse mid-stream without `--force`; print from→to; always restart the daemon. |
| O9 | P2 | Full ticket logged at `info`. | Redact. |

### Performance
| # | Sev | Finding | Fix |
|---|---|---|---|
| F1 | P1 | **Encoder is always software openh264**; VAAPI/VideoToolbox compiled in but never selected. | Try hardware codec first (`VideoCodec::best_available()` / VAAPI factory), fall back; log/expose `is_hardware`; `video.codec = auto`. |
| F2 | P1 | **60 fps without remote control captures at 30 fps** (`ScreenCapturer::new()` uses `target_fps: 30`). | `ScreenCapturer::open(None, None, &ScreenConfig{target_fps})`. |
| F3 | P1 | Bandwidth numbers in DESIGN §6 don't match the bitrate formula (`px × 0.07 × (30+(fps−30)/2)`): 720p30≈1.9, 1080p60≈6.5 Mbit/s; no cap/warn on viewers. | Fix docs; warn on 1080p60 / many viewers; relay later. |
| F4 | P2 | Viewer forces software decode. | `DecoderBackend::Auto` with software fallback. |
| F5 | P2 | Control: one D-Bus round trip per pointer event, no coalescing; rate limit can drop presses. | Coalesce moves (viewer per frame, server collapse); never drop presses/text. |
| F6 | P2 | Viewer repaints at 60 Hz forever (incl. after "Stream ended"). | Repaint from stream fps; slow down when ended. |
| F7 | P2 | openh264 tuned for camera video; 1 s GOP. | Fork patch: `ScreenContentRealTime`, GOP 2–3 s. |
| F8 | P3 | Default iroh-live features pull camera stacks; `online()` untimed; fsync on every state write. | Explicit feature list; timeouts; no fsync for cache files. |

### Product / UX
| # | Sev | Finding | Fix |
|---|---|---|---|
| U1 | **P0** | Card shows five equal buttons incl. a Stop only the streamer may use; three "open" buttons. | Row 1 viewers: Watch · Request control · Open in app; Row 2: End stream; install link in text. |
| U2 | **P0** | Consent dialog doesn't say what is shared (mic on by default, hidden), who/why, or the 90 s deadline. | Show "you ran /stream in X", Screen + Microphone toggle inline, countdown; mic default off. |
| U3 | P1 | Vocabulary: "daemon"/"bot"/"app"/"window" mixed; "Start daemon" fails silently. | "Meshcast", "background service", "the server's bot"; surface launch errors. |
| U4 | P1 | Windows is viewer-only but nothing in-product says so; `/stream` dead-ends after four steps. | Daemon on Windows answers `StreamFailed` immediately; app + installer note. |
| U5 | P1 | Portal cancelled / capture failed → unactionable copy; pairing "timed out — is the bot running?" for expired codes. | Map causes to human sentences; "codes expire after 10 min — run /link again". |
| U6 | P1 | `/link` in a second server of the same bot orphans the first entry ("Linked servers" wording). | One link per bot; `/link` says so when already linked; dedupe by bot id. |
| U7 | P1 | Timeouts invisible (90 s consent, control request, 60 s grant, 10 min idle). | Say them; countdowns in prompts. |
| U8 | P1 | Control prompt shows spoofable display name only. | "Display Name (@username)"; explain what control means. |
| U9 | P1 | macOS: "Open in app" dead end; quarantine; no autostart. | Platform-aware watch page; docs; LaunchAgent later. |
| U10 | P2 | Notification urgency, viewer count semantics, ended-card loses author, first-run guidance, viewer hotkeys/fullscreen, watch-page copy, upgrade notes, copy consistency. | As listed. |

### Code quality
| # | Sev | Finding | Fix |
|---|---|---|---|
| C1 | P1 | `meshcast-cli/src/main.rs` (~2.1k lines) is six programs; `Session` untestable without network/screen. | Split into modules; outbox + capture seam; unit-test the state machine. |
| C2 | P1 | Bot receiver tasks un-cancellable (= R7). | AbortHandle. |
| C3 | P2 | `DaemonState` half mirrored by hand; control state across 4 fields; tuples for pending/active. | `snapshot()`; `ControlPhase` enum; typed structs; `Quality` enum. |
| C4 | P2 | No protocol version/capabilities; wire-format pins incomplete. | `Hello`; pin every variant + goldens. |
| C5 | P2 | Duplicated helpers (lock, shutdown signal, await-signal loops, release walks, LinkState/LinkConfig). | Consolidate. |
| C6 | P2 | No lint config; bot untested; cfg sprawl; dep hygiene (`dirs-next`, rand/toml dup). | `[workspace.lints]`; bot `Action` enum + tests; `inject::start` seam. |

## 2. Execution plan

Work is batched so each batch is a coherent, independently-shippable commit set with CI green. Order reflects value/risk: first the things that make it *wrong or unsafe today*, then what makes it *supportable*, then *fast*, then *nice*, then the refactor that keeps it maintainable.

- [x] **Batch A — correctness & security core (S each)** _(shipped 2026-08-23; R8 tray write done, R12 online timeout done)_: R1 rejoin, S1 PIN oracle, S2 single-use token + controller id, S3 relay-only addrs, S5 authorship checks, S6 dirs/notify, R2 outbox + bot connectivity, R4 card-post failure, R5 timeouts, R7/C2 bot task abort, R10 link check, R12 online timeout, O9 redact ticket, R8 tray atomic write.
- [x] **Batch B — operability (M)** _(shipped 2026-08-23)_: O1 log files + panic hook, O4 filters, O2 versions + `Signal::Hello` + bot `--version` + app footer + `status`, O3 installer smoke + runtime deps + desktop fallback, O5 config backup, R11 error_at, O8 upgrade guard.
- [x] **Batch C — performance (S/M)** _(F1/F2/F4/F5/F6/F3 shipped 2026-08-23; F7 GOP/screen-content and F8 features/no-fsync deferred)_: F2 capture fps, F1 hardware encoder with fallback + `video.codec`, F4 decoder Auto, F5 coalescing + never-drop presses, F6 repaint cadence, F3 docs + 1080p60 warning, F8 features/timeouts/no-fsync.
- [x] **Batch D — UX (partial, shipped 2026-08-23: U1 card, U2 consent, U4 Windows, U5 copy, U6 link, U8 identity, U9 watch page; U3 vocabulary, U7 live countdowns, U10 polish deferred)**: U1 card layout, U2 consent dialog (what/why/countdown, mic toggle, mic default off), U3 vocabulary, U4 Windows, U5 failure copy, U6 one link per bot, U7 timeouts, U8 control prompt identity, U9 watch page platform-aware, U10 selected polish.
- [ ] **Batch E — resilience (M)**: R3 persist cards + Stop-when-unknown, R6 non-blocking capture start/pairing, R9 await inject shutdown.
- [ ] **Batch F — maintainability & supply chain**: C1 module split + Session seam + state-machine tests, C3/C4/C5 typed state + full wire pins + dedupe, C6 lints/deps, S4 action SHA pins + permissions + strict checksum, O6 CI `--locked` + release dry-run + image build, O7 deploy-bot improvements, cosign signing.
- [ ] **Release v0.7.0** with changelog; field-test checklist in BACKLOG.

Each batch: implement → `fmt/clippy/test/e2e` → commit → push → CI green → tick here.
