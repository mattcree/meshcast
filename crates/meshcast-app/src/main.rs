use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use eframe::egui;
use futures_lite::StreamExt;
use iroh_live::ticket::LiveTicket;
use iroh_live::Live;
use meshcast_signal::{AppConfig, Event, LinkConfig, PairCode, PairToken, Signal, SignalNode};
use moq_media::capture::ScreenCapturer;
use moq_media::codec::{AudioCodec, VideoCodec};
use moq_media::format::{AudioPreset, VideoPreset};
use moq_media::publish::LocalBroadcast;
use moq_media::AudioBackend;
use tokio::sync::mpsc;

fn create_tray_icon() -> Option<tray_icon::TrayIcon> {
    use tray_icon::menu::{Menu, MenuItemBuilder};
    use tray_icon::TrayIconBuilder;

    let menu = Menu::new();
    let show_item = MenuItemBuilder::new()
        .text("Show")
        .id(tray_icon::menu::MenuId("show".into()))
        .build();
    let quit_item = MenuItemBuilder::new()
        .text("Quit")
        .id(tray_icon::menu::MenuId("quit".into()))
        .build();
    menu.append(&show_item).ok();
    menu.append(&quit_item).ok();

    // Simple 16x16 green square icon (RGBA)
    let size = 16u32;
    let mut rgba = vec![0u8; (size * size * 4) as usize];
    for pixel in rgba.chunks_exact_mut(4) {
        pixel[0] = 0x58; // R
        pixel[1] = 0x65; // G
        pixel[2] = 0xF2; // B (Discord blurple)
        pixel[3] = 0xFF; // A
    }
    let icon = tray_icon::Icon::from_rgba(rgba, size, size).ok()?;

    TrayIconBuilder::new()
        .with_menu(Box::new(menu))
        .with_tooltip("Meshcast")
        .with_icon(icon)
        .build()
        .ok()
}

/// Messages from the gossip background task to the UI.
#[derive(Debug)]
enum UiEvent {
    Connected,
    Disconnected,
    StreamRequested { title: String },
    StreamStarted { ticket: String },
    StreamFailed { error: String },
    WatchRequested,
    ViewerCount(u32),
    Linked,
}

/// Messages from the UI to the gossip background task.
#[derive(Debug)]
enum DaemonCmd {
    Link { token: String },
    StopStream,
}

/// Shared state between UI and background tasks.
struct AppState {
    config: AppConfig,
    is_linked: bool,
    is_connected: bool,
    is_streaming: bool,
    viewer_count: u32,
    stream_ticket: Option<String>,
    status_msg: String,
    link_token_input: String,
}

impl Default for AppState {
    fn default() -> Self {
        Self {
            config: AppConfig::default(),
            is_linked: false,
            is_connected: false,
            is_streaming: false,
            viewer_count: 0,
            stream_ticket: None,
            status_msg: "Starting...".into(),
            link_token_input: String::new(),
        }
    }
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "meshcast_app=info,iroh_live=info,iroh=warn".into()),
        )
        .init();

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("Failed to build tokio runtime")?;

    // Load config
    let config = rt.block_on(AppConfig::load()).unwrap_or_default();
    let state = Arc::new(Mutex::new(AppState {
        is_linked: config.link.is_some(),
        config: config.clone(),
        status_msg: if config.link.is_some() {
            "Linked. Connecting to bot...".into()
        } else {
            "Not linked. Paste a link token to connect.".into()
        },
        ..Default::default()
    }));

    let (ui_tx, ui_rx) = mpsc::unbounded_channel::<UiEvent>();
    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel::<DaemonCmd>();

    // Spawn the background daemon
    let state_clone = state.clone();
    let _guard = rt.enter();
    rt.spawn(daemon_task(config, ui_tx, cmd_rx, state_clone));

    // Run eframe on the main thread
    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Meshcast")
            .with_inner_size([420.0, 360.0])
            .with_min_inner_size([360.0, 300.0]),
        ..Default::default()
    };

    // Initialize GTK on Linux (required for tray-icon)
    #[cfg(target_os = "linux")]
    gtk::init().ok();

    eframe::run_native(
        "Meshcast",
        native_options,
        Box::new(move |_cc| {
            // Create tray icon
            let tray = create_tray_icon();

            Ok(Box::new(MeshcastApp {
                state,
                ui_rx,
                cmd_tx,
                visible: true,
                quit: false,
                _tray: tray,
            }))
        }),
    )
    .map_err(|e| anyhow::anyhow!("eframe error: {e}"))?;

    Ok(())
}

struct MeshcastApp {
    state: Arc<Mutex<AppState>>,
    ui_rx: mpsc::UnboundedReceiver<UiEvent>,
    cmd_tx: mpsc::UnboundedSender<DaemonCmd>,
    visible: bool,
    quit: bool,
    _tray: Option<tray_icon::TrayIcon>,
}

impl eframe::App for MeshcastApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Handle window close → minimize to tray instead of quitting
        if ctx.input(|i| i.viewport().close_requested()) && !self.quit {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(true));
            self.visible = false;
        }

        // Check tray icon events — show window on click
        if let Ok(tray_icon::TrayIconEvent::Click { .. }) = tray_icon::TrayIconEvent::receiver().try_recv() {
            self.visible = true;
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
            ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
        }
        if let Ok(event) = tray_icon::menu::MenuEvent::receiver().try_recv() {
            match event.id().0.as_str() {
                "show" => {
                    self.visible = true;
                    ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
                    ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
                }
                "quit" => {
                    self.quit = true;
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }
                _ => {}
            }
        }

        // Drain UI events from background task
        while let Ok(event) = self.ui_rx.try_recv() {
            let mut s = self.state.lock().expect("poisoned");
            match event {
                UiEvent::Connected => {
                    s.is_connected = true;
                    s.status_msg = "Connected to bot.".into();
                }
                UiEvent::Disconnected => {
                    s.is_connected = false;
                    s.status_msg = "Disconnected from bot.".into();
                }
                UiEvent::StreamRequested { title } => {
                    s.status_msg = format!("Starting stream: {title}...");
                }
                UiEvent::StreamStarted { ticket } => {
                    s.is_streaming = true;
                    s.stream_ticket = Some(ticket);
                    s.status_msg = "Streaming!".into();
                }
                UiEvent::StreamFailed { error } => {
                    s.status_msg = format!("Stream failed: {error}");
                }
                UiEvent::WatchRequested => {
                    s.status_msg = "Opening viewer...".into();
                }
                UiEvent::ViewerCount(count) => {
                    s.viewer_count = count;
                }
                UiEvent::Linked => {
                    s.is_linked = true;
                    s.status_msg = "Linked! Connecting to bot...".into();
                }
            }
        }

        // Request periodic repaint for status updates
        ctx.request_repaint_after(std::time::Duration::from_millis(250));

        egui::CentralPanel::default().show(ctx, |ui| {
            let s = self.state.lock().expect("poisoned");

            ui.heading("Meshcast");
            ui.separator();

            // Status
            ui.horizontal(|ui| {
                let (color, label) = if s.is_streaming {
                    (egui::Color32::RED, "LIVE")
                } else if s.is_connected {
                    (egui::Color32::GREEN, "Connected")
                } else if s.is_linked {
                    (egui::Color32::YELLOW, "Linked (offline)")
                } else {
                    (egui::Color32::GRAY, "Not linked")
                };
                ui.colored_label(color, format!("● {label}"));
            });
            ui.label(&s.status_msg);
            ui.add_space(8.0);

            // Link section
            if !s.is_linked {
                ui.group(|ui| {
                    ui.label("Enter pairing code from Discord /link command:");
                    drop(s); // release lock for mutable access
                    let mut s = self.state.lock().expect("poisoned");
                    let response = ui.text_edit_singleline(&mut s.link_token_input);
                    if (response.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)))
                        || ui.button("Link").clicked()
                    {
                        let token = s.link_token_input.clone();
                        if !token.is_empty() {
                            let _ = self.cmd_tx.send(DaemonCmd::Link { token });
                            s.link_token_input.clear();
                            s.status_msg = "Linking...".into();
                        }
                    }
                });
            } else {
                // Config section
                ui.group(|ui| {
                    ui.label("Video Quality");
                    drop(s);
                    let mut s = self.state.lock().expect("poisoned");
                    egui::ComboBox::from_label("")
                        .selected_text(&s.config.video.quality)
                        .show_ui(ui, |ui| {
                            ui.selectable_value(&mut s.config.video.quality, "360p".into(), "360p");
                            ui.selectable_value(&mut s.config.video.quality, "720p".into(), "720p");
                            ui.selectable_value(
                                &mut s.config.video.quality,
                                "1080p".into(),
                                "1080p",
                            );
                        });

                    ui.horizontal(|ui| {
                        ui.label("FPS:");
                        ui.selectable_value(&mut s.config.video.fps, 30, "30");
                        ui.selectable_value(&mut s.config.video.fps, 60, "60");
                    });

                    ui.checkbox(&mut s.config.audio.enabled, "Audio capture");
                });

                let s = self.state.lock().expect("poisoned");
                ui.add_space(4.0);

                // Stream controls
                if s.is_streaming {
                    let vc = s.viewer_count;
                    ui.label(format!(
                        "{vc} viewer{}",
                        if vc == 1 { "" } else { "s" }
                    ));
                    drop(s);
                    if ui.button("Stop Stream").clicked() {
                        let _ = self.cmd_tx.send(DaemonCmd::StopStream);
                    }
                }
            }
        });
    }
}

/// Background task: manages gossip connection and stream lifecycle.
/// Restarts the loop when config changes (e.g. after linking).
async fn daemon_task(
    mut config: AppConfig,
    ui_tx: mpsc::UnboundedSender<UiEvent>,
    mut cmd_rx: mpsc::UnboundedReceiver<DaemonCmd>,
    state: Arc<Mutex<AppState>>,
) {
    loop {
        match daemon_loop(&mut config, &ui_tx, &mut cmd_rx, &state).await {
            Ok(restart) => {
                if restart {
                    // Reload config and restart
                    config = AppConfig::load().await.unwrap_or(config.clone());
                    tracing::info!("Restarting daemon loop with updated config");
                    continue;
                }
                break;
            }
            Err(e) => {
                tracing::error!("Daemon error: {e}");
                break;
            }
        }
    }
}

/// Returns Ok(true) to restart, Ok(false) to exit.
async fn daemon_loop(
    config: &mut AppConfig,
    ui_tx: &mpsc::UnboundedSender<UiEvent>,
    cmd_rx: &mut mpsc::UnboundedReceiver<DaemonCmd>,
    state: &Arc<Mutex<AppState>>,
) -> Result<bool> {
    let link_state = config.link_state();
    let secret_key = link_state.as_ref().map(|l| l.secret_key());

    let node = SignalNode::new(secret_key).await?;

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
        None
    };

    let (sender, mut receiver) = match gossip_channel {
        Some((s, r)) => (Some(s), Some(r)),
        None => (None, None),
    };

    let mut live: Option<(Live, LocalBroadcast)> = None;

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
                        let _ = ui_tx.send(UiEvent::Disconnected);
                        break;
                    }
                    None => break,
                };

                match event {
                    Event::Received(msg) => {
                        tracing::info!("Received gossip message");
                        match Signal::decode(&msg.content) {
                            Ok(Signal::StartStream { title }) => {
                                tracing::info!("Start stream: {title}");
                                let _ = ui_tx.send(UiEvent::StreamRequested { title });

                                let quality = state.lock().expect("poisoned").config.video.quality.clone();
                                match start_stream("meshcast".into(), &quality).await {
                                    Ok((l, bc, ticket)) => {
                                        tracing::info!("Streaming: {ticket}");
                                        if let Some(ref s) = sender {
                                            let sig = Signal::StreamReady { ticket: ticket.clone() };
                                            let _ = s.broadcast_neighbors(sig.encode()?).await;
                                        }
                                        let _ = ui_tx.send(UiEvent::StreamStarted { ticket });
                                        live = Some((l, bc));
                                    }
                                    Err(e) => {
                                        tracing::error!("Failed to start stream: {e}");
                                        let _ = ui_tx.send(UiEvent::StreamFailed { error: e.to_string() });
                                    }
                                }
                            }
                            Ok(Signal::StopStream) => {
                                tracing::info!("Stop stream");
                                if let Some((l, _bc)) = live.take() {
                                    l.shutdown().await;
                                    if let Some(ref s) = sender {
                                        let _ = s.broadcast_neighbors(Signal::StreamStopped.encode()?).await;
                                    }
                                    state.lock().expect("poisoned").is_streaming = false;
                                    state.lock().expect("poisoned").status_msg = "Stream stopped.".into();
                                }
                            }
                            Ok(Signal::WatchStream { ticket }) => {
                                tracing::info!("Watch: {ticket}");
                                let _ = ui_tx.send(UiEvent::WatchRequested);
                                let exe = std::env::current_exe().unwrap_or_else(|_| "meshcast".into());
                                // Use meshcast CLI for viewer (it has the egui viewer)
                                let meshcast_cli = exe.parent()
                                    .map(|p| p.join("meshcast"))
                                    .unwrap_or_else(|| "meshcast".into());
                                match std::process::Command::new("setsid")
                                    .args([meshcast_cli.as_os_str(), std::ffi::OsStr::new("watch"), std::ffi::OsStr::new(&ticket)])
                                    .stdin(std::process::Stdio::null())
                                    .stdout(std::process::Stdio::null())
                                    .stderr(std::process::Stdio::null())
                                    .spawn()
                                {
                                    Ok(_) => tracing::info!("Viewer launched"),
                                    Err(e) => tracing::error!("Failed to launch viewer: {e}"),
                                }
                            }
                            Ok(Signal::ViewerUpdate { count }) => {
                                tracing::info!("Viewer count: {count}");
                                let _ = ui_tx.send(UiEvent::ViewerCount(count));
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
                        let _ = ui_tx.send(UiEvent::Connected);
                    }
                    Event::NeighborDown(id) => {
                        tracing::warn!(peer = %id.fmt_short(), "Bot disconnected");
                        let _ = ui_tx.send(UiEvent::Disconnected);
                    }
                    _ => {}
                }
            }

            // UI commands
            cmd = cmd_rx.recv() => {
                match cmd {
                    Some(DaemonCmd::Link { token }) => {
                        match do_link(&node, &token, config, ui_tx).await {
                            Ok(_) => {
                                tracing::info!("Link successful, restarting daemon loop");
                                return Ok(true); // restart with new config
                            }
                            Err(e) => {
                                tracing::error!("Link failed: {e}");
                                state.lock().expect("poisoned").status_msg = format!("Link failed: {e}");
                            }
                        }
                    }
                    Some(DaemonCmd::StopStream) => {
                        if let Some((l, _bc)) = live.take() {
                            l.shutdown().await;
                            if let Some(ref s) = sender {
                                let _ = s.broadcast_neighbors(Signal::StreamStopped.encode()?).await;
                            }
                            state.lock().expect("poisoned").is_streaming = false;
                            state.lock().expect("poisoned").status_msg = "Stream stopped.".into();
                        }
                    }
                    None => break,
                }
            }
        }
    }

    Ok(false)
}

async fn do_link(
    node: &SignalNode,
    input: &str,
    config: &mut AppConfig,
    ui_tx: &mpsc::UnboundedSender<UiEvent>,
) -> Result<()> {
    // Try parsing as short code first, fall back to legacy token
    let (bot_id, topic) = match PairCode::parse(input) {
        Ok((Some(bot_id), _pin)) => {
            // Full code — we have the bot's endpoint ID but need the topic from legacy path
            // For now, fall back to legacy PairToken which includes the topic
            tracing::info!("Parsed short code, bot endpoint: {}", bot_id.fmt_short());
            // TODO: implement PIN-based topic exchange with bot
            // For now, try legacy format
            return do_link_legacy(node, input, config, ui_tx).await;
        }
        Ok((None, _pin)) => {
            // PIN only — need cached bot endpoint ID
            let cached_link = config.link_state();
            match cached_link {
                Some(ls) => (ls.peer_endpoint_id(), ls.topic_id()),
                None => anyhow::bail!("No previous connection. Use the full pairing code for first-time setup."),
            }
        }
        Err(_) => {
            // Try legacy token format
            return do_link_legacy(node, input, config, ui_tx).await;
        }
    };

    // PIN-only re-link: use cached bot endpoint ID and existing topic
    let _topic = node
        .gossip
        .subscribe_and_join(topic, vec![bot_id])
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;

    let link_state = meshcast_signal::LinkState::new(
        topic,
        &node.endpoint.secret_key(),
        bot_id,
    );
    config.link = Some(LinkConfig::from(link_state));
    config.save().await?;

    let _ = ui_tx.send(UiEvent::Linked);
    Ok(())
}

async fn do_link_legacy(
    node: &SignalNode,
    token: &str,
    config: &mut AppConfig,
    ui_tx: &mpsc::UnboundedSender<UiEvent>,
) -> Result<()> {
    let pair = PairToken::from_str(token)?;

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

    let link_state = meshcast_signal::LinkState::new(
        pair.topic,
        &node.endpoint.secret_key(),
        peer_ids[0],
    );
    config.link = Some(LinkConfig::from(link_state));
    config.save().await?;

    let _ = ui_tx.send(UiEvent::Linked);
    Ok(())
}

async fn start_stream(name: String, quality: &str) -> Result<(Live, LocalBroadcast, String)> {
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
    broadcast
        .video()
        .set_source(screen, VideoCodec::H264, [preset])
        .context("Failed to set video source")?;

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
