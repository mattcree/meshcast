//! `meshcast` — CLI and background daemon.
//!
//! * `meshcast daemon` is the long-running process that talks to the Discord
//!   bot(s) over gossip, starts/stops screen capture and launches viewer
//!   windows. The GUI (`meshcast-app`) and tray are thin clients of it.
//! * `meshcast watch <ticket>` is the viewer window.
//! * `meshcast stream` / `link` / `unlink` / `status` are manual equivalents.

use std::path::PathBuf;
use std::process::Child;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use futures_lite::StreamExt;
use iroh_live::ticket::LiveTicket;
use iroh_live::Live;
use meshcast_signal::ipc::{self, Command as IpcCommand};
use meshcast_signal::process;
use meshcast_signal::{
    derive_pairing_topic, normalize_fps, normalize_quality, sanitize_title, AppConfig, DaemonState,
    EndpointId, Event, GossipSender, LinkConfig, LinkState, PairCode, PairSignal, ServerLink,
    Signal, SignalNode, StreamRequest, TopicId,
};
use moq_media::capture::ScreenCapturer;
use moq_media::codec::h264::H264Encoder;
use moq_media::codec::{AudioCodec, VideoCodec};
use moq_media::format::{
    AudioPreset, DecoderBackend, PlaybackConfig, VideoEncoderConfig, VideoPreset,
};
use moq_media::publish::{LocalBroadcast, VideoRenditions};
use moq_media::traits::VideoEncoderFactory;
use moq_media::AudioBackend;
use tokio::sync::{mpsc, oneshot};

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
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "meshcast=info,iroh_live=info".into()),
        )
        .init();

    let cli = Cli::parse();

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
}

impl ActiveStream {
    async fn stop(self) {
        self.live.shutdown().await;
    }
}

/// Start screen capture + publish. Returns the live handle and ticket.
async fn start_stream(name: &str, quality: &str, fps: u32, audio: bool) -> Result<ActiveStream> {
    let quality = normalize_quality(quality);
    let fps = normalize_fps(fps);

    let live = Live::from_env()
        .await
        .context("Failed to initialise iroh-live")?
        .with_router()
        .spawn();

    let broadcast = LocalBroadcast::new();

    let screen = ScreenCapturer::new().context("Failed to start screen capture")?;
    let preset = match quality {
        "360p" => VideoPreset::P360,
        "1080p" => VideoPreset::P1080,
        _ => VideoPreset::P720,
    };

    if fps == meshcast_signal::DEFAULT_FPS {
        broadcast
            .video()
            .set_source(screen, VideoCodec::H264, [preset])
            .context("Failed to set video source")?;
    } else {
        // Custom FPS: build the rendition manually so we can set the framerate.
        let enc_config = VideoEncoderConfig::from_preset(preset).framerate(fps);
        let video_config = H264Encoder::config_for(&enc_config);
        let mut renditions = VideoRenditions::empty(screen);
        renditions.add_with_callback(
            format!("video/h264-openh264-{quality}-{fps}fps"),
            video_config.into(),
            move || H264Encoder::with_config(enc_config.clone()),
        );
        broadcast
            .video()
            .set(renditions)
            .context("Failed to set video source")?;
    }
    tracing::info!("Screen capture started ({quality} {fps}fps)");

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

    let ticket = LiveTicket::new(live.endpoint().addr(), name).to_string();
    Ok(ActiveStream {
        live,
        _broadcast: broadcast,
        ticket,
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
    let stream = start_stream(&name, &quality, fps, !no_audio).await?;

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
    println!("Config:  {}", AppConfig::config_path().display());
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

    let result = run_session().await;

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
}

impl LinkConn {
    fn is_active(&self) -> bool {
        self.sender.is_some()
    }

    async fn send(&self, signal: Signal) {
        let Some(sender) = &self.sender else {
            return;
        };
        match signal.encode() {
            Ok(bytes) => {
                if let Err(e) = sender.broadcast_neighbors(bytes).await {
                    tracing::warn!(link = %self.name, "Failed to send {signal:?}: {e}");
                }
            }
            Err(e) => tracing::error!("Failed to encode {signal:?}: {e}"),
        }
    }

    fn deactivate(&mut self) {
        self.sender = None;
        self.stop = None; // drops the oneshot → receiver task exits
        self.connected = false;
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
                    let Some(event) = event else { break };
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
    })
}

struct Session {
    config: AppConfig,
    links: Vec<LinkConn>,
    state: DaemonState,
    active: Option<(usize, ActiveStream)>,
    pending: Option<(usize, StreamRequest, Instant)>,
    viewers: Vec<Child>,
}

impl Session {
    fn publish_state(&mut self) {
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
            if conn.is_active()
                && !new_config
                    .links
                    .iter()
                    .any(|l| l.config.topic == conn.topic)
            {
                tracing::info!(link = %conn.name, "Link removed");
                conn.deactivate();
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
                .any(|c| c.is_active() && c.topic == sl.config.topic);
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

    async fn send_to(&self, link_idx: usize, signal: Signal) {
        if let Some(link) = self.links.get(link_idx) {
            link.send(signal).await;
        }
    }

    async fn stop_stream(&mut self, reason: &str) {
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

    async fn approve_pending(&mut self) {
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
        match start_stream("meshcast", &req.quality, req.fps, audio).await {
            Ok(stream) => {
                tracing::info!("Streaming: {}", stream.ticket);
                self.send_to(
                    idx,
                    Signal::StreamReady {
                        ticket: stream.ticket.clone(),
                    },
                )
                .await;
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
            Signal::StopStream => self.stop_stream("requested by bot").await,
            Signal::WatchStream { ticket } => {
                tracing::info!("Watch requested");
                self.launch_viewer(&ticket);
            }
            Signal::ViewerUpdate { count } => {
                self.state.viewers = count;
                self.publish_state();
            }
            Signal::Ping => self.send_to(idx, Signal::Pong).await,
            Signal::Pong
            | Signal::StreamReady { .. }
            | Signal::StreamStopped
            | Signal::StreamFailed { .. } => {
                // App-originated signals echoed back over gossip; ignore.
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
            IpcCommand::Approve => self.approve_pending().await,
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
                .args(["--app-name=Meshcast", "--urgency=critical", &summary, &body])
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
        IpcCommand::Approve => "approve",
        IpcCommand::Reject => "reject",
        IpcCommand::Reload => "reload",
        IpcCommand::Link(_) => "link",
        IpcCommand::Unknown(_) => "unknown",
    }
}

async fn run_session() -> Result<()> {
    let config = AppConfig::load().await.unwrap_or_else(|e| {
        tracing::error!("Config unreadable, using defaults: {e:#}");
        AppConfig::default()
    });

    // The first link's key gives the daemon a stable identity; the bot does not
    // verify app identity, so one key works for every link.
    let secret_key = config.link_state().map(|l| l.secret_key());
    let node = SignalNode::new(secret_key).await?;

    let (tx, mut rx) = mpsc::channel::<LinkEvent>(256);

    let mut session = Session {
        state: DaemonState::default(),
        config: AppConfig::default(),
        links: Vec::new(),
        active: None,
        pending: None,
        viewers: Vec::new(),
    };
    session.apply_config(&node, &tx, config).await;
    if session.links.is_empty() {
        tracing::info!("Not linked — waiting for a pairing code (paste it into the Meshcast app)");
    }
    println!("Daemon running. Press Ctrl+C to stop.");

    let mut tick = tokio::time::interval(TICK);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let shutdown = wait_for_shutdown_signal();
    tokio::pin!(shutdown);

    loop {
        tokio::select! {
            biased;

            _ = &mut shutdown => {
                tracing::info!("Shutting down daemon...");
                break;
            }

            Some((idx, event)) = rx.recv() => {
                let Some(link) = session.links.get(idx) else { continue };
                if !link.is_active() {
                    continue; // stale event from a removed link
                }
                match event {
                    Ok(Event::Received(msg)) => {
                        if msg.delivered_from != link.peer_id {
                            tracing::warn!(
                                "Rejected message from unexpected peer {}",
                                msg.delivered_from.fmt_short()
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
                        }
                        session.state.error = None;
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
                        tracing::error!("Gossip error on link {idx}: {e}");
                        if let Some(link) = session.links.get_mut(idx) {
                            link.connected = false;
                        }
                        session.state.error = Some(format!("Connection error: {e}"));
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
                session.reap_viewers();

                if let Some(cmd) = ipc::take_command() {
                    tracing::debug!("Command: {}", command_label(&cmd));
                    session.handle_command(&node, &tx, cmd).await;
                }
            }
        }
    }

    session.shutdown().await;
    node.shutdown().await;
    Ok(())
}

// ---------------------------------------------------------------------------
// Viewer
// ---------------------------------------------------------------------------

/// Watch command — connects, then runs eframe on the main thread.
fn cmd_watch(raw: String, rt: &tokio::runtime::Runtime) -> Result<()> {
    use moq_media_egui::{create_egui_wgpu_config, VideoTrackView};

    let ticket_str = meshcast_signal::parse_ticket_uri(&raw);
    let ticket: LiveTicket = match meshcast_signal::validate_ticket(ticket_str)
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
        let playback_config = PlaybackConfig {
            backend: DecoderBackend::Software,
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
            }))
        }),
    )
    .map_err(|e| anyhow::anyhow!("eframe error: {e}"))?;

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
}

impl eframe::App for WatchApp {
    fn update(&mut self, ctx: &eframe::egui::Context, _frame: &mut eframe::Frame) {
        use eframe::egui;

        ctx.request_repaint_after(Duration::from_millis(16));

        // Detect stream end
        if !self.stream_ended && self.broadcast.shutdown_token().is_cancelled() {
            self.stream_ended = true;
        }

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
                    ui.add_sized(avail, img);
                } else {
                    ui.centered_and_justified(|ui| {
                        ui.label(
                            egui::RichText::new("Audio only — no video track")
                                .color(egui::Color32::WHITE),
                        );
                    });
                }
            });
    }

    fn on_exit(&mut self) {
        tracing::info!("Exiting viewer");
        self.sub.session().close(0, b"bye");
    }
}
