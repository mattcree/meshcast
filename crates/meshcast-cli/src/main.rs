use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use futures_lite::StreamExt;
use iroh_live::ticket::LiveTicket;
use iroh_live::Live;
use meshcast_signal::{
    AppConfig, DaemonState, Event, LinkConfig, LinkState, PairCode, PairSignal, PairToken, Signal,
    SignalNode, StreamRequest, derive_pairing_topic,
};
use moq_media::capture::ScreenCapturer;
use moq_media::codec::h264::H264Encoder;
use moq_media::codec::{AudioCodec, VideoCodec};
use moq_media::format::{AudioPreset, DecoderBackend, PlaybackConfig, VideoEncoderConfig, VideoPreset};
use moq_media::publish::{LocalBroadcast, VideoRenditions};
use moq_media::traits::VideoEncoderFactory;
use moq_media::AudioBackend;

#[derive(Parser)]
#[command(name = "meshcast", about = "P2P screen streaming for Discord via iroh-live")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Start streaming your screen
    Stream {
        /// Broadcast name (used in the ticket)
        #[arg(long, default_value = "meshcast")]
        name: String,

        /// Disable audio capture
        #[arg(long)]
        no_audio: bool,

        /// Video quality preset: 360p, 720p, 1080p
        #[arg(long, default_value = "720p")]
        quality: String,
    },

    /// Watch a stream
    Watch {
        /// Ticket string or meshcast://watch/<ticket> URI
        ticket: String,
    },

    /// Link this machine to the Discord bot for remote stream control
    Link {
        /// Pairing token from `/link` command in Discord
        token: String,
    },

    /// Run as a daemon, waiting for the bot to signal stream start/stop
    Daemon,

    /// Remove the link to the Discord bot
    Unlink,
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
        } => rt.block_on(cmd_stream(name, no_audio, quality)),
        Commands::Watch { ticket } => cmd_watch(ticket, &rt),
        Commands::Link { token } => rt.block_on(cmd_link(token)),
        Commands::Daemon => rt.block_on(cmd_daemon()),
        Commands::Unlink => rt.block_on(cmd_unlink()),
    }
}

async fn cmd_stream(name: String, no_audio: bool, quality: String) -> Result<()> {
    let live = Live::from_env()
        .await
        .context("Failed to initialize iroh-live")?
        .with_router()
        .spawn();

    let broadcast = LocalBroadcast::new();

    // Screen capture
    let screen = ScreenCapturer::new().context("Failed to initialize screen capture")?;
    let preset = match quality.as_str() {
        "360p" => VideoPreset::P360,
        "720p" => VideoPreset::P720,
        "1080p" => VideoPreset::P1080,
        _ => {
            tracing::warn!("Unknown quality '{quality}', defaulting to 720p");
            VideoPreset::P720
        }
    };
    broadcast
        .video()
        .set_source(screen, VideoCodec::H264, [preset])
        .context("Failed to set video source")?;
    tracing::info!("Screen capture started ({quality})");

    // Audio (optional)
    if !no_audio {
        let audio_backend = AudioBackend::default();
        match audio_backend.default_input().await {
            Ok(mic) => {
                broadcast
                    .audio()
                    .set(mic, AudioCodec::Opus, [AudioPreset::Hq])
                    .context("Failed to set audio source")?;
                tracing::info!("Audio capture started");
            }
            Err(e) => {
                tracing::warn!("No audio input available: {e}");
            }
        }
    }

    // Publish and print ticket
    live.publish(&name, &broadcast)
        .await
        .context("Failed to publish broadcast")?;

    let ticket = LiveTicket::new(live.endpoint().addr(), &name);
    let ticket_str = ticket.to_string();

    println!("\nStreaming! Share this ticket to let others watch:\n");
    println!("  {ticket_str}\n");
    println!("  meshcast://watch/{ticket_str}\n");
    println!("Press Ctrl+C to stop.\n");

    tokio::signal::ctrl_c().await?;
    tracing::info!("Shutting down...");
    live.shutdown().await;

    Ok(())
}

async fn cmd_link(input: String) -> Result<()> {
    let mut config = AppConfig::load().await.unwrap_or_default();

    // Try PairCode first, fall back to legacy token
    let (bot_id, pin) = match PairCode::parse(&input) {
        Ok((bot_id, pin)) => (bot_id, pin),
        Err(_) => {
            // Legacy token format
            let pair = PairToken::decode(&input).context("Invalid pairing code")?;
            let memory_lookup = iroh::address_lookup::memory::MemoryLookup::new();
            for peer in &pair.peers {
                memory_lookup.add_endpoint_info(peer.clone());
            }
            let node = SignalNode::new(None).await?;
            if let Ok(lookup) = node.endpoint.address_lookup() {
                lookup.add(memory_lookup);
            }
            let peer_ids: Vec<_> = pair.peers.iter().map(|p| p.id).collect();
            let _topic = node.gossip.subscribe_and_join(pair.topic, peer_ids.clone()).await
                .map_err(|e| anyhow::anyhow!("{e}"))?;
            let state = LinkState::new(pair.topic, &node.endpoint.secret_key(), peer_ids[0]);
            config.add_link(format!("Server {}", peer_ids[0].fmt_short()), LinkConfig::from(state));
            config.save().await?;
            println!("Linked! Run `meshcast daemon` to start listening.");
            return Ok(());
        }
    };

    tracing::info!("Pairing with bot {} using PIN", bot_id.fmt_short());
    let node = SignalNode::new(None).await?;

    // Join the pairing topic derived from PIN
    let pairing_topic = derive_pairing_topic(&pin);
    let pairing_sub = node.gossip
        .subscribe_and_join(pairing_topic, vec![bot_id])
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    let (pair_sender, mut pair_receiver) = pairing_sub.split();

    // Send PairRequest
    tracing::info!("Sending pairing request...");
    pair_sender
        .broadcast_neighbors(PairSignal::PairRequest { pin }.encode()?)
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;

    // Wait for response
    let real_topic = tokio::time::timeout(std::time::Duration::from_secs(15), async {
        while let Some(event) = pair_receiver.next().await {
            if let Ok(Event::Received(msg)) = event {
                match PairSignal::decode(&msg.content) {
                    Ok(PairSignal::PairAccepted { topic, server_name }) => {
                        return Ok((meshcast_signal::TopicId::from_bytes(topic), server_name));
                    }
                    Ok(PairSignal::PairRejected { reason }) => {
                        return Err(anyhow::anyhow!("Rejected: {reason}"));
                    }
                    _ => continue,
                }
            }
        }
        Err(anyhow::anyhow!("Connection lost"))
    })
    .await
    .map_err(|_| anyhow::anyhow!("Timed out — is the bot running?"))??;

    let (real_topic, server_name) = real_topic;
    let state = LinkState::new(real_topic, &node.endpoint.secret_key(), bot_id);
    config.add_link(server_name, LinkConfig::from(state));
    config.save().await?;

    println!("Linked! Run `meshcast daemon` to start listening.");
    Ok(())
}

/// Write daemon state to `.tray-state` for the tray and app window to read.
fn write_state(state: &DaemonState) {
    let path = AppConfig::state_path();
    if let Ok(json) = serde_json::to_string(state) {
        let _ = std::fs::write(&path, json);
    }
}

/// Read and delete `.tray-cmd`. Returns the command string if present.
fn read_cmd() -> Option<String> {
    let path = AppConfig::cmd_path();
    match std::fs::read_to_string(&path) {
        Ok(cmd) => {
            let _ = std::fs::remove_file(&path);
            let cmd = cmd.trim().to_string();
            if cmd.is_empty() { None } else { Some(cmd) }
        }
        Err(_) => None,
    }
}

async fn cmd_daemon() -> Result<()> {
    // Write PID file
    let pid_path = AppConfig::daemon_pid_path();
    if let Some(parent) = pid_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(&pid_path, std::process::id().to_string());

    let result = daemon_task().await;

    // Clean up PID and state files on exit
    let _ = std::fs::remove_file(&pid_path);
    let _ = std::fs::remove_file(AppConfig::state_path());

    result
}

/// Outer loop: restarts daemon_loop when config changes (e.g. after linking).
async fn daemon_task() -> Result<()> {
    let mut config = AppConfig::load().await.unwrap_or_default();
    loop {
        match daemon_loop(&mut config).await {
            Ok(true) => {
                config = AppConfig::load().await.unwrap_or(config.clone());
                tracing::info!("Restarting daemon loop with updated config");
                continue;
            }
            Ok(false) => break Ok(()),
            Err(e) => break Err(e),
        }
    }
}

/// Returns Ok(true) to restart, Ok(false) to exit.
async fn daemon_loop(config: &mut AppConfig) -> Result<bool> {
    let link_state = config.link_state();
    let secret_key = link_state.as_ref().map(|l| l.secret_key());

    let node = SignalNode::new(secret_key).await?;

    // Build initial daemon state
    let server_names: Vec<String> = config.links.iter().map(|l| l.name.clone()).collect();
    let mut state = DaemonState {
        linked_servers: server_names,
        quality: config.video.quality.clone(),
        fps: config.video.fps,
        ..Default::default()
    };

    // If linked, join the gossip topic
    let gossip_channel = if let Some(ref ls) = link_state {
        let topic = ls.topic_id();
        let peer_id = ls.peer_endpoint_id();

        match node.gossip.subscribe_and_join(topic, vec![peer_id]).await {
            Ok(gt) => {
                tracing::info!("Joined gossip topic");
                Some(gt.split())
            }
            Err(e) => {
                tracing::warn!("Failed to join gossip: {e}");
                None
            }
        }
    } else {
        tracing::info!("Not linked — waiting for link command");
        None
    };

    let (sender, mut receiver) = match gossip_channel {
        Some((s, r)) => (Some(s), Some(r)),
        None => (None, None),
    };

    let mut live: Option<(Live, LocalBroadcast)> = None;
    let mut pending_stream: Option<StreamRequest> = None;
    let mut active_viewers: u32 = 0;
    const MAX_VIEWERS: u32 = 5;
    let expected_peer = link_state.as_ref().map(|ls| ls.peer_endpoint_id());

    write_state(&state);
    println!("Daemon running. Press Ctrl+C to stop.\n");

    let mut cmd_interval = tokio::time::interval(std::time::Duration::from_millis(250));

    loop {
        tokio::select! {
            // Gossip events
            event = async {
                match receiver.as_mut() {
                    Some(rx) => rx.next().await,
                    None => std::future::pending().await,
                }
            } => {
                let event = match event {
                    Some(Ok(e)) => e,
                    Some(Err(e)) => {
                        tracing::error!("Gossip error: {e}");
                        state.connected = false;
                        state.error = Some(format!("Gossip error: {e}"));
                        write_state(&state);
                        break;
                    }
                    None => break,
                };

                match event {
                    Event::Received(msg) => {
                        if let Some(ref expected) = expected_peer {
                            if msg.delivered_from != *expected {
                                tracing::warn!(
                                    "Rejected message from unknown peer {}",
                                    msg.delivered_from.fmt_short()
                                );
                                continue;
                            }
                        }

                        tracing::info!("Received gossip message");
                        match Signal::decode(&msg.content) {
                            Ok(Signal::StartStream { title, quality, fps, server }) => {
                                tracing::info!("Start stream requested: {title} ({quality} {fps}fps) from {server}");
                                // Store request, wait for user consent
                                let request = StreamRequest {
                                    title: title.clone(),
                                    server: server.clone(),
                                    quality: quality.clone(),
                                    fps,
                                };
                                pending_stream = Some(request.clone());
                                state.pending_request = Some(request);
                                state.quality = quality;
                                state.fps = fps;
                                write_state(&state);

                                // Desktop notification
                                let notify_server = server;
                                let notify_title = title;
                                std::thread::spawn(move || {
                                    let _ = std::process::Command::new("notify-send")
                                        .args([
                                            "--app-name=Meshcast",
                                            "--urgency=critical",
                                            &format!("Stream Request from {notify_server}"),
                                            &format!("\"{}\" — Open Meshcast to approve", notify_title),
                                        ])
                                        .spawn();
                                });
                            }
                            Ok(Signal::StopStream) => {
                                tracing::info!("Stop stream");
                                if let Some((l, _bc)) = live.take() {
                                    l.shutdown().await;
                                    if let Some(ref s) = sender {
                                        let _ = s.broadcast_neighbors(Signal::StreamStopped.encode()?).await;
                                    }
                                    state.streaming = false;
                                    state.stream_ticket = None;
                                    state.viewers = 0;
                                    active_viewers = 0;
                                    write_state(&state);
                                    tracing::info!("Stream stopped");
                                }
                            }
                            Ok(Signal::WatchStream { ticket }) => {
                                if active_viewers >= MAX_VIEWERS {
                                    tracing::warn!("Viewer limit reached ({MAX_VIEWERS}), ignoring WatchStream");
                                } else {
                                    tracing::info!("Watch: {ticket}");
                                    let exe = std::env::current_exe()
                                        .unwrap_or_else(|_| "meshcast".into());
                                    match meshcast_signal::launch_viewer(&exe, &ticket) {
                                        Ok(_) => {
                                            active_viewers += 1;
                                            tracing::info!("Viewer launched ({active_viewers}/{MAX_VIEWERS})");
                                        }
                                        Err(e) => tracing::error!("Failed to launch viewer: {e}"),
                                    }
                                }
                            }
                            Ok(Signal::ViewerUpdate { count }) => {
                                tracing::info!("Viewer count: {count}");
                                state.viewers = count;
                                write_state(&state);
                            }
                            Ok(Signal::Ping) => {
                                if let Some(ref s) = sender {
                                    let _ = s.broadcast_neighbors(Signal::Pong.encode()?).await;
                                }
                            }
                            Ok(_) => {}
                            Err(e) => tracing::warn!("Decode error: {e}"),
                        }
                    }
                    Event::NeighborUp(id) => {
                        tracing::info!(peer = %id.fmt_short(), "Bot connected");
                        state.connected = true;
                        state.error = None;
                        write_state(&state);
                    }
                    Event::NeighborDown(id) => {
                        tracing::warn!(peer = %id.fmt_short(), "Bot disconnected");
                        state.connected = false;
                        write_state(&state);
                    }
                    _ => {}
                }
            }

            // Poll .tray-cmd for commands from tray/app
            _ = cmd_interval.tick() => {
                if let Some(cmd) = read_cmd() {
                    match cmd.as_str() {
                        "stop" => {
                            if let Some((l, _bc)) = live.take() {
                                l.shutdown().await;
                                if let Some(ref s) = sender {
                                    let _ = s.broadcast_neighbors(Signal::StreamStopped.encode()?).await;
                                }
                                state.streaming = false;
                                state.stream_ticket = None;
                                state.viewers = 0;
                                active_viewers = 0;
                                write_state(&state);
                                tracing::info!("Stream stopped via command");
                            }
                        }
                        "approve" => {
                            if let Some(req) = pending_stream.take() {
                                state.pending_request = None;
                                match start_stream("meshcast".into(), &req.quality, req.fps).await {
                                    Ok((l, bc, ticket)) => {
                                        tracing::info!("Streaming: {ticket}");
                                        if let Some(ref s) = sender {
                                            let sig = Signal::StreamReady { ticket: ticket.clone() };
                                            let _ = s.broadcast_neighbors(sig.encode()?).await;
                                        }
                                        state.streaming = true;
                                        state.stream_ticket = Some(ticket);
                                        write_state(&state);
                                        live = Some((l, bc));
                                    }
                                    Err(e) => {
                                        tracing::error!("Failed to start stream: {e}");
                                        state.error = Some(format!("Stream failed: {e}"));
                                        write_state(&state);
                                    }
                                }
                            }
                        }
                        "reject" => {
                            pending_stream = None;
                            state.pending_request = None;
                            write_state(&state);
                            tracing::info!("Stream request declined");
                        }
                        "reload" => {
                            tracing::info!("Reload requested");
                            return Ok(true); // restart with new config
                        }
                        _ if cmd.starts_with("link:") => {
                            let code = &cmd[5..];
                            match do_link(&node, code, config).await {
                                Ok(_) => {
                                    tracing::info!("Link successful, restarting daemon loop");
                                    return Ok(true); // restart with new config
                                }
                                Err(e) => {
                                    tracing::error!("Link failed: {e}");
                                    state.error = Some(format!("Link failed: {e}"));
                                    write_state(&state);
                                }
                            }
                        }
                        other => {
                            tracing::debug!("Unknown command: {other}");
                        }
                    }
                }
            }

            _ = tokio::signal::ctrl_c() => {
                tracing::info!("Shutting down daemon...");
                if let Some((l, _bc)) = live.take() {
                    l.shutdown().await;
                }
                break;
            }
        }
    }

    Ok(false)
}

/// Pair with a Discord bot using a pairing code (PIN or legacy token).
async fn do_link(
    node: &SignalNode,
    input: &str,
    config: &mut AppConfig,
) -> Result<()> {
    let (bot_id, pin) = match PairCode::parse(input) {
        Ok((bot_id, pin)) => (bot_id, pin),
        Err(_) => {
            return do_link_legacy(node, input, config).await;
        }
    };

    tracing::info!("Pairing with bot {} using PIN", bot_id.fmt_short());

    let pairing_topic = derive_pairing_topic(&pin);
    let pairing_sub = node
        .gossip
        .subscribe_and_join(pairing_topic, vec![bot_id])
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    let (pair_sender, mut pair_receiver) = pairing_sub.split();

    pair_sender
        .broadcast_neighbors(PairSignal::PairRequest { pin }.encode()?)
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;

    let real_topic = tokio::time::timeout(std::time::Duration::from_secs(15), async {
        while let Some(event) = pair_receiver.next().await {
            if let Ok(Event::Received(msg)) = event {
                match PairSignal::decode(&msg.content) {
                    Ok(PairSignal::PairAccepted { topic, server_name }) => {
                        return Ok((meshcast_signal::TopicId::from_bytes(topic), server_name));
                    }
                    Ok(PairSignal::PairRejected { reason }) => {
                        return Err(anyhow::anyhow!("Rejected: {reason}"));
                    }
                    _ => continue,
                }
            }
        }
        Err(anyhow::anyhow!("Connection lost"))
    })
    .await
    .map_err(|_| anyhow::anyhow!("Timed out — is the bot running?"))??;

    let (real_topic, server_name) = real_topic;
    let link_state = LinkState::new(real_topic, &node.endpoint.secret_key(), bot_id);
    config.add_link(server_name, LinkConfig::from(link_state));
    config.save().await?;

    Ok(())
}

async fn do_link_legacy(
    node: &SignalNode,
    token: &str,
    config: &mut AppConfig,
) -> Result<()> {
    let pair = PairToken::decode(token)?;

    let memory_lookup = iroh::address_lookup::memory::MemoryLookup::new();
    for peer in &pair.peers {
        memory_lookup.add_endpoint_info(peer.clone());
    }
    if let Ok(lookup) = node.endpoint.address_lookup() {
        lookup.add(memory_lookup);
    }

    let peer_ids: Vec<_> = pair.peers.iter().map(|p| p.id).collect();
    let _topic = node
        .gossip
        .subscribe_and_join(pair.topic, peer_ids.clone())
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;

    let link_state = LinkState::new(pair.topic, &node.endpoint.secret_key(), peer_ids[0]);
    config.add_link(
        format!("Server {}", peer_ids[0].fmt_short()),
        LinkConfig::from(link_state),
    );
    config.save().await?;

    Ok(())
}

/// Start a stream with quality/fps passthrough and return the Live handle, broadcast, and ticket.
async fn start_stream(name: String, quality: &str, fps: u32) -> Result<(Live, LocalBroadcast, String)> {
    let l = Live::from_env()
        .await
        .context("Failed to initialize iroh-live")?
        .with_router()
        .spawn();

    let broadcast = LocalBroadcast::new();

    let screen = ScreenCapturer::new().context("Failed to initialize screen capture")?;
    let preset = match quality {
        "360p" => VideoPreset::P360,
        "720p" => VideoPreset::P720,
        "1080p" => VideoPreset::P1080,
        _ => VideoPreset::P720,
    };

    if fps != 30 {
        // Custom FPS: build renditions manually with VideoEncoderConfig
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
    } else {
        broadcast
            .video()
            .set_source(screen, VideoCodec::H264, [preset])
            .context("Failed to set video source")?;
    }

    let audio_backend = AudioBackend::default();
    if let Ok(mic) = audio_backend.default_input().await {
        let _ = broadcast
            .audio()
            .set(mic, AudioCodec::Opus, [AudioPreset::Hq]);
    }

    l.publish(&name, &broadcast)
        .await
        .context("Failed to publish")?;

    let ticket = LiveTicket::new(l.endpoint().addr(), &name);
    Ok((l, broadcast, ticket.to_string()))
}

async fn cmd_unlink() -> Result<()> {
    let mut config = AppConfig::load().await.unwrap_or_default();
    if !config.links.is_empty() || config.link.is_some() {
        config.links.clear();
        config.link = None;
        config.save().await?;
        println!("Unlinked from all servers.");
    } else {
        println!("Not linked.");
    }
    // Also clean up legacy link file if it exists
    let legacy = dirs_next::home_dir().unwrap_or_default().join(".config/meshcast/link.json");
    let _ = tokio::fs::remove_file(&legacy).await;
    Ok(())
}

/// Watch command — sets up async connection, then runs eframe on the main thread.
fn cmd_watch(raw: String, rt: &tokio::runtime::Runtime) -> Result<()> {
    use moq_media_egui::{VideoTrackView, create_egui_wgpu_config};

    let ticket_str = parse_ticket_uri(&raw);
    let ticket: LiveTicket = ticket_str
        .parse()
        .context("Invalid ticket string")?;

    // Async setup: connect and subscribe
    let (live, sub, tracks) = rt.block_on(async {
        tracing::info!("Connecting to stream '{}'...", ticket.broadcast_name);

        let live = Live::from_env()
            .await
            .context("Failed to initialize iroh-live")?
            .spawn();

        let sub = live
            .subscribe(ticket.endpoint, &ticket.broadcast_name)
            .await
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
            .context("Failed to initialize media tracks")?;

        tracing::info!("Connected.");
        anyhow::Ok((live, sub, tracks))
    })?;

    // eframe must run on the main thread
    let _guard = rt.enter();
    let native_options = eframe::NativeOptions {
        renderer: eframe::Renderer::Wgpu,
        wgpu_options: create_egui_wgpu_config(),
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

        ctx.request_repaint_after(std::time::Duration::from_millis(16));

        // Detect stream end
        if !self.stream_ended && self.broadcast.shutdown_token().is_cancelled() {
            self.stream_ended = true;
        }

        egui::CentralPanel::default()
            .frame(egui::Frame::new().inner_margin(0.0).fill(egui::Color32::BLACK))
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
                        ui.label("Audio only — no video track");
                    });
                }
            });
    }

    fn on_exit(&mut self) {
        tracing::info!("Exiting viewer");
        self.sub.session().close(0, b"bye");
        // Can't block_on shutdown here cleanly, but dropping will clean up
    }
}

/// Strips the `meshcast://watch/` prefix if present.
fn parse_ticket_uri(raw: &str) -> &str {
    raw.strip_prefix("meshcast://watch/")
        .or_else(|| raw.strip_prefix("meshcast:///watch/"))
        .unwrap_or(raw)
}
