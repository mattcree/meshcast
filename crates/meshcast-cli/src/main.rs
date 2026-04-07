use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use futures_lite::StreamExt;
use iroh_live::ticket::LiveTicket;
use iroh_live::Live;
use meshcast_signal::{AppConfig, Event, LinkConfig, LinkState, PairCode, PairSignal, PairToken, Signal, SignalNode, derive_pairing_topic};
use moq_media::capture::ScreenCapturer;
use moq_media::codec::{AudioCodec, VideoCodec};
use moq_media::format::{AudioPreset, DecoderBackend, PlaybackConfig, VideoPreset};
use moq_media::publish::LocalBroadcast;
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
            config.link = Some(LinkConfig::from(state));
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
                    Ok(PairSignal::PairAccepted { topic }) => {
                        return Ok(meshcast_signal::TopicId::from_bytes(topic));
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

    let state = LinkState::new(real_topic, &node.endpoint.secret_key(), bot_id);
    config.link = Some(LinkConfig::from(state));
    config.save().await?;

    println!("Linked! Run `meshcast daemon` to start listening.");
    Ok(())
}

async fn cmd_daemon() -> Result<()> {
    let config = AppConfig::load().await.unwrap_or_default();
    let state = config.link_state()
        .context("Not linked. Run `meshcast link <CODE>` first.")?;

    let node = SignalNode::new(Some(state.secret_key())).await?;
    let peer_id = state.peer_endpoint_id();

    let gossip_topic = node
        .gossip
        .subscribe_and_join(state.topic_id(), vec![peer_id])
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;

    let (sender, mut receiver) = gossip_topic.split();

    println!("Daemon running. Waiting for commands from Discord bot...");
    println!("Press Ctrl+C to stop.\n");

    let mut live: Option<(Live, LocalBroadcast)> = None;
    let expected_peer = state.peer_endpoint_id();

    loop {
        tokio::select! {
            event = receiver.next() => {
                let event = match event {
                    Some(Ok(e)) => e,
                    Some(Err(e)) => {
                        tracing::error!("Gossip error: {e}");
                        break;
                    }
                    None => break,
                };

                match event {
                    Event::Received(msg) => {
                        // Verify sender identity
                        if msg.delivered_from != expected_peer {
                            tracing::warn!(
                                "Rejected message from unknown peer {}",
                                msg.delivered_from.fmt_short()
                            );
                            continue;
                        }
                        tracing::info!("Received gossip message ({} bytes)", msg.content.len());
                        match Signal::decode(&msg.content) {
                            Ok(Signal::StartStream { title, quality: _, fps: _ }) => {
                                tracing::info!("Bot requested stream start: {title}");
                                match start_stream("meshcast".to_string()).await {
                                    Ok((l, bc, ticket)) => {
                                        tracing::info!("Streaming! Ticket: {ticket}");
                                        let signal = Signal::StreamReady { ticket };
                                        let _ = sender.broadcast_neighbors(signal.encode()?).await;
                                        live = Some((l, bc));
                                    }
                                    Err(e) => {
                                        tracing::error!("Failed to start stream: {e}");
                                    }
                                }
                            }
                            Ok(Signal::StopStream) => {
                                tracing::info!("Bot requested stream stop");
                                if let Some((l, _bc)) = live.take() {
                                    l.shutdown().await;
                                    let _ = sender.broadcast_neighbors(Signal::StreamStopped.encode()?).await;
                                    tracing::info!("Stream stopped");
                                }
                            }
                            Ok(Signal::WatchStream { ticket }) => {
                                tracing::info!("Bot requested watch: {ticket}");
                                let exe = std::env::current_exe()
                                    .unwrap_or_else(|_| "meshcast".into());
                                match meshcast_signal::launch_viewer(&exe, &ticket) {
                                    Ok(_) => tracing::info!("Viewer launched"),
                                    Err(e) => tracing::error!("Failed to launch viewer: {e}"),
                                }
                            }
                            Ok(Signal::Ping) => {
                                let _ = sender.broadcast_neighbors(Signal::Pong.encode()?).await;
                            }
                            Ok(other) => {
                                tracing::debug!("Ignoring signal: {other:?}");
                            }
                            Err(e) => {
                                tracing::warn!("Failed to decode signal: {e}");
                            }
                        }
                    }
                    Event::NeighborUp(id) => {
                        tracing::info!(peer = %id.fmt_short(), "Bot connected");
                    }
                    Event::NeighborDown(id) => {
                        tracing::warn!(peer = %id.fmt_short(), "Bot disconnected");
                    }
                    Event::Lagged => {
                        tracing::warn!("Signal receiver lagged");
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

    Ok(())
}

/// Start a stream and return the Live handle, broadcast (must stay alive!), and ticket.
async fn start_stream(name: String) -> Result<(Live, LocalBroadcast, String)> {
    let l = Live::from_env()
        .await
        .context("Failed to initialize iroh-live")?
        .with_router()
        .spawn();

    let broadcast = LocalBroadcast::new();

    let screen = ScreenCapturer::new().context("Failed to initialize screen capture")?;
    broadcast
        .video()
        .set_source(screen, VideoCodec::H264, [VideoPreset::P720])
        .context("Failed to set video source")?;

    // Try audio but don't fail if unavailable
    let audio_backend = AudioBackend::default();
    if let Ok(mic) = audio_backend.default_input().await {
        let _ = broadcast
            .audio()
            .set(mic, AudioCodec::Opus, [AudioPreset::Hq]);
    }

    l.publish(&name, &broadcast)
        .await
        .context("Failed to publish broadcast")?;

    let ticket = LiveTicket::new(l.endpoint().addr(), &name);
    Ok((l, broadcast, ticket.to_string()))
}

async fn cmd_unlink() -> Result<()> {
    let mut config = AppConfig::load().await.unwrap_or_default();
    if config.link.is_some() {
        config.link = None;
        config.save().await?;
        println!("Unlinked.");
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
