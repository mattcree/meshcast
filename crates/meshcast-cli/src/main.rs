//! `meshcast` — CLI and background daemon.
//!
//! * `meshcast daemon` is the long-running process that talks to the Discord
//!   bot(s) over gossip, starts/stops screen capture and launches viewer
//!   windows. The GUI (`meshcast-app`) and tray are thin clients of it.
//! * `meshcast watch <ticket>` is the viewer window.
//! * `meshcast stream` / `link` / `unlink` / `status` are manual equivalents.

mod control;
#[cfg(any(target_os = "macos", target_os = "windows"))]
mod inject_enigo;
#[cfg(target_os = "linux")]
mod inject_portal;

use std::path::PathBuf;
use std::process::Child;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use futures_lite::StreamExt;
use iroh::EndpointAddr;
use iroh_live::ticket::LiveTicket;
use iroh_live::Live;
use meshcast_signal::control::{self as ctrl, ControlGrant};
use meshcast_signal::ipc::{self, Command as IpcCommand};
use meshcast_signal::process;
use meshcast_signal::{
    derive_pairing_topic, normalize_fps, normalize_quality, relay_only, sanitize_title, AppConfig,
    DaemonState, DeliveryScope, EndpointId, Event, GossipSender, LinkConfig, LinkState, PairCode,
    PairSignal, ServerLink, Signal, SignalNode, StreamRequest, TopicId,
};
use moq_media::capture::{ScreenCapturer, ScreenConfig};
use moq_media::codec::h264::H264Encoder;
#[cfg(target_os = "linux")]
use moq_media::codec::VaapiEncoder;
#[cfg(target_os = "macos")]
use moq_media::codec::VtbEncoder;
use moq_media::codec::{AudioCodec, VideoCodec};
use moq_media::format::{
    AudioPreset, DecoderBackend, PlaybackConfig, VideoEncoderConfig, VideoPreset,
};
use moq_media::publish::{LocalBroadcast, VideoRenditions};
use moq_media::traits::{VideoEncoderFactory, VideoSource};
use moq_media::AudioBackend;
use tokio::sync::{mpsc, oneshot};

use crate::control::{ControlServer, InjectorHandle, ServerEvent};

/// How long the user has to approve a stream request in the app.
const CONSENT_TIMEOUT: Duration = Duration::from_secs(90);
/// How long pairing waits for the bot to answer.
const PAIR_TIMEOUT: Duration = Duration::from_secs(20);
/// Maximum simultaneously open viewer windows launched by the daemon.
const MAX_VIEWER_WINDOWS: usize = 5;
/// How often the daemon polls the command file and reaps viewer windows.
const TICK: Duration = Duration::from_millis(250);
/// Maximum time to wait for a stream to connect in `watch`.
const WATCH_CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
/// How long the streamer has to answer a remote-control request.
const CONTROL_REQUEST_TIMEOUT: Duration = Duration::from_secs(90);
/// How long a granted controller has to actually connect before the grant lapses.
const CONTROL_CONNECT_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Parser)]
#[command(
    name = "meshcast",
    version,
    about = "P2P screen streaming for Discord via iroh-live"
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Start streaming your screen (manual mode, without Discord)
    Stream {
        /// Broadcast name (used in the ticket)
        #[arg(long, default_value = "meshcast")]
        name: String,

        /// Disable audio capture
        #[arg(long)]
        no_audio: bool,

        /// Video quality preset: 360p, 720p, 1080p
        #[arg(long, default_value = meshcast_signal::DEFAULT_QUALITY)]
        quality: String,

        /// Frames per second: 30 or 60
        #[arg(long, default_value_t = meshcast_signal::DEFAULT_FPS)]
        fps: u32,
    },

    /// Watch a stream
    Watch {
        /// Ticket string or meshcast://watch/<ticket> URI
        ticket: String,
    },

    /// Link this machine to a Discord bot (paste the code from /link)
    Link {
        /// Pairing code from the `/link` command in Discord
        code: String,
    },

    /// Run the background daemon that responds to the Discord bot
    Daemon,

    /// Remove the link to a Discord bot (all links if no name is given)
    Unlink {
        /// Name of the server link to remove
        #[arg(long)]
        name: Option<String>,
    },

    /// Show daemon status and configured links
    Status,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    // `watch` is a separate short-lived window; log it apart from the daemon.
    let component = if matches!(cli.command, Commands::Watch { .. }) {
        "watch"
    } else if matches!(cli.command, Commands::Daemon) {
        "daemon"
    } else {
        "cli"
    };
    meshcast_signal::telemetry::init(component);

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("Failed to build tokio runtime")?;

    match cli.command {
        Commands::Stream {
            name,
            no_audio,
            quality,
            fps,
        } => rt.block_on(cmd_stream(name, no_audio, quality, fps)),
        Commands::Watch { ticket } => cmd_watch(ticket, &rt),
        Commands::Link { code } => rt.block_on(cmd_link(code)),
        Commands::Daemon => rt.block_on(cmd_daemon()),
        Commands::Unlink { name } => rt.block_on(cmd_unlink(name)),
        Commands::Status => cmd_status(),
    }
}

// ---------------------------------------------------------------------------
// Streaming (shared by `stream` and the daemon)
// ---------------------------------------------------------------------------

struct ActiveStream {
    live: Live,
    _broadcast: LocalBroadcast,
    ticket: String,
    /// Present when this stream accepts remote control.
    injector: Option<InjectorHandle>,
    /// Keeps the platform capture/injection session alive (portal session).
    _control_guard: Option<Box<dyn std::any::Any + Send + Sync>>,
}

impl ActiveStream {
    async fn stop(self) {
        self.live.shutdown().await;
    }
}

/// Pick the screen source, optionally through a session that also allows
/// input injection (remote control).
async fn open_screen(
    fps: u32,
    control: bool,
) -> Result<(
    Box<dyn moq_media::capture::VideoSource>,
    Option<InjectorHandle>,
    Option<Box<dyn std::any::Any + Send + Sync>>,
)> {
    if control {
        #[cfg(target_os = "linux")]
        {
            match inject_portal::start(fps).await {
                Ok(pc) => {
                    return Ok((
                        Box::new(pc.capturer),
                        Some(pc.injector),
                        Some(Box::new(pc.guard) as Box<dyn std::any::Any + Send + Sync>),
                    ));
                }
                Err(inject_portal::PortalError::Unavailable(e)) => {
                    // No portal at all → plain capture, no control.
                    tracing::warn!("Remote control unavailable, streaming without it: {e:#}");
                }
                // Dialog cancelled / session failed → that *is* the answer; don't
                // open a second screen-share dialog.
                Err(inject_portal::PortalError::Failed(e)) => {
                    return Err(e.context("Screen sharing (with remote control) failed"));
                }
            }
        }
        #[cfg(any(target_os = "macos", target_os = "windows"))]
        {
            let screen = open_capturer(fps).context("Failed to start screen capture")?;
            return match inject_enigo::start() {
                Ok(h) => Ok((Box::new(screen), Some(h), None)),
                Err(e) => {
                    tracing::warn!("Remote control unavailable, streaming without it: {e:#}");
                    Ok((Box::new(screen), None, None))
                }
            };
        }
    }
    let screen = open_capturer(fps).context("Failed to start screen capture")?;
    Ok((Box::new(screen), None, None))
}

/// Open the default screen at the requested frame rate. `ScreenCapturer::new()`
/// pins 30 fps, so a "60 fps" stream without remote control was actually
/// captured at 30 — pass the fps through explicitly.
fn open_capturer(fps: u32) -> Result<ScreenCapturer> {
    let config = ScreenConfig {
        target_fps: Some(fps as f32),
        ..Default::default()
    };
    ScreenCapturer::open(None, None, &config)
}

/// Which H.264 encoder to use. `auto` prefers hardware (VAAPI/VideoToolbox)
/// when a device is actually usable, else software openh264.
#[derive(Clone, Copy)]
enum Encoder {
    Openh264,
    #[cfg(target_os = "linux")]
    Vaapi,
    #[cfg(target_os = "macos")]
    Vtb,
}

impl Encoder {
    fn label(self) -> &'static str {
        match self {
            Encoder::Openh264 => "openh264 (software)",
            #[cfg(target_os = "linux")]
            Encoder::Vaapi => "VAAPI (hardware)",
            #[cfg(target_os = "macos")]
            Encoder::Vtb => "VideoToolbox (hardware)",
        }
    }
}

/// Resolve the config's `codec` setting to an encoder that can actually be
/// constructed on this machine (probes the hardware encoder and falls back).
fn resolve_encoder(setting: &str, preset: VideoPreset, fps: u32) -> Encoder {
    let _probe = VideoEncoderConfig::from_preset(preset).framerate(fps);
    let _setting = setting.trim().to_ascii_lowercase();
    let _want_hw = matches!(_setting.as_str(), "auto" | "" | "hardware");

    #[cfg(target_os = "linux")]
    if _want_hw || _setting == "h264-vaapi" || _setting == "vaapi" {
        match VaapiEncoder::with_config(_probe.clone()) {
            Ok(_) => return Encoder::Vaapi,
            Err(e) if !_want_hw => {
                tracing::warn!("VAAPI requested but unavailable ({e}); using software");
            }
            Err(e) => tracing::info!("No usable VAAPI encoder ({e}); using software"),
        }
    }
    #[cfg(target_os = "macos")]
    if _want_hw || _setting == "h264-vtb" || _setting == "videotoolbox" {
        match VtbEncoder::with_config(_probe.clone()) {
            Ok(_) => return Encoder::Vtb,
            Err(e) if !_want_hw => {
                tracing::warn!("VideoToolbox requested but unavailable ({e}); using software");
            }
            Err(e) => tracing::info!("No usable VideoToolbox encoder ({e}); using software"),
        }
    }
    Encoder::Openh264
}

/// Build a single rendition for the chosen encoder at the given quality/fps.
fn build_renditions(
    screen: Box<dyn VideoSource>,
    enc: Encoder,
    quality: &str,
    preset: VideoPreset,
    fps: u32,
) -> VideoRenditions {
    fn add<E: VideoEncoderFactory>(r: &mut VideoRenditions, name: String, cfg: VideoEncoderConfig) {
        let track = E::config_for(&cfg);
        r.add_with_callback(name, track.into(), move || E::with_config(cfg.clone()));
    }
    let cfg = VideoEncoderConfig::from_preset(preset).framerate(fps);
    let mut r = VideoRenditions::empty(screen);
    let name = format!("video/{quality}-{fps}fps");
    match enc {
        Encoder::Openh264 => add::<H264Encoder>(&mut r, name, cfg),
        #[cfg(target_os = "linux")]
        Encoder::Vaapi => add::<VaapiEncoder>(&mut r, name, cfg),
        #[cfg(target_os = "macos")]
        Encoder::Vtb => add::<VtbEncoder>(&mut r, name, cfg),
    }
    r
}

/// Start screen capture + publish. Returns the live handle and ticket.
async fn start_stream(
    name: &str,
    quality: &str,
    fps: u32,
    audio: bool,
    control: bool,
    codec: &str,
) -> Result<ActiveStream> {
    let quality = normalize_quality(quality);
    let fps = normalize_fps(fps);

    let live = Live::from_env()
        .await
        .context("Failed to initialise iroh-live")?
        .with_router()
        .spawn();

    let broadcast = LocalBroadcast::new();

    let (screen, injector, control_guard) = open_screen(fps, control).await?;
    let preset = match quality {
        "360p" => VideoPreset::P360,
        "1080p" => VideoPreset::P1080,
        _ => VideoPreset::P720,
    };

    let enc = resolve_encoder(codec, preset, fps);
    let renditions = build_renditions(screen, enc, quality, preset, fps);
    broadcast
        .video()
        .set(renditions)
        .context("Failed to set video source")?;
    tracing::info!("Streaming {quality} {fps}fps via {}", enc.label());
    // Keep VideoCodec imported (used by tests / manual paths).
    let _ = VideoCodec::H264;

    if audio {
        let audio_backend = AudioBackend::default();
        match audio_backend.default_input().await {
            Ok(mic) => match broadcast
                .audio()
                .set(mic, AudioCodec::Opus, [AudioPreset::Hq])
            {
                Ok(()) => tracing::info!("Audio capture started"),
                Err(e) => tracing::warn!("Audio disabled: {e}"),
            },
            Err(e) => tracing::warn!("No audio input available: {e}"),
        }
    }

    live.publish(name, &broadcast)
        .await
        .context("Failed to publish broadcast")?;

    // Relay-only address: the ticket is posted to a whole channel; don't hand out
    // the streamer's direct IPs with it (connected peers still hole-punch).
    let ticket = LiveTicket::new(relay_only(live.endpoint().addr()), name).to_string();
    Ok(ActiveStream {
        live,
        _broadcast: broadcast,
        ticket,
        injector,
        _control_guard: control_guard,
    })
}

async fn cmd_stream(name: String, no_audio: bool, quality: String, fps: u32) -> Result<()> {
    // The name ends up in the ticket; keep it to characters `validate_ticket` accepts.
    let name: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
                c
            } else {
                '-'
            }
        })
        .collect();
    let name = if name.is_empty() {
        "meshcast".to_string()
    } else {
        name
    };
    let stream = start_stream(&name, &quality, fps, !no_audio, false, "auto").await?;

    println!("\nStreaming! Share this ticket to let others watch:\n");
    println!("  {}\n", stream.ticket);
    println!("  {}\n", meshcast_signal::ticket_uri(&stream.ticket));
    println!("Press Ctrl+C to stop.\n");

    wait_for_shutdown_signal().await;
    tracing::info!("Shutting down...");
    stream.stop().await;
    Ok(())
}

/// Resolves on Ctrl+C, or SIGTERM on Unix.
async fn wait_for_shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let mut term = match signal(SignalKind::terminate()) {
            Ok(s) => s,
            Err(_) => {
                let _ = tokio::signal::ctrl_c().await;
                return;
            }
        };
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = term.recv() => {}
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

// ---------------------------------------------------------------------------
// Linking
// ---------------------------------------------------------------------------

async fn cmd_link(code: String) -> Result<()> {
    let mut config = AppConfig::load().await?;
    let node = SignalNode::new(None).await?;
    let name = do_link(&node, &code, &mut config).await?;
    node.shutdown().await;
    if process::pid_file_alive(&AppConfig::daemon_pid_path()) {
        let _ = ipc::send_command(&IpcCommand::Reload);
        println!("Linked to \"{name}\". The running daemon has been told to reconnect.");
    } else {
        println!(
            "Linked to \"{name}\". Run `meshcast daemon` (or open the Meshcast app) to start listening."
        );
    }
    Ok(())
}

/// Pair with a Discord bot using a pairing code. Saves the link into `config`
/// and returns the server name reported by the bot.
async fn do_link(node: &SignalNode, input: &str, config: &mut AppConfig) -> Result<String> {
    let (bot_id, pin) = PairCode::parse(input)?;
    tracing::info!("Pairing with bot {}", bot_id.fmt_short());

    let pairing_topic = derive_pairing_topic(&pin);
    let pairing_sub = node
        .gossip
        .subscribe(pairing_topic, vec![bot_id])
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    let (pair_sender, mut pair_receiver) = pairing_sub.split();

    let request = PairSignal::PairRequest { pin }.encode()?;

    let outcome = tokio::time::timeout(PAIR_TIMEOUT, async {
        let mut sent = false;
        while let Some(event) = pair_receiver.next().await {
            match event {
                Ok(Event::NeighborUp(_)) if !sent => {
                    // We have a neighbour (the bot) — now the request can be delivered.
                    pair_sender
                        .broadcast_neighbors(request.clone())
                        .await
                        .map_err(|e| anyhow::anyhow!("{e}"))?;
                    sent = true;
                }
                Ok(Event::Received(msg)) => match PairSignal::decode(&msg.content) {
                    Ok(PairSignal::PairAccepted { topic, server_name }) => {
                        return Ok((TopicId::from_bytes(topic), server_name));
                    }
                    Ok(PairSignal::PairRejected { reason }) => {
                        anyhow::bail!("Rejected: {reason}");
                    }
                    _ => {}
                },
                Ok(_) => {}
                Err(e) => anyhow::bail!("Connection lost: {e}"),
            }
        }
        anyhow::bail!("Connection closed")
    })
    .await
    .map_err(|_| anyhow::anyhow!("Timed out — is the bot running?"))??;

    let (real_topic, server_name) = outcome;
    let state = LinkState::new(real_topic, node.endpoint.secret_key(), bot_id);
    config.add_link(server_name.clone(), LinkConfig::from(state));
    config.save().await?;
    Ok(server_name)
}

async fn cmd_unlink(name: Option<String>) -> Result<()> {
    let mut config = AppConfig::load().await?;
    match name {
        Some(name) => {
            if config.remove_link(&name) {
                config.save().await?;
                println!("Unlinked from \"{name}\".");
            } else {
                println!("No link named \"{name}\".");
            }
        }
        None => {
            if config.is_linked() {
                config.links.clear();
                config.save().await?;
                println!("Unlinked from all servers.");
            } else {
                println!("Not linked.");
            }
        }
    }
    // Tell a running daemon to pick up the change.
    if process::pid_file_alive(&AppConfig::daemon_pid_path()) {
        let _ = ipc::send_command(&IpcCommand::Reload);
    }
    Ok(())
}

fn cmd_status() -> Result<()> {
    let config = AppConfig::load_sync()?;
    let daemon_running = process::pid_file_alive(&AppConfig::daemon_pid_path());
    let state = ipc::read_state();
    println!("Meshcast {}", meshcast_signal::VERSION);
    println!("Config:  {}", AppConfig::config_path().display());
    println!(
        "Logs:    {}",
        meshcast_signal::telemetry::log_path("daemon").display()
    );
    if let Some(v) = &state.bot_version {
        println!("Bot ver: {v}");
    }
    println!(
        "Daemon:  {}",
        if daemon_running {
            "running"
        } else {
            "not running"
        }
    );
    if daemon_running {
        println!(
            "Bot:     {}",
            if state.connected {
                "connected"
            } else {
                "not connected"
            }
        );
        println!(
            "Stream:  {}",
            if state.streaming {
                format!(
                    "LIVE {} {}fps, {} viewer(s)",
                    state.quality, state.fps, state.viewers
                )
            } else {
                "idle".into()
            }
        );
        if let Some(err) = state.error {
            println!("Error:   {err}");
        }
    }
    if config.links.is_empty() {
        println!("Links:   none (run /link in Discord, then `meshcast link <code>`)");
    } else {
        println!("Links:");
        for l in &config.links {
            println!("  - {}", l.name);
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Daemon
// ---------------------------------------------------------------------------

async fn cmd_daemon() -> Result<()> {
    let pid_path = AppConfig::daemon_pid_path();
    if process::pid_file_alive(&pid_path) {
        anyhow::bail!(
            "Another Meshcast daemon is already running (pid {}).",
            process::read_pid_file(&pid_path).unwrap_or_default()
        );
    }
    process::write_pid_file(&pid_path)?;

    let result = run_session(wait_for_shutdown_signal()).await;

    process::remove_own_pid_file(&pid_path);
    ipc::clear_state();
    result
}

/// One linked bot within a session. Index into `Session::links` is stable for
/// the life of the session; removed links keep their slot with `sender = None`.
struct LinkConn {
    name: String,
    topic: [u8; 32],
    peer_id: EndpointId,
    sender: Option<GossipSender>,
    /// Dropping this stops the receiver task (and thus the subscription).
    stop: Option<oneshot::Sender<()>>,
    connected: bool,
    /// Removed from config (never resubscribe) vs. merely disconnected.
    removed: bool,
    /// Last time we asked gossip to (re)join the bot, and the current backoff.
    last_join: Instant,
    join_backoff: Duration,
    /// Signals queued while the bot was unreachable; flushed on NeighborUp.
    outbox: Vec<Signal>,
}

impl LinkConn {
    fn is_active(&self) -> bool {
        self.sender.is_some()
    }

    async fn send(&mut self, signal: Signal) {
        if !self.is_active() {
            return;
        }
        if !self.connected {
            // `broadcast_neighbors` with no neighbours silently sends nothing, so
            // keep it for when the bot is back (bounded; newest wins).
            tracing::debug!(link = %self.name, "Bot not connected; queueing {}", signal.name());
            if self.outbox.len() >= 32 {
                self.outbox.remove(0);
            }
            self.outbox.push(signal);
            return;
        }
        self.send_now(signal).await;
    }

    async fn send_now(&self, signal: Signal) {
        let Some(sender) = &self.sender else {
            return;
        };
        match signal.encode() {
            Ok(bytes) => {
                if let Err(e) = sender.broadcast_neighbors(bytes).await {
                    tracing::warn!(link = %self.name, "Failed to send {}: {e}", signal.name());
                }
            }
            Err(e) => tracing::error!("Failed to encode {}: {e}", signal.name()),
        }
    }

    /// Deliver anything queued while disconnected.
    async fn flush_outbox(&mut self) {
        let queued = std::mem::take(&mut self.outbox);
        for s in queued {
            self.send_now(s).await;
        }
    }

    /// Ask gossip to dial the bot again (it does not retry on its own).
    async fn rejoin(&mut self) {
        if let Some(sender) = &self.sender {
            tracing::info!(link = %self.name, "Re-joining bot");
            if let Err(e) = sender.join_peers(vec![self.peer_id]).await {
                tracing::debug!(link = %self.name, "join_peers: {e}");
            }
        }
        self.last_join = Instant::now();
        self.join_backoff = (self.join_backoff * 2).min(Duration::from_secs(60));
    }

    fn deactivate(&mut self) {
        self.sender = None;
        self.stop = None; // drops the oneshot → receiver task exits
        self.connected = false;
        self.outbox.clear();
    }
}

type LinkEvent = (usize, Result<Event>);

/// Subscribe to one bot topic and forward its events (tagged with `idx`) into `tx`.
///
/// Uses `subscribe` (not `subscribe_and_join`) so an offline bot doesn't block;
/// connection state is tracked via NeighborUp/NeighborDown events.
async fn subscribe_link(
    node: &SignalNode,
    sl: &ServerLink,
    idx: usize,
    tx: &mpsc::Sender<LinkEvent>,
) -> Result<LinkConn> {
    let ls = LinkState::from(sl.config.clone());
    let peer_id = ls.peer_endpoint_id()?;
    let topic = node
        .gossip
        .subscribe(ls.topic_id(), vec![peer_id])
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    let (sender, mut receiver) = topic.split();
    let (stop_tx, mut stop_rx) = oneshot::channel::<()>();
    let tx = tx.clone();
    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = &mut stop_rx => break,
                                event = receiver.next() => {
                    let Some(event) = event else {
                        // Subscription ended (gossip shut the topic) — tell the loop so
                        // the link can be re-subscribed rather than silently dead.
                        let _ = tx.send((idx, Err(anyhow::anyhow!("subscription ended")))).await;
                        break;
                    };
                    let item = event.map_err(|e| anyhow::anyhow!("{e}"));
                    if tx.send((idx, item)).await.is_err() {
                        break;
                    }
                }
            }
        }
    });
    tracing::info!(link = %sl.name, "Subscribed to bot topic");
    Ok(LinkConn {
        name: sl.name.clone(),
        topic: sl.config.topic,
        peer_id,
        sender: Some(sender),
        stop: Some(stop_tx),
        connected: false,
        removed: false,
        last_join: Instant::now(),
        join_backoff: Duration::from_secs(5),
        outbox: Vec::new(),
    })
}

struct Session {
    config: AppConfig,
    links: Vec<LinkConn>,
    state: DaemonState,
    active: Option<(usize, ActiveStream)>,
    pending: Option<(usize, StreamRequest, Instant)>,
    viewers: Vec<Child>,
    /// Remote-control server (armed per grant).
    control: ControlServer,
    /// Our address, sent to the bot with a grant so the viewer can dial us.
    addr: EndpointAddr,
    /// A viewer's control request awaiting Allow/Deny: (link, request_id, viewer, since).
    pending_control: Option<(usize, u64, String, Instant)>,
    /// Link through which the current grant was made (to report revocation).
    control_link: Option<usize>,
    /// When the current grant was armed and no controller has connected yet.
    armed_at: Option<Instant>,
}

impl Session {
    fn publish_state(&mut self) {
        self.state.version = meshcast_signal::VERSION.to_string();
        self.state.control_allowed = self
            .active
            .as_ref()
            .is_some_and(|(_, s)| s.injector.is_some());
        self.state.controller = self.control.controller();
        self.state.connected = self.links.iter().any(|l| l.is_active() && l.connected);
        self.state.linked_servers = self
            .links
            .iter()
            .filter(|l| l.is_active())
            .map(|l| l.name.clone())
            .collect();
        if let Err(e) = ipc::write_state(&self.state) {
            tracing::warn!("Failed to write state file: {e}");
        }
    }

    /// Bring the set of subscribed bot topics in line with `new_config`
    /// without disturbing an active stream (unless its own link was removed).
    async fn apply_config(
        &mut self,
        node: &SignalNode,
        tx: &mpsc::Sender<LinkEvent>,
        new_config: AppConfig,
    ) {
        // Remove links that are gone from the config.
        let mut removed = Vec::new();
        for (idx, conn) in self.links.iter_mut().enumerate() {
            if !conn.removed
                && !new_config
                    .links
                    .iter()
                    .any(|l| l.config.topic == conn.topic)
            {
                tracing::info!(link = %conn.name, "Link removed");
                conn.deactivate();
                conn.removed = true;
                removed.push(idx);
            }
        }
        for idx in removed {
            if self.active.as_ref().is_some_and(|(i, _)| *i == idx) {
                self.stop_stream("link removed").await;
            }
            if self.pending.as_ref().is_some_and(|(i, _, _)| *i == idx) {
                self.pending = None;
                self.state.pending_request = None;
            }
        }
        // Add links that are new.
        for sl in &new_config.links {
            let already = self
                .links
                .iter()
                .any(|c| !c.removed && c.topic == sl.config.topic);
            if already {
                continue;
            }
            let idx = self.links.len();
            match subscribe_link(node, sl, idx, tx).await {
                Ok(conn) => self.links.push(conn),
                Err(e) => tracing::warn!(link = %sl.name, "Failed to subscribe: {e}"),
            }
        }
        self.state.quality = new_config.video.quality.clone();
        self.state.fps = new_config.video.fps;
        self.config = new_config;
        self.publish_state();
    }

    async fn send_to(&mut self, link_idx: usize, signal: Signal) {
        if let Some(link) = self.links.get_mut(link_idx) {
            link.send(signal).await;
        }
    }

    /// Is this link the one the active stream belongs to?
    fn owns_stream(&self, idx: usize) -> bool {
        self.active.as_ref().is_some_and(|(i, _)| *i == idx)
    }

    /// Periodic link maintenance: re-join bots we've lost, resubscribe ended
    /// subscriptions.
    async fn maintain_links(&mut self, node: &SignalNode, tx: &mpsc::Sender<LinkEvent>) {
        for idx in 0..self.links.len() {
            let (due, active, removed, name, topic) = {
                let l = &self.links[idx];
                (
                    !l.connected && l.last_join.elapsed() >= l.join_backoff,
                    l.is_active(),
                    l.removed,
                    l.name.clone(),
                    l.topic,
                )
            };
            if removed || !due {
                continue;
            }
            if active {
                self.links[idx].rejoin().await;
            } else if let Some(sl) = self.config.links.iter().find(|l| l.config.topic == topic) {
                match subscribe_link(node, sl, idx, tx).await {
                    Ok(conn) => {
                        tracing::info!(link = %name, "Re-subscribed");
                        self.links[idx] = conn;
                    }
                    Err(e) => {
                        tracing::warn!(link = %name, "Re-subscribe failed: {e}");
                        let l = &mut self.links[idx];
                        l.last_join = Instant::now();
                        l.join_backoff = (l.join_backoff * 2).min(Duration::from_secs(60));
                    }
                }
            }
        }
    }

    async fn stop_stream(&mut self, reason: &str) {
        self.revoke_control("stream stopped").await;
        if self
            .state
            .error
            .as_deref()
            .is_some_and(|e| e.starts_with("Remote control unavailable"))
        {
            self.state.error = None;
        }
        if let Some((idx, _, _, _)) = self.pending_control.take() {
            self.send_to(
                idx,
                Signal::ControlDenied {
                    request_id: 0,
                    reason: "stream stopped".into(),
                },
            )
            .await;
            self.state.pending_control = None;
        }
        if let Some((idx, stream)) = self.active.take() {
            stream.stop().await;
            self.send_to(idx, Signal::StreamStopped).await;
            self.state.streaming = false;
            self.state.stream_ticket = None;
            self.state.viewers = 0;
            self.publish_state();
            tracing::info!("Stream stopped ({reason})");
        }
    }

    async fn approve_pending(&mut self, control: bool) {
        let Some((idx, req, _)) = self.pending.take() else {
            return;
        };
        self.state.pending_request = None;
        self.state.error = None;
        if self.active.is_some() {
            // Shouldn't happen (requests are rejected while streaming), but be safe.
            self.stop_stream("replaced").await;
        }
        let audio = self.config.audio.enabled;
        let codec = self.config.video.codec.clone();
        match start_stream("meshcast", &req.quality, req.fps, audio, control, &codec).await {
            Ok(stream) => {
                tracing::info!("Streaming ({} chars ticket)", stream.ticket.len());
                self.send_to(
                    idx,
                    Signal::ControlAvailable {
                        available: stream.injector.is_some(),
                    },
                )
                .await;
                self.send_to(
                    idx,
                    Signal::StreamReady {
                        ticket: stream.ticket.clone(),
                    },
                )
                .await;
                if control && stream.injector.is_none() {
                    self.state.error =
                        Some("Remote control unavailable on this desktop (see logs)".into());
                }
                self.state.streaming = true;
                self.state.stream_ticket = Some(stream.ticket.clone());
                self.state.quality = normalize_quality(&req.quality).to_string();
                self.state.fps = normalize_fps(req.fps);
                self.active = Some((idx, stream));
            }
            Err(e) => {
                tracing::error!("Failed to start stream: {e:#}");
                self.state.error = Some(format!("Stream failed: {e}"));
                self.send_to(
                    idx,
                    Signal::StreamFailed {
                        reason: format!("{e}"),
                    },
                )
                .await;
            }
        }
        self.publish_state();
    }

    async fn reject_pending(&mut self, reason: &str) {
        if let Some((idx, _, _)) = self.pending.take() {
            self.state.pending_request = None;
            self.publish_state();
            self.send_to(
                idx,
                Signal::StreamFailed {
                    reason: reason.to_string(),
                },
            )
            .await;
            tracing::info!("Stream request dismissed ({reason})");
        }
    }

    fn reap_viewers(&mut self) {
        self.viewers
            .retain_mut(|child| matches!(child.try_wait(), Ok(None)));
    }

    fn launch_viewer(&mut self, ticket: &str) {
        self.reap_viewers();
        if self.viewers.len() >= MAX_VIEWER_WINDOWS {
            tracing::warn!("Too many viewer windows open ({MAX_VIEWER_WINDOWS}); ignoring Watch");
            self.state.error = Some(format!(
                "Close some viewer windows first (limit {MAX_VIEWER_WINDOWS})"
            ));
            self.publish_state();
            return;
        }
        let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("meshcast"));
        match process::launch_viewer(&exe, ticket) {
            Ok(child) => {
                self.viewers.push(child);
                tracing::info!("Viewer launched ({} open)", self.viewers.len());
            }
            Err(e) => {
                tracing::error!("Failed to launch viewer: {e:#}");
                self.state.error = Some(format!("Couldn't open viewer: {e}"));
                self.publish_state();
            }
        }
    }

    async fn handle_signal(&mut self, idx: usize, signal: Signal) {
        let link_name = self
            .links
            .get(idx)
            .map(|l| l.name.clone())
            .unwrap_or_default();
        match signal {
            Signal::StartStream {
                title,
                quality,
                fps,
                server,
            } => {
                if self.active.is_some() {
                    tracing::warn!("StartStream while already streaming — rejecting");
                    self.send_to(
                        idx,
                        Signal::StreamFailed {
                            reason: "Already streaming. Stop the current stream first.".into(),
                        },
                    )
                    .await;
                    return;
                }
                let title = {
                    let t = sanitize_title(&title);
                    if t.is_empty() {
                        "Stream".to_string()
                    } else {
                        t
                    }
                };
                let server = if server.trim().is_empty() {
                    link_name
                } else {
                    server
                };
                tracing::info!("Stream requested: \"{title}\" ({quality} {fps}fps) from {server}");
                let request = StreamRequest {
                    title: title.clone(),
                    server: server.clone(),
                    quality: normalize_quality(&quality).to_string(),
                    fps: normalize_fps(fps),
                };
                // A newer request supersedes an older unanswered one.
                if let Some((old_idx, _, _)) = self.pending.take() {
                    self.send_to(
                        old_idx,
                        Signal::StreamFailed {
                            reason: "Superseded by a newer request".into(),
                        },
                    )
                    .await;
                }
                self.pending = Some((idx, request.clone(), Instant::now()));
                self.state.pending_request = Some(request);
                self.state.error = None;
                self.publish_state();
                notify(
                    &format!("Stream request from {server}"),
                    &format!("\"{title}\" — open Meshcast to approve"),
                );
            }
            Signal::StopStream => {
                if self.pending.as_ref().is_some_and(|(i, _, _)| *i == idx) {
                    self.reject_pending("cancelled by the bot").await;
                }
                if self.owns_stream(idx) {
                    self.stop_stream("requested by bot").await;
                } else if self.active.is_some() {
                    tracing::debug!(link = %link_name, "StopStream from a link that doesn't own the stream; ignored");
                } else {
                    // Nothing running: answer so the bot's card converges anyway.
                    self.send_to(idx, Signal::StreamStopped).await;
                }
            }
            Signal::WatchStream { ticket } => {
                tracing::info!("Watch requested");
                self.launch_viewer(&ticket);
            }
            Signal::ViewerUpdate { count } => {
                if self.owns_stream(idx) {
                    self.state.viewers = count;
                    self.publish_state();
                }
            }
            Signal::Ping => self.send_to(idx, Signal::Pong).await,
            Signal::Hello { version, .. } => {
                if idx == 0 {
                    self.state.bot_version = Some(version);
                    self.publish_state();
                }
            }
            Signal::ControlRequest { request_id, viewer } => {
                self.on_control_request(idx, request_id, viewer).await;
            }
            Signal::RevokeControl => {
                if self.owns_stream(idx) {
                    self.revoke_control("revoked from Discord").await;
                }
            }
            Signal::ControlToken {
                ticket,
                token,
                addr,
                streamer,
            } => {
                // We are the viewer: hand the token to the viewer window for this stream.
                let grant = ControlGrant {
                    ticket,
                    token,
                    addr,
                    streamer: streamer.clone(),
                };
                match ctrl::write_grant(&grant) {
                    Ok(()) => {
                        tracing::info!("Control granted by {streamer}; handed to viewer window")
                    }
                    Err(e) => tracing::error!("Failed to store control grant: {e:#}"),
                }
            }
            Signal::Pong
            | Signal::StreamReady { .. }
            | Signal::StreamStopped
            | Signal::StreamFailed { .. }
            | Signal::ControlGranted { .. }
            | Signal::ControlDenied { .. }
            | Signal::ControlRevoked
            | Signal::ControlAvailable { .. } => {
                // App-originated signals echoed back over gossip; ignore.
            }
        }
    }

    async fn on_control_request(&mut self, idx: usize, request_id: u64, viewer: String) {
        let viewer = sanitize_title(&viewer);
        let viewer = if viewer.is_empty() {
            "A viewer".to_string()
        } else {
            viewer
        };
        let available = self
            .active
            .as_ref()
            .is_some_and(|(_, s)| s.injector.is_some());
        if !available {
            self.send_to(
                idx,
                Signal::ControlDenied {
                    request_id,
                    reason: "remote control isn't enabled for this stream".into(),
                },
            )
            .await;
            return;
        }
        if let Some(current) = self.control.controller() {
            self.send_to(
                idx,
                Signal::ControlDenied {
                    request_id,
                    reason: format!("{current} already has control"),
                },
            )
            .await;
            return;
        }
        if let Some((old_idx, old_id, _, _)) = self.pending_control.take() {
            self.send_to(
                old_idx,
                Signal::ControlDenied {
                    request_id: old_id,
                    reason: "superseded by a newer request".into(),
                },
            )
            .await;
        }
        tracing::info!("Control requested by {viewer}");
        self.pending_control = Some((idx, request_id, viewer.clone(), Instant::now()));
        self.state.pending_control = Some(meshcast_signal::ControlRequestState {
            request_id,
            viewer: viewer.clone(),
        });
        self.publish_state();
        notify(
            &format!("{viewer} wants to control your screen"),
            "Open Meshcast to allow or deny",
        );
    }

    async fn grant_control(&mut self) {
        let Some((idx, request_id, viewer, _)) = self.pending_control.take() else {
            return;
        };
        self.state.pending_control = None;
        let injector = self.active.as_ref().and_then(|(_, s)| s.injector.clone());
        let Some(injector) = injector else {
            self.send_to(
                idx,
                Signal::ControlDenied {
                    request_id,
                    reason: "remote control is no longer available".into(),
                },
            )
            .await;
            self.publish_state();
            return;
        };
        let token = ctrl::generate_token();
        self.control.arm(token.clone(), viewer.clone(), injector);
        self.control_link = Some(idx);
        self.armed_at = Some(Instant::now());
        self.send_to(
            idx,
            Signal::ControlGranted {
                request_id,
                token,
                addr: relay_only(self.addr.clone()),
            },
        )
        .await;
        tracing::info!("Control granted to {viewer}");
        self.publish_state();
    }

    async fn deny_control(&mut self, reason: &str) {
        if let Some((idx, request_id, viewer, _)) = self.pending_control.take() {
            self.state.pending_control = None;
            self.send_to(
                idx,
                Signal::ControlDenied {
                    request_id,
                    reason: reason.to_string(),
                },
            )
            .await;
            tracing::info!("Control request from {viewer} denied ({reason})");
            self.publish_state();
        }
    }

    async fn revoke_control(&mut self, reason: &str) {
        self.armed_at = None;
        if let Some(who) = self.control.disarm(reason) {
            if let Some(idx) = self.control_link.take() {
                self.send_to(idx, Signal::ControlRevoked).await;
            }
            tracing::info!("Control of {who} ended ({reason})");
            self.publish_state();
        }
    }

    async fn on_server_event(&mut self, ev: ServerEvent) {
        match ev {
            ServerEvent::ControllerConnected { controller, peer } => {
                self.armed_at = None;
                tracing::info!("{controller} ({peer}) is now controlling the screen");
                notify(
                    "Meshcast",
                    &format!("{controller} is now controlling your screen"),
                );
            }
            ServerEvent::ControllerDisconnected { controller, reason } => {
                tracing::info!("{controller} disconnected: {reason}");
                self.revoke_control(&format!("viewer disconnected: {reason}"))
                    .await;
            }
        }
    }

    /// Handle a local command from the window/tray.
    async fn handle_command(
        &mut self,
        node: &SignalNode,
        tx: &mpsc::Sender<LinkEvent>,
        cmd: IpcCommand,
    ) {
        match cmd {
            IpcCommand::Stop => self.stop_stream("requested locally").await,
            IpcCommand::Approve { control } => self.approve_pending(control).await,
            IpcCommand::Grant => self.grant_control().await,
            IpcCommand::Deny => self.deny_control("declined by the streamer").await,
            IpcCommand::Revoke => self.revoke_control("revoked in the app").await,
            IpcCommand::Reject => self.reject_pending("Declined in the Meshcast app").await,
            IpcCommand::Reload => match AppConfig::load().await {
                Ok(cfg) => {
                    tracing::info!("Reloading config");
                    self.apply_config(node, tx, cfg).await;
                }
                Err(e) => {
                    tracing::error!("Config reload failed: {e:#}");
                    self.state.error = Some(format!("Config error: {e}"));
                    self.publish_state();
                }
            },
            IpcCommand::Link(code) => {
                self.state.error = None;
                self.publish_state();
                // Work on a fresh copy of the config so we never clobber edits the
                // window made since this session started.
                let mut cfg = match AppConfig::load().await {
                    Ok(cfg) => cfg,
                    Err(e) => {
                        tracing::error!("Config unreadable: {e:#}");
                        self.state.error = Some(format!("Config error: {e}"));
                        self.publish_state();
                        return;
                    }
                };
                match do_link(node, &code, &mut cfg).await {
                    Ok(name) => {
                        tracing::info!("Linked to \"{name}\"");
                        notify("Meshcast", &format!("Linked to {name}"));
                        self.apply_config(node, tx, cfg).await;
                    }
                    Err(e) => {
                        tracing::error!("Link failed: {e:#}");
                        self.state.error = Some(format!("Link failed: {e}"));
                        self.publish_state();
                    }
                }
            }
            IpcCommand::Unknown(other) => tracing::debug!("Unknown command: {other}"),
        }
    }

    async fn shutdown(mut self) {
        self.stop_stream("daemon shutting down").await;
    }
}

/// Notify the desktop user (best effort, platform dependent).
fn notify(summary: &str, body: &str) {
    let summary = summary.to_string();
    let body = body.to_string();
    std::thread::spawn(move || {
        #[cfg(target_os = "linux")]
        {
            let _ = std::process::Command::new("notify-send")
                .args([
                    "--app-name=Meshcast",
                    "--urgency=critical",
                    "--",
                    &summary,
                    &body,
                ])
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status();
        }
        #[cfg(target_os = "macos")]
        {
            let esc = |s: &str| s.replace('\\', "\\\\").replace('"', "\\\"");
            let script = format!(
                "display notification \"{}\" with title \"{}\"",
                esc(&body),
                esc(&summary)
            );
            let _ = std::process::Command::new("osascript")
                .args(["-e", &script])
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status();
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        {
            let _ = (&summary, &body);
        }
    });
}

/// Name of a command for logs — never includes payloads (pairing codes are secrets).
fn command_label(cmd: &IpcCommand) -> &'static str {
    match cmd {
        IpcCommand::Stop => "stop",
        IpcCommand::Approve { .. } => "approve",
        IpcCommand::Grant => "grant",
        IpcCommand::Deny => "deny",
        IpcCommand::Revoke => "revoke",
        IpcCommand::Reject => "reject",
        IpcCommand::Reload => "reload",
        IpcCommand::Link(_) => "link",
        IpcCommand::Unknown(_) => "unknown",
    }
}

/// Run the daemon until `shutdown` resolves (Ctrl+C/SIGTERM in production;
/// a oneshot in tests).
async fn run_session(shutdown: impl std::future::Future<Output = ()>) -> Result<()> {
    let config = AppConfig::load().await.unwrap_or_else(|e| {
        tracing::error!("Config unreadable, using defaults: {e:#}");
        AppConfig::default()
    });

    // The first link's key gives the daemon a stable identity; the bot does not
    // verify app identity, so one key works for every link.
    let secret_key = config.link_state().map(|l| l.secret_key());
    let (control, mut control_rx) = ControlServer::new();
    let control_for_router = control.clone();
    let node = SignalNode::with_protocols(secret_key, move |r| {
        r.accept(ctrl::CONTROL_ALPN, control_for_router)
    })
    .await?;
    ctrl::clear_all_grants();

    let (tx, mut rx) = mpsc::channel::<LinkEvent>(256);

    let mut session = Session {
        state: DaemonState::default(),
        config: AppConfig::default(),
        links: Vec::new(),
        active: None,
        pending: None,
        viewers: Vec::new(),
        addr: node.addr(),
        control,
        pending_control: None,
        control_link: None,
        armed_at: None,
    };
    session.apply_config(&node, &tx, config).await;
    if session.links.is_empty() {
        tracing::info!("Not linked — waiting for a pairing code (paste it into the Meshcast app)");
    }
    println!("Daemon running. Press Ctrl+C to stop.");

    let mut tick = tokio::time::interval(TICK);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    tokio::pin!(shutdown);

    loop {
        tokio::select! {
            biased;

            _ = &mut shutdown => {
                tracing::info!("Shutting down daemon...");
                break;
            }

            Some(ev) = control_rx.recv() => {
                session.on_server_event(ev).await;
            }

            Some((idx, event)) = rx.recv() => {
                let Some(link) = session.links.get(idx) else { continue };
                if !link.is_active() {
                    continue; // stale event from a removed link
                }
                match event {
                                        Ok(Event::Received(msg)) => {
                        // Only direct, neighbours-scoped messages from the bot itself:
                        // `delivered_from` is the last hop, so swarm-relayed messages
                        // could originate from anyone on the topic.
                        if msg.delivered_from != link.peer_id
                            || !matches!(msg.scope, DeliveryScope::Neighbors)
                        {
                            tracing::warn!(
                                "Rejected message from unexpected peer {} ({:?})",
                                msg.delivered_from.fmt_short(),
                                msg.scope
                            );
                            continue;
                        }
                        match Signal::decode(&msg.content) {
                            Ok(signal) => session.handle_signal(idx, signal).await,
                            Err(e) => tracing::warn!("Undecodable signal: {e}"),
                        }
                    }
                                        Ok(Event::NeighborUp(id)) => {
                        if let Some(link) = session.links.get_mut(idx) {
                            tracing::info!(link = %link.name, peer = %id.fmt_short(), "Bot connected");
                                                        link.connected = true;
                            link.join_backoff = Duration::from_secs(5);
                            link.flush_outbox().await;
                        }
                        // Announce ourselves so the bot can adapt to our version.
                        session
                            .send_to(
                                idx,
                                Signal::Hello {
                                    version: meshcast_signal::VERSION.to_string(),
                                    features: meshcast_signal::CAPABILITIES
                                        .iter()
                                        .map(|s| s.to_string())
                                        .collect(),
                                },
                            )
                            .await;
                        session.publish_state();
                    }
                    Ok(Event::NeighborDown(id)) => {
                        if let Some(link) = session.links.get_mut(idx) {
                            tracing::warn!(link = %link.name, peer = %id.fmt_short(), "Bot disconnected");
                            link.connected = false;
                        }
                        session.publish_state();
                    }
                    Ok(_) => {}
                                        Err(e) => {
                        tracing::warn!("Gossip error on link {idx}: {e}; will re-subscribe");
                        if let Some(link) = session.links.get_mut(idx) {
                            // Keep the slot (indices are stable) but drop the dead
                            // subscription; maintain_links re-subscribes with backoff.
                            link.deactivate();
                            link.last_join = Instant::now();
                        }
                        session.publish_state();
                    }
                }
            }

            _ = tick.tick() => {
                // Expire unanswered consent prompts.
                if let Some((_, _, since)) = &session.pending {
                    if since.elapsed() > CONSENT_TIMEOUT {
                        session.reject_pending("No response in the Meshcast app").await;
                    }
                }
                if let Some((_, _, _, since)) = &session.pending_control {
                    if since.elapsed() > CONTROL_REQUEST_TIMEOUT {
                        session.deny_control("no response from the streamer").await;
                    }
                }
                // A grant nobody connects to must not linger (card/app/tray would
                // disagree and further requests would be refused).
                if let Some(armed) = session.armed_at {
                    if !session.control.is_connected() && armed.elapsed() > CONTROL_CONNECT_TIMEOUT {
                        session.revoke_control("viewer never connected").await;
                    }
                }
                                session.reap_viewers();
                session.maintain_links(&node, &tx).await;

                if let Some(cmd) = ipc::take_command() {
                    tracing::debug!("Command: {}", command_label(&cmd));
                    session.handle_command(&node, &tx, cmd).await;
                }
            }
        }
    }

    session.shutdown().await;
    node.shutdown().await;
    ctrl::clear_all_grants();
    Ok(())
}

// ---------------------------------------------------------------------------
// Viewer
// ---------------------------------------------------------------------------

/// Watch command — connects, then runs eframe on the main thread.
fn cmd_watch(raw: String, rt: &tokio::runtime::Runtime) -> Result<()> {
    use moq_media_egui::{create_egui_wgpu_config, VideoTrackView};

    let ticket_str = meshcast_signal::parse_ticket_uri(&raw).to_string();
    let ticket: LiveTicket = match meshcast_signal::validate_ticket(&ticket_str)
        .and_then(|t| t.parse().context("Invalid ticket string"))
    {
        Ok(t) => t,
        Err(e) => return show_error_window(&format!("Invalid stream ticket.\n\n{e}")),
    };

    // Async setup: connect and subscribe
    let connected = rt.block_on(async {
        tracing::info!("Connecting to stream '{}'...", ticket.broadcast_name);

        let live = Live::from_env()
            .await
            .context("Failed to initialise iroh-live")?
            .spawn();

        let sub = tokio::time::timeout(
            WATCH_CONNECT_TIMEOUT,
            live.subscribe(ticket.endpoint.clone(), &ticket.broadcast_name),
        )
        .await
        .map_err(|_| anyhow::anyhow!("Timed out connecting to the streamer"))?
        .context("Failed to subscribe to stream")?;

        let audio_backend = AudioBackend::default();
        // Prefer hardware decode; iroh-live falls back to software internally.
        // Force software with MESHCAST_SW_DECODE=1 if a GPU misbehaves.
        let backend = if std::env::var_os("MESHCAST_SW_DECODE").is_some() {
            DecoderBackend::Software
        } else {
            DecoderBackend::Auto
        };
        let playback_config = PlaybackConfig {
            backend,
            ..Default::default()
        };
        let tracks = sub
            .broadcast()
            .media(&audio_backend, playback_config)
            .await
            .context("Failed to initialise media tracks")?;

        tracing::info!("Connected.");
        anyhow::Ok((live, sub, tracks))
    });

    let (live, sub, tracks) = match connected {
        Ok(c) => c,
        Err(e) => {
            tracing::error!("{e:#}");
            return show_error_window(&format!(
                "Couldn't connect to the stream.\n\n{e:#}\n\nThe streamer may have stopped, or the network is blocking the connection."
            ));
        }
    };

    // eframe must run on the main thread
    let _guard = rt.enter();
    let native_options = eframe::NativeOptions {
        renderer: eframe::Renderer::Wgpu,
        wgpu_options: create_egui_wgpu_config(),
        viewport: eframe::egui::ViewportBuilder::default()
            .with_title(format!("Meshcast — {}", ticket.broadcast_name))
            .with_inner_size([1280.0, 720.0])
            .with_min_inner_size([320.0, 180.0]),
        ..Default::default()
    };

    let endpoint = live.endpoint().clone();
    let rt_handle = rt.handle().clone();
    eframe::run_native(
        "Meshcast",
        native_options,
        Box::new(move |cc| {
            let video_view = tracks
                .video
                .map(|track| VideoTrackView::new(&cc.egui_ctx, "video", track));

            Ok(Box::new(WatchApp {
                video: video_view,
                _audio: tracks.audio,
                broadcast: tracks.broadcast,
                sub,
                _live: live,
                stream_ended: false,
                control: viewer::ControlUi::new(endpoint, ticket_str, rt_handle),
            }))
        }),
    )
    .map_err(|e| anyhow::anyhow!("eframe error: {e}"))?;

    // Give a control connection a moment to flush its Release/close frames
    // before the runtime (and the QUIC endpoint) go away.
    rt.block_on(tokio::time::sleep(Duration::from_millis(300)));
    Ok(())
}

/// Show a small window with an error message (the viewer is usually launched
/// detached, so stderr is invisible to the user).
fn show_error_window(message: &str) -> Result<()> {
    let message = message.to_string();
    let native_options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_title("Meshcast")
            .with_inner_size([440.0, 200.0]),
        ..Default::default()
    };
    eframe::run_native(
        "Meshcast",
        native_options,
        Box::new(move |_cc| Ok(Box::new(ErrorApp { message }))),
    )
    .map_err(|e| anyhow::anyhow!("eframe error: {e}"))?;
    Ok(())
}

struct ErrorApp {
    message: String,
}

impl eframe::App for ErrorApp {
    fn update(&mut self, ctx: &eframe::egui::Context, _frame: &mut eframe::Frame) {
        use eframe::egui;
        egui::CentralPanel::default().show(ctx, |ui| {
            ui.heading("Meshcast");
            ui.add_space(8.0);
            ui.label(&self.message);
            ui.add_space(12.0);
            if ui.button("Close").clicked() {
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
        });
    }
}

struct WatchApp {
    video: Option<moq_media_egui::VideoTrackView>,
    _audio: Option<moq_media::subscribe::AudioTrack>,
    broadcast: moq_media::subscribe::RemoteBroadcast,
    sub: iroh_live::Subscription,
    _live: Live,
    stream_ended: bool,
    control: viewer::ControlUi,
}

impl eframe::App for WatchApp {
    fn update(&mut self, ctx: &eframe::egui::Context, _frame: &mut eframe::Frame) {
        use eframe::egui;

        // While live, repaint fast enough to display the stream; once it's ended
        // there's nothing to draw, so stop spinning the CPU/GPU at 60 Hz.
        ctx.request_repaint_after(Duration::from_millis(if self.stream_ended {
            500
        } else {
            16
        }));

        // Detect stream end
        if !self.stream_ended && self.broadcast.shutdown_token().is_cancelled() {
            self.stream_ended = true;
            self.control.stream_ended();
        }

        self.control.poll_grant();
        self.control.banner(ctx);

        let mut video_rect: Option<egui::Rect> = None;
        egui::CentralPanel::default()
            .frame(
                egui::Frame::new()
                    .inner_margin(0.0)
                    .fill(egui::Color32::BLACK),
            )
            .show(ctx, |ui| {
                let avail = ui.available_size();
                if self.stream_ended {
                    ui.centered_and_justified(|ui| {
                        ui.label(
                            egui::RichText::new("Stream ended")
                                .color(egui::Color32::WHITE)
                                .heading(),
                        );
                    });
                } else if let Some(video) = self.video.as_mut() {
                    let (img, _) = video.render(ctx, avail);
                    // Aspect-fit the frame ourselves so we know the exact rect the
                    // video occupies (pointer mapping depends on it).
                    let full = ui.available_rect_before_wrap();
                    let rect = match img.size() {
                        Some(sz) if sz.x > 0.0 && sz.y > 0.0 => {
                            let scale = (avail.x / sz.x).min(avail.y / sz.y);
                            egui::Rect::from_center_size(full.center(), sz * scale)
                        }
                        _ => full,
                    };
                    ui.put(rect, img);
                    video_rect = Some(rect);
                } else {
                    ui.centered_and_justified(|ui| {
                        ui.label(
                            egui::RichText::new("Audio only — no video track")
                                .color(egui::Color32::WHITE),
                        );
                    });
                }
            });

        if let Some(rect) = video_rect {
            self.control.forward_input(ctx, rect);
        }
    }

    fn on_exit(&mut self) {
        tracing::info!("Exiting viewer");
        self.control.release();
        self.sub.session().close(0, b"bye");
    }
}

/// Remote-control UI glue for the viewer window: grant polling, the control
/// connection, input forwarding and the banner.
mod viewer {
    use std::time::{Duration, Instant};

    use eframe::egui;
    use iroh::Endpoint;
    use meshcast_signal::control::{self as proto, ControlMsg, NamedKey, PointerButton};

    use crate::control::{ClientStatus, ControlClient};

    /// Grants older than this are ignored (stale file from an earlier stream).
    const GRANT_MAX_AGE: Duration = Duration::from_secs(15 * 60);
    const GRANT_POLL: Duration = Duration::from_secs(1);
    const DOUBLE_ESC: Duration = Duration::from_millis(700);

    pub struct ControlUi {
        endpoint: Endpoint,
        ticket: String,
        rt: tokio::runtime::Handle,
        client: Option<ControlClient>,
        /// Token we already acted on, so a Denied/Ended grant isn't retried forever.
        seen_token: Option<String>,
        streamer: String,
        last_poll: Instant,
        paused: bool,
        last_esc: Option<Instant>,
        modifiers: egui::Modifiers,
        /// Printable keys we sent as *pressed* (chords), so their release is
        /// always forwarded even if the modifier went up first.
        pressed_chars: std::collections::HashSet<char>,
        notice: Option<(String, Instant)>,
        ended: bool,
    }

    impl ControlUi {
        pub fn new(endpoint: Endpoint, ticket: String, rt: tokio::runtime::Handle) -> Self {
            Self {
                endpoint,
                ticket,
                rt,
                client: None,
                seen_token: None,
                streamer: String::new(),
                last_poll: Instant::now() - GRANT_POLL,
                paused: false,
                last_esc: None,
                modifiers: egui::Modifiers::NONE,
                pressed_chars: std::collections::HashSet::new(),
                notice: None,
                ended: false,
            }
        }

        pub fn stream_ended(&mut self) {
            self.ended = true;
            self.release();
            proto::clear_grant(&self.ticket);
        }

        /// Stop controlling (viewer-initiated).
        pub fn release(&mut self) {
            if let Some(c) = self.client.take() {
                c.disconnect();
                self.set_notice("Control released");
            }
            self.pressed_chars.clear();
            self.paused = false;
        }

        fn set_notice(&mut self, text: impl Into<String>) {
            self.notice = Some((text.into(), Instant::now()));
        }

        fn active(&self) -> bool {
            self.client.as_ref().is_some_and(|c| c.is_active())
        }

        /// Look for a grant file written by our daemon and connect.
        pub fn poll_grant(&mut self) {
            if self.ended || self.last_poll.elapsed() < GRANT_POLL {
                return;
            }
            self.last_poll = Instant::now();

            // Report status changes of an existing client.
            if let Some(c) = &self.client {
                match c.status() {
                    ClientStatus::Denied(r) => {
                        self.set_notice(format!("Control denied: {r}"));
                        self.client = None;
                    }
                    ClientStatus::Ended(r) => {
                        self.set_notice(format!("Control ended: {r}"));
                        self.client = None;
                        self.paused = false;
                    }
                    _ => {}
                }
            }
            if self.client.is_some() {
                return;
            }

            let Some(grant) = proto::read_grant(&self.ticket) else {
                return;
            };
            if self.seen_token.as_deref() == Some(grant.token.as_str()) {
                return;
            }
            // Ignore stale grant files.
            if let Ok(meta) = std::fs::metadata(proto::grant_path(&self.ticket)) {
                if let Ok(modified) = meta.modified() {
                    if modified.elapsed().unwrap_or_default() > GRANT_MAX_AGE {
                        return;
                    }
                }
            }
            self.seen_token = Some(grant.token.clone());
            self.streamer = grant.streamer.clone();
            let _enter = self.rt.enter();
            self.client = Some(ControlClient::connect(self.endpoint.clone(), grant));
            self.set_notice("Connecting control…");
        }

        /// Top banner while control is pending/active, or a transient notice.
        pub fn banner(&mut self, ctx: &egui::Context) {
            let active = self.active();
            let connecting = self
                .client
                .as_ref()
                .is_some_and(|c| c.status() == ClientStatus::Connecting);
            let notice = self
                .notice
                .clone()
                .filter(|(_, at)| at.elapsed() < Duration::from_secs(6))
                .map(|(t, _)| t);
            if !active && !connecting && notice.is_none() {
                return;
            }
            let fill = if active && !self.paused {
                egui::Color32::from_rgb(46, 125, 50)
            } else if active {
                egui::Color32::from_rgb(120, 100, 20)
            } else {
                egui::Color32::from_rgb(50, 52, 58)
            };
            let mut release_clicked = false;
            let mut pause_clicked = false;
            egui::TopBottomPanel::top("control-banner")
                .frame(
                    egui::Frame::new()
                        .fill(fill)
                        .inner_margin(egui::Margin::symmetric(10, 6)),
                )
                .show(ctx, |ui| {
                    ui.horizontal(|ui| {
                        let who = if self.streamer.is_empty() {
                            "the streamer".to_string()
                        } else {
                            self.streamer.clone()
                        };
                        let text = if active && !self.paused {
                            format!("🎮 You control {who}'s screen — F8 pause · Esc Esc release")
                        } else if active {
                            format!("⏸ Control paused ({who}) — F8 to resume")
                        } else if connecting {
                            "Connecting remote control…".to_string()
                        } else {
                            notice.clone().unwrap_or_default()
                        };
                        ui.label(
                            egui::RichText::new(text)
                                .color(egui::Color32::WHITE)
                                .strong(),
                        );
                        if active {
                            ui.with_layout(
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| {
                                    if ui.button("Release").clicked() {
                                        release_clicked = true;
                                    }
                                    if ui
                                        .button(if self.paused { "Resume" } else { "Pause" })
                                        .clicked()
                                    {
                                        pause_clicked = true;
                                    }
                                },
                            );
                        }
                    });
                });
            if release_clicked {
                self.release();
            }
            if pause_clicked {
                self.toggle_pause();
            }
        }

        fn toggle_pause(&mut self) {
            self.paused = !self.paused;
            if self.paused {
                if let Some(c) = &self.client {
                    c.send(ControlMsg::Release);
                }
            }
        }

        /// Translate this frame's egui input into control messages.
        pub fn forward_input(&mut self, ctx: &egui::Context, video_rect: egui::Rect) {
            if !self.active() {
                return;
            }
            let (events, mods) = ctx.input(|i| (i.events.clone(), i.modifiers));
            let Some(client) = self.client.clone() else {
                return;
            };
            // winit can deliver many PointerMoved per frame (1000 Hz mice); only
            // the final position matters, so forward just the last one.
            let last_move = events
                .iter()
                .rposition(|e| matches!(e, egui::Event::PointerMoved(_)));

            // Hotkeys are handled even when paused.
            for ev in &events {
                if let egui::Event::Key {
                    key,
                    pressed: true,
                    repeat: false,
                    ..
                } = ev
                {
                    match key {
                        egui::Key::F8 => {
                            self.toggle_pause();
                            return;
                        }
                        egui::Key::Escape => {
                            if self.last_esc.is_some_and(|t| t.elapsed() < DOUBLE_ESC) {
                                self.release();
                                return;
                            }
                            self.last_esc = Some(Instant::now());
                        }
                        _ => {}
                    }
                }
            }
            if self.paused {
                return;
            }

            // Modifier transitions → key press/release.
            let pairs = [
                (self.modifiers.shift, mods.shift, NamedKey::Shift),
                (self.modifiers.ctrl, mods.ctrl, NamedKey::Control),
                (self.modifiers.alt, mods.alt, NamedKey::Alt),
                (self.modifiers.mac_cmd, mods.mac_cmd, NamedKey::Super),
            ];
            for (was, now, key) in pairs {
                if was != now {
                    client.send(ControlMsg::Key { key, pressed: now });
                }
            }
            self.modifiers = mods;
            let chord = mods.ctrl || mods.alt || mods.mac_cmd;

            let norm = |pos: egui::Pos2| -> Option<(f32, f32)> {
                if video_rect.width() <= 0.0 || video_rect.height() <= 0.0 {
                    return None;
                }
                let x = (pos.x - video_rect.left()) / video_rect.width();
                let y = (pos.y - video_rect.top()) / video_rect.height();
                Some((x.clamp(0.0, 1.0), y.clamp(0.0, 1.0)))
            };

            for (i, ev) in events.iter().cloned().enumerate() {
                match ev {
                    egui::Event::PointerMoved(pos) => {
                        if Some(i) == last_move && video_rect.contains(pos) {
                            if let Some((x, y)) = norm(pos) {
                                client.send(ControlMsg::PointerMove { x, y });
                            }
                        }
                    }
                    egui::Event::PointerButton {
                        pos,
                        button,
                        pressed,
                        ..
                    } => {
                        // Presses only inside the video; releases always (so nothing
                        // sticks) — but don't drag the pointer to the edge first.
                        let inside = video_rect.contains(pos);
                        if pressed && !inside {
                            continue;
                        }
                        if let Some(b) = map_button(button) {
                            if inside {
                                if let Some((x, y)) = norm(pos) {
                                    client.send(ControlMsg::PointerMove { x, y });
                                }
                            }
                            client.send(ControlMsg::PointerButton { button: b, pressed });
                        }
                    }
                    egui::Event::MouseWheel { unit, delta, .. } => {
                        // egui: positive y = content moves down (wheel rolled up).
                        let lines = match unit {
                            egui::MouseWheelUnit::Line => delta,
                            egui::MouseWheelUnit::Point => delta / 40.0,
                            egui::MouseWheelUnit::Page => delta * 10.0,
                        };
                        client.send(ControlMsg::Scroll {
                            dx: lines.x,
                            dy: -lines.y,
                        });
                    }
                    egui::Event::Key {
                        key,
                        pressed,
                        repeat,
                        ..
                    } => {
                        if repeat && !pressed {
                            continue;
                        }
                        if matches!(key, egui::Key::F8) {
                            continue;
                        }
                        if let Some(k) = map_key(key) {
                            client.send(ControlMsg::Key { key: k, pressed });
                        } else if let Some(c) = key_char(key) {
                            // Printable keys: as a *key* only inside chords (Ctrl+C…);
                            // plain typing arrives as Text. A release is always sent
                            // for a char we pressed, even if the modifier went up first.
                            if pressed && chord {
                                self.pressed_chars.insert(c);
                                client.send(ControlMsg::Key {
                                    key: NamedKey::Char(c),
                                    pressed: true,
                                });
                            } else if !pressed && self.pressed_chars.remove(&c) {
                                client.send(ControlMsg::Key {
                                    key: NamedKey::Char(c),
                                    pressed: false,
                                });
                            }
                        }
                    }
                    // egui turns Ctrl/Cmd+C/X/V into these instead of Key events.
                    egui::Event::Copy => send_chord_char(&client, 'c'),
                    egui::Event::Cut => send_chord_char(&client, 'x'),
                    egui::Event::Paste(_) => send_chord_char(&client, 'v'),
                    egui::Event::Text(text) => {
                        if !chord && !text.is_empty() {
                            client.send(ControlMsg::Text { text });
                        }
                    }
                    egui::Event::WindowFocused(false) => {
                        client.send(ControlMsg::Release);
                        self.modifiers = egui::Modifiers::NONE;
                        self.pressed_chars.clear();
                    }
                    _ => {}
                }
            }
        }
    }

    fn map_button(b: egui::PointerButton) -> Option<PointerButton> {
        Some(match b {
            egui::PointerButton::Primary => PointerButton::Left,
            egui::PointerButton::Secondary => PointerButton::Right,
            egui::PointerButton::Middle => PointerButton::Middle,
            egui::PointerButton::Extra1 => PointerButton::Back,
            egui::PointerButton::Extra2 => PointerButton::Forward,
        })
    }

    fn map_key(k: egui::Key) -> Option<NamedKey> {
        use egui::Key as K;
        Some(match k {
            K::Escape => NamedKey::Escape,
            K::Enter => NamedKey::Enter,
            K::Tab => NamedKey::Tab,
            K::Backspace => NamedKey::Backspace,
            K::Delete => NamedKey::Delete,
            K::Insert => NamedKey::Insert,
            K::Space => NamedKey::Space,
            K::ArrowLeft => NamedKey::ArrowLeft,
            K::ArrowRight => NamedKey::ArrowRight,
            K::ArrowUp => NamedKey::ArrowUp,
            K::ArrowDown => NamedKey::ArrowDown,
            K::Home => NamedKey::Home,
            K::End => NamedKey::End,
            K::PageUp => NamedKey::PageUp,
            K::PageDown => NamedKey::PageDown,
            K::F1 => NamedKey::F1,
            K::F2 => NamedKey::F2,
            K::F3 => NamedKey::F3,
            K::F4 => NamedKey::F4,
            K::F5 => NamedKey::F5,
            K::F6 => NamedKey::F6,
            K::F7 => NamedKey::F7,
            K::F9 => NamedKey::F9,
            K::F10 => NamedKey::F10,
            K::F11 => NamedKey::F11,
            K::F12 => NamedKey::F12,
            _ => return None,
        })
    }

    /// Press+release a printable key while the (already forwarded) modifiers are held.
    fn send_chord_char(client: &ControlClient, c: char) {
        client.send(ControlMsg::Key {
            key: NamedKey::Char(c),
            pressed: true,
        });
        client.send(ControlMsg::Key {
            key: NamedKey::Char(c),
            pressed: false,
        });
    }

    /// Printable character for a key, for chords like Ctrl+C.
    fn key_char(k: egui::Key) -> Option<char> {
        use egui::Key as K;
        Some(match k {
            K::Minus => '-',
            K::Plus => '+',
            K::Equals => '=',
            K::Comma => ',',
            K::Period => '.',
            K::Slash => '/',
            K::Backslash => '\\',
            K::Semicolon => ';',
            K::Quote => '\'',
            K::OpenBracket => '[',
            K::CloseBracket => ']',
            K::Backtick => '`',
            _ => {
                // Letters and digits: the key's name is a single character.
                let mut chars = k.name().chars();
                match (chars.next(), chars.next()) {
                    (Some(c), None) if c.is_ascii_alphanumeric() => c.to_ascii_lowercase(),
                    _ => return None,
                }
            }
        })
    }
}

#[cfg(test)]
mod tests {
    //! End-to-end over real iroh-gossip: a fake "bot" node and the real daemon
    //! session, with a throwaway config dir. Needs network for relay/discovery
    //! bootstrap, so it is `#[ignore]`:
    //! `cargo test -p meshcast-cli -- --ignored daemon_session_roundtrip --test-threads=1`
    use super::*;
    use meshcast_signal::LinkConfig;

    async fn wait_for<F: Fn() -> bool>(what: &str, secs: u64, f: F) {
        let deadline = Instant::now() + Duration::from_secs(secs);
        while !f() {
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    #[ignore]
    async fn daemon_session_roundtrip() {
        // Throwaway config dir so we never touch the real one.
        let dir = std::env::temp_dir().join(format!("meshcast-it-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::env::set_var("MESHCAST_CONFIG_DIR", &dir);

        // Fake bot.
        let bot = SignalNode::new(None).await.unwrap();
        let topic = TopicId::from_bytes(rand_bytes());
        let bot_topic = bot.gossip.subscribe(topic, vec![]).await.unwrap();
        let (bot_tx, mut bot_rx) = bot_topic.split();

        // Daemon config: one link to the bot.
        let mut cfg = AppConfig::default();
        cfg.add_link(
            "Test Server".into(),
            LinkConfig {
                topic: *topic.as_bytes(),
                secret_key: rand_bytes(),
                peer_id: *bot.endpoint.id().as_bytes(),
            },
        );
        cfg.save().await.unwrap();

        let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
        let daemon = tokio::spawn(run_session(async move {
            let _ = shutdown_rx.await;
        }));

        // Bot sees the daemon join.
        let joined = tokio::time::timeout(Duration::from_secs(60), async {
            while let Some(ev) = bot_rx.next().await {
                if let Ok(Event::NeighborUp(_)) = ev {
                    return true;
                }
            }
            false
        })
        .await
        .unwrap_or(false);
        assert!(joined, "daemon never connected to the bot");
        wait_for("daemon state connected", 20, || ipc::read_state().connected).await;
        assert_eq!(
            ipc::read_state().linked_servers,
            vec!["Test Server".to_string()]
        );

        // StartStream → consent prompt appears in the state file.
        bot_tx
            .broadcast_neighbors(
                Signal::StartStream {
                    title: "IT".into(),
                    quality: "720p".into(),
                    fps: 30,
                    server: "Test Server".into(),
                }
                .encode()
                .unwrap(),
            )
            .await
            .unwrap();
        wait_for("pending request", 20, || {
            ipc::read_state().pending_request.is_some()
        })
        .await;
        assert_eq!(ipc::read_state().pending_request.unwrap().title, "IT");

        // Decline in the "app" → bot gets StreamFailed.
        ipc::send_command(&IpcCommand::Reject).unwrap();
        let failed = tokio::time::timeout(Duration::from_secs(20), async {
            while let Some(ev) = bot_rx.next().await {
                if let Ok(Event::Received(m)) = ev {
                    if let Ok(Signal::StreamFailed { reason }) = Signal::decode(&m.content) {
                        return Some(reason);
                    }
                }
            }
            None
        })
        .await
        .ok()
        .flatten();
        assert!(
            failed.as_deref().is_some_and(|r| r.contains("Declined")),
            "expected StreamFailed(Declined), got {failed:?}"
        );
        assert!(ipc::read_state().pending_request.is_none());

        // Ping/Pong.
        bot_tx
            .broadcast_neighbors(Signal::Ping.encode().unwrap())
            .await
            .unwrap();
        let pong = tokio::time::timeout(Duration::from_secs(20), async {
            while let Some(ev) = bot_rx.next().await {
                if let Ok(Event::Received(m)) = ev {
                    if let Ok(Signal::Pong) = Signal::decode(&m.content) {
                        return true;
                    }
                }
            }
            false
        })
        .await
        .unwrap_or(false);
        assert!(pong, "no Pong");

        // A control request while not streaming is denied immediately.
        bot_tx
            .broadcast_neighbors(
                Signal::ControlRequest {
                    request_id: 7,
                    viewer: "Viewer".into(),
                }
                .encode()
                .unwrap(),
            )
            .await
            .unwrap();
        let denied = tokio::time::timeout(Duration::from_secs(20), async {
            while let Some(ev) = bot_rx.next().await {
                if let Ok(Event::Received(m)) = ev {
                    if let Ok(Signal::ControlDenied { request_id, .. }) = Signal::decode(&m.content)
                    {
                        return request_id == 7;
                    }
                }
            }
            false
        })
        .await
        .unwrap_or(false);
        assert!(denied, "control request not denied");

        // Clean shutdown.
        let _ = shutdown_tx.send(());
        tokio::time::timeout(Duration::from_secs(30), daemon)
            .await
            .expect("daemon did not stop")
            .unwrap()
            .unwrap();
        bot.shutdown().await;
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn rand_bytes() -> [u8; 32] {
        let mut b = [0u8; 32];
        for (i, x) in b.iter_mut().enumerate() {
            // Not cryptographic; fine for a test topic/key.
            *x = (std::process::id() as u64 * 2654435761
                + i as u64 * 40503
                + Instant::now().elapsed().as_nanos() as u64) as u8
                ^ (i as u8).wrapping_mul(17);
        }
        b
    }
}
