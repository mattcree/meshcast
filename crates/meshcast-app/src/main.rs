use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use eframe::egui;
use futures_lite::StreamExt;
use iroh_live::ticket::LiveTicket;
use iroh_live::Live;
use meshcast_signal::{AppConfig, Event, LinkConfig, PairCode, PairToken, Signal, SignalNode};
use moq_media::capture::ScreenCapturer;
use moq_media::codec::{AudioCodec, VideoCodec, h264::H264Encoder};
use moq_media::format::{AudioPreset, VideoEncoderConfig, VideoPreset};
use moq_media::publish::{LocalBroadcast, VideoRenditions};
use moq_media::traits::VideoEncoderFactory;
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

/// Write tray state to a file for the Python tray subprocess to read.
fn write_tray_state(streaming: bool, connected: bool, quality: &str, fps: u32, viewers: u32) {
    let state_path = std::env::var("HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_default()
        .join(".config/meshcast/.tray-state");
    let json = format!(
        r#"{{"streaming":{},"connected":{},"quality":"{}","fps":{},"viewers":{}}}"#,
        streaming, connected, quality, fps, viewers
    );
    let _ = std::fs::write(&state_path, json);
}

/// Messages from the gossip background task to the UI.
#[derive(Debug)]
enum UiEvent {
    Connected,
    Disconnected,
    StreamRequested { title: String, server: String },
    StreamStarted { ticket: String },
    StreamFailed { error: String },
    WatchRequested,
    ViewerCount(u32),
    Linked { config: AppConfig },
}

/// Messages from the UI to the gossip background task.
#[derive(Debug)]
enum DaemonCmd {
    Link { token: String },
    StopStream,
    ApproveStream,
    RejectStream,
}

/// Shared state between UI and background tasks.
struct AppState {
    config: AppConfig,
    pending_stream_title: Option<String>,
    pending_stream_server: Option<String>,
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
            pending_stream_title: None,
            pending_stream_server: None,
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

    // Handle SIGUSR1 (sent by tray quit) — exit cleanly instead of crashing
    #[cfg(unix)]
    {
        use std::sync::atomic::{AtomicBool, Ordering};
        static QUIT_FLAG: AtomicBool = AtomicBool::new(false);
        unsafe {
            libc::signal(libc::SIGUSR1, handle_sigusr1 as libc::sighandler_t);
        }
        extern "C" fn handle_sigusr1(_: libc::c_int) {
            QUIT_FLAG.store(true, std::sync::atomic::Ordering::Relaxed);
        }
        // Check flag periodically — we'll poll it in the eframe update loop
        std::thread::spawn(move || {
            loop {
                std::thread::sleep(std::time::Duration::from_millis(500));
                if QUIT_FLAG.load(Ordering::Relaxed) {
                    tracing::info!("SIGUSR1 received — exiting");
                    std::process::exit(0);
                }
            }
        });
    }

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("Failed to build tokio runtime")?;

    // Load config
    let config = rt.block_on(AppConfig::load()).unwrap_or_default();
    let state = Arc::new(Mutex::new(AppState {
        is_linked: !config.links.is_empty() || config.link.is_some(),
        config: config.clone(),
        status_msg: if config.link.is_some() {
            String::new()
        } else {
            String::new()
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

    // Write PID file so tray knows if we're running
    let pid_path = std::env::var("HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_default()
        .join(".config/meshcast/.app-pid");
    let _ = std::fs::write(&pid_path, std::process::id().to_string());

    // Write initial tray state
    write_tray_state(false, false, "720p", 30, 0);

    // Spawn tray icon as a Python subprocess with dynamic state reading
    #[cfg(target_os = "linux")]
    let app_exe = std::env::current_exe().unwrap_or_default();
    #[cfg(target_os = "linux")]
    let tray_pid = std::process::Command::new("python3")
        .args(["-c", r#"
import gi, os, signal, json
gi.require_version("Gtk", "3.0")
gi.require_version("AppIndicator3", "0.1")
from gi.repository import Gtk, AppIndicator3, GLib

ppid = os.getppid()
state_path = os.path.expanduser("~/.config/meshcast/.tray-state")
last_state = {}

ind = AppIndicator3.Indicator.new("meshcast", "dialog-information", AppIndicator3.IndicatorCategory.APPLICATION_STATUS)
ind.set_status(AppIndicator3.IndicatorStatus.ACTIVE)

cmd_path = os.path.expanduser("~/.config/meshcast/.tray-cmd")

def write_cmd(cmd):
    with open(cmd_path, "w") as f: f.write(cmd)

import subprocess, sys
app_path = sys.argv[1] if len(sys.argv) > 1 else None
pid_path = os.path.expanduser("~/.config/meshcast/.app-pid")
app_proc = None

def is_app_running():
    try:
        with open(pid_path) as f:
            pid = int(f.read().strip())
        os.kill(pid, 0)
        return True
    except: return False

def show_app(_):
    global app_proc
    if not app_path: return
    if is_app_running(): return  # already open
    app_proc = subprocess.Popen([app_path], start_new_session=True)

menu = Gtk.Menu()
show_item = Gtk.MenuItem(label="Show Meshcast")
show_item.connect("activate", show_app)
stop_item = Gtk.MenuItem(label="Stop Stream")
stop_item.connect("activate", lambda _: write_cmd("stop"))
stop_item.set_sensitive(False)
sep = Gtk.SeparatorMenuItem()
quit_item = Gtk.MenuItem(label="Quit Meshcast")
quit_item.connect("activate", lambda _: (os.kill(ppid, signal.SIGUSR1), Gtk.main_quit()))
menu.append(show_item)
menu.append(stop_item)
menu.append(sep)
menu.append(quit_item)
menu.show_all()
ind.set_menu(menu)

def update_state():
    global last_state
    # Check parent alive
    try: os.kill(ppid, 0)
    except: Gtk.main_quit(); return False
    # Read state
    try:
        with open(state_path) as f:
            state = json.load(f)
    except: return True
    if state == last_state: return True
    last_state = state
    streaming = state.get("streaming", False)
    connected = state.get("connected", False)
    quality = state.get("quality", "")
    fps = state.get("fps", 30)
    viewers = state.get("viewers", 0)
    # Update menu
    stop_item.set_sensitive(streaming)
    # Update tooltip
    if streaming:
        ind.set_icon_full("media-record", "Meshcast")
        tip = f"LIVE: {quality} {fps}fps — {viewers} viewer{'s' if viewers != 1 else ''}"
    elif connected:
        ind.set_icon_full("network-idle", "Meshcast")
        tip = "Meshcast — Connected"
    else:
        ind.set_icon_full("dialog-information", "Meshcast")
        tip = "Meshcast — Offline"
    ind.set_title(tip)
    return True

GLib.timeout_add_seconds(2, update_state)
update_state()
Gtk.main()
"#, app_exe.to_str().unwrap_or("")])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .ok()
        .map(|c| c.id());
    #[cfg(not(target_os = "linux"))]
    let tray_pid: Option<u32> = None;

    eframe::run_native(
        "Meshcast",
        native_options,
        Box::new(move |cc| {
            // Dark theme with blurple accent
            let mut visuals = egui::Visuals::dark();
            visuals.widgets.noninteractive.bg_fill = egui::Color32::from_rgb(30, 31, 34); // Discord dark
            visuals.widgets.inactive.bg_fill = egui::Color32::from_rgb(43, 45, 49);
            visuals.widgets.hovered.bg_fill = egui::Color32::from_rgb(53, 55, 60);
            visuals.widgets.active.bg_fill = egui::Color32::from_rgb(88, 101, 242); // Blurple
            visuals.window_fill = egui::Color32::from_rgb(30, 31, 34);
            visuals.panel_fill = egui::Color32::from_rgb(30, 31, 34);
            visuals.widgets.noninteractive.corner_radius = egui::CornerRadius::same(6);
            visuals.widgets.inactive.corner_radius = egui::CornerRadius::same(6);
            visuals.widgets.hovered.corner_radius = egui::CornerRadius::same(6);
            visuals.widgets.active.corner_radius = egui::CornerRadius::same(6);
            cc.egui_ctx.set_visuals(visuals);

            // Create tray icon
            // Tray icon on Linux handled by Python subprocess above
            #[cfg(not(target_os = "linux"))]
            let tray = create_tray_icon();
            #[cfg(target_os = "linux")]
            let tray: Option<tray_icon::TrayIcon> = None;

            Ok(Box::new(MeshcastApp {
                state,
                ui_rx,
                cmd_tx,
                visible: true,
                quit: false,
                frame_count: 0,
                _tray: tray,
            }))
        }),
    )
    .map_err(|e| anyhow::anyhow!("eframe error: {e}"))?;

    // Clean up PID file and tray subprocess on exit
    let _ = std::fs::remove_file(&pid_path);
    if let Some(pid) = tray_pid {
        let _ = std::process::Command::new("kill").arg(pid.to_string()).spawn();
    }

    Ok(())
}

struct MeshcastApp {
    state: Arc<Mutex<AppState>>,
    ui_rx: mpsc::UnboundedReceiver<UiEvent>,
    cmd_tx: mpsc::UnboundedSender<DaemonCmd>,
    visible: bool,
    quit: bool,
    frame_count: u32,
    _tray: Option<tray_icon::TrayIcon>,
}

impl eframe::App for MeshcastApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.frame_count = self.frame_count.saturating_add(1);

        // Handle quit flag from UI button or tray menu
        if self.quit && self.frame_count > 10 {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            return;
        }

        // Consume any stale close events from previous instance
        if self.frame_count <= 10 {
            if ctx.input(|i| i.viewport().close_requested()) {
                ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            }
            // Don't process quit button clicks in early frames
            self.quit = false;
        }

        // Handle window close → minimize instead of quitting
        if self.frame_count > 10 && ctx.input(|i| i.viewport().close_requested()) && !self.quit {
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

        // Poll tray command file (Linux Python tray subprocess)
        {
            let cmd_path = std::env::var("HOME")
                .map(std::path::PathBuf::from)
                .unwrap_or_default()
                .join(".config/meshcast/.tray-cmd");
            if let Ok(cmd) = std::fs::read_to_string(&cmd_path) {
                let _ = std::fs::remove_file(&cmd_path);
                match cmd.trim() {
                    "show" => {
                        // On Wayland, focus/minimize commands are often ignored.
                        // Toggle visibility to force window re-map.
                        ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
                        ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
                        ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
                        ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
                    }
                    "stop" => {
                        let _ = self.cmd_tx.send(DaemonCmd::StopStream);
                    }
                    _ => {}
                }
            }
        }

        // Drain UI events from background task
        while let Ok(event) = self.ui_rx.try_recv() {
            let mut s = self.state.lock().expect("poisoned");
            match event {
                UiEvent::Connected => {
                    s.is_connected = true;
                    s.status_msg = "Connected to bot.".into();
                    write_tray_state(s.is_streaming, true, &s.config.video.quality, s.config.video.fps, s.viewer_count);
                }
                UiEvent::Disconnected => {
                    s.is_connected = false;
                    s.status_msg = "Disconnected from bot.".into();
                    write_tray_state(s.is_streaming, false, &s.config.video.quality, s.config.video.fps, s.viewer_count);
                }
                UiEvent::StreamRequested { title, server } => {
                    let notify_server = server.clone();
                    let notify_title = title.clone();
                    s.pending_stream_title = Some(title);
                    s.pending_stream_server = Some(server);
                    s.status_msg = "Stream requested — approve below.".into();
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
                    drop(s);
                    self.visible = true;
                    ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
                    ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
                    continue;
                }
                UiEvent::StreamStarted { ticket } => {
                    s.is_streaming = true;
                    s.stream_ticket = Some(ticket);
                    s.status_msg = String::new();
                    write_tray_state(true, s.is_connected, &s.config.video.quality, s.config.video.fps, s.viewer_count);
                }
                UiEvent::StreamFailed { error } => {
                    s.status_msg = format!("Stream failed: {error}");
                    write_tray_state(false, s.is_connected, &s.config.video.quality, s.config.video.fps, s.viewer_count);
                }
                UiEvent::WatchRequested => {
                    s.status_msg = "Opening viewer...".into();
                }
                UiEvent::ViewerCount(count) => {
                    s.viewer_count = count;
                    write_tray_state(s.is_streaming, s.is_connected, &s.config.video.quality, s.config.video.fps, count);
                }
                UiEvent::Linked { config } => {
                    s.config = config;
                    s.is_linked = !s.config.links.is_empty() || s.config.link.is_some();
                    s.status_msg = String::new();
                }
            }
        }

        // Request periodic repaint for status updates
        ctx.request_repaint_after(std::time::Duration::from_millis(250));

        let blurple = egui::Color32::from_rgb(88, 101, 242);
        let panel_frame = egui::Frame::new()
            .fill(egui::Color32::from_rgb(30, 31, 34))
            .inner_margin(egui::Margin::same(16));

        egui::CentralPanel::default().frame(panel_frame).show(ctx, |ui| {
            let s = self.state.lock().expect("poisoned");

            // Header bar
            ui.horizontal(|ui| {
                ui.heading(egui::RichText::new("Meshcast").color(egui::Color32::WHITE).strong());
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.add(egui::Button::new(
                        egui::RichText::new("Quit").color(egui::Color32::from_rgb(200, 200, 200))
                    ).fill(egui::Color32::from_rgb(55, 57, 63))).clicked() {
                        self.quit = true;
                    }
                });
            });
            ui.add_space(4.0);

            // Status pill
            let (color, label) = if s.is_streaming {
                (egui::Color32::from_rgb(237, 66, 69), "LIVE")
            } else if s.is_connected {
                (egui::Color32::from_rgb(87, 242, 135), "Connected")
            } else if s.is_linked {
                (egui::Color32::from_rgb(254, 231, 92), "Offline")
            } else {
                (egui::Color32::from_rgb(128, 132, 142), "Not linked")
            };
            ui.horizontal(|ui| {
                ui.colored_label(color, egui::RichText::new(format!("● {label}")).strong());
                ui.label(
                    egui::RichText::new(&s.status_msg)
                        .color(egui::Color32::from_rgb(148, 155, 164)),
                );
            });

            ui.add_space(12.0);
            ui.separator();
            ui.add_space(8.0);

            // Server list
            {
                let server_names: Vec<String> = s.config.links.iter()
                    .map(|l| l.name.clone())
                    .collect();

                if !server_names.is_empty() {
                    ui.label(egui::RichText::new("Servers").color(egui::Color32::from_rgb(185, 187, 190)).small());
                    let mut to_remove = None;
                    for name in &server_names {
                        ui.horizontal(|ui| {
                            ui.colored_label(
                                egui::Color32::from_rgb(87, 242, 135),
                                egui::RichText::new(format!("● {name}")),
                            );
                            if ui.small_button("Unlink").clicked() {
                                to_remove = Some(name.clone());
                            }
                        });
                    }
                    if let Some(name) = to_remove {
                        drop(s);
                        let mut s = self.state.lock().expect("poisoned");
                        s.config.remove_link(&name);
                        s.is_linked = !s.config.links.is_empty();
                        let config = s.config.clone();
                        drop(s);
                        if let Ok(rt) = tokio::runtime::Handle::try_current() {
                            rt.spawn(async move { let _ = config.save().await; });
                        }
                        let s = self.state.lock().expect("poisoned");
                        // continue with updated state
                        let _ = &s;
                        drop(s);
                    } else {
                        drop(s);
                    }
                    ui.add_space(4.0);
                } else {
                    ui.label(
                        egui::RichText::new("Type /link in a Discord server to get a pairing code.")
                            .color(egui::Color32::from_rgb(148, 155, 164)),
                    );
                    drop(s);
                }

                // Always show pairing input
                let mut s = self.state.lock().expect("poisoned");
                ui.horizontal(|ui| {
                    let response = ui.add(
                        egui::TextEdit::singleline(&mut s.link_token_input)
                            .hint_text("Paste pairing code...")
                            .desired_width(ui.available_width() - 70.0),
                    );
                    let link_clicked = ui.add(
                        egui::Button::new(egui::RichText::new("Link").color(egui::Color32::WHITE))
                            .fill(blurple),
                    ).clicked();
                    if (response.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)))
                        || link_clicked
                    {
                        let token = s.link_token_input.clone();
                        if !token.is_empty() {
                            let _ = self.cmd_tx.send(DaemonCmd::Link { token });
                            s.link_token_input.clear();
                            s.status_msg = "Connecting...".into();
                        }
                    }
                });
                drop(s);
            }

            ui.add_space(8.0);
            ui.separator();
            ui.add_space(8.0);

            // Stream section (only when linked)
            if self.state.lock().expect("poisoned").is_linked {
                // Consent dialog for incoming stream request
                let s = self.state.lock().expect("poisoned");
                if let Some(title) = s.pending_stream_title.clone() {
                    let server = s.pending_stream_server.clone().unwrap_or_default();
                    ui.group(|ui| {
                        ui.label(
                            egui::RichText::new("Stream Request")
                                .color(egui::Color32::from_rgb(254, 231, 92))
                                .heading(),
                        );
                        if !server.is_empty() {
                            ui.label(egui::RichText::new(&server).color(egui::Color32::WHITE).strong());
                        }
                        ui.label(format!("\"{title}\""));
                        ui.add_space(8.0);
                        ui.horizontal(|ui| {
                            drop(s);
                            let approve = ui.add_sized(
                                [120.0, 32.0],
                                egui::Button::new(
                                    egui::RichText::new("Share Screen").color(egui::Color32::WHITE),
                                )
                                .fill(blurple),
                            ).clicked();
                            let reject = ui.add_sized(
                                [120.0, 32.0],
                                egui::Button::new("Decline")
                                    .fill(egui::Color32::from_rgb(55, 57, 63)),
                            ).clicked();

                            if approve {
                                let _ = self.cmd_tx.send(DaemonCmd::ApproveStream);
                                let mut st = self.state.lock().expect("poisoned");
                                st.pending_stream_title = None;
                                st.pending_stream_server = None;
                            }
                            if reject {
                                let _ = self.cmd_tx.send(DaemonCmd::RejectStream);
                                let mut st = self.state.lock().expect("poisoned");
                                st.pending_stream_title = None;
                                st.pending_stream_server = None;
                                st.status_msg = "Stream declined.".into();
                            }
                        });
                    });
                    return; // don't show settings while consent is pending
                }

                // Stream status
                if s.is_streaming {
                    let vc = s.viewer_count;
                    let quality = s.config.video.quality.clone();
                    let fps = s.config.video.fps;
                    ui.label(
                        egui::RichText::new(format!(
                            "{quality} {fps}fps — {vc} viewer{}",
                            if vc == 1 { "" } else { "s" }
                        ))
                        .color(egui::Color32::from_rgb(185, 187, 190)),
                    );
                    ui.add_space(8.0);
                    drop(s);
                    if ui.add_sized(
                        [ui.available_width(), 32.0],
                        egui::Button::new(egui::RichText::new("Stop Stream").color(egui::Color32::WHITE))
                            .fill(egui::Color32::from_rgb(237, 66, 69)),
                    ).clicked() {
                        let _ = self.cmd_tx.send(DaemonCmd::StopStream);
                    }
                } else {
                    drop(s);
                    ui.label(
                        egui::RichText::new("Ready. Use /stream in Discord to start streaming.")
                            .color(egui::Color32::from_rgb(148, 155, 164)),
                    );
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
    let mut pending_stream: Option<String> = None; // title waiting for consent
    let mut active_viewers: u32 = 0;
    const MAX_VIEWERS: u32 = 5;
    let expected_peer = link_state.as_ref().map(|ls| ls.peer_endpoint_id());

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
                        // Verify sender identity
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
                                // Store config from Discord, ask for user consent
                                {
                                    let mut s = state.lock().expect("poisoned");
                                    s.config.video.quality = quality;
                                    s.config.video.fps = fps;
                                }
                                pending_stream = Some(title.clone());
                                let _ = ui_tx.send(UiEvent::StreamRequested { title, server });
                            }
                            Ok(Signal::StopStream) => {
                                tracing::info!("Stop stream");
                                if let Some((l, _bc)) = live.take() {
                                    l.shutdown().await;
                                    if let Some(ref s) = sender {
                                        let _ = s.broadcast_neighbors(Signal::StreamStopped.encode()?).await;
                                    }
                                    {
                                        let mut s = state.lock().expect("poisoned");
                                        s.is_streaming = false;
                                        s.status_msg = "Stream stopped.".into();
                                    }
                                }
                            }
                            Ok(Signal::WatchStream { ticket }) => {
                                if active_viewers >= MAX_VIEWERS {
                                    tracing::warn!("Viewer limit reached ({MAX_VIEWERS}), ignoring WatchStream");
                                } else {
                                    tracing::info!("Watch: {ticket}");
                                    let _ = ui_tx.send(UiEvent::WatchRequested);
                                    let exe = std::env::current_exe().unwrap_or_else(|_| "meshcast".into());
                                    let meshcast_cli = exe.parent()
                                        .map(|p| p.join("meshcast"))
                                        .unwrap_or_else(|| "meshcast".into());
                                    match meshcast_signal::launch_viewer(&meshcast_cli, &ticket) {
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
                    Some(DaemonCmd::ApproveStream) => {
                        if let Some(_title) = pending_stream.take() {
                            let cfg = state.lock().expect("poisoned").config.clone();
                            match start_stream("meshcast".into(), &cfg.video.quality, cfg.video.fps).await {
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
                    }
                    Some(DaemonCmd::RejectStream) => {
                        pending_stream = None;
                        tracing::info!("Stream request declined by user");
                    }
                    Some(DaemonCmd::StopStream) => {
                        if let Some((l, _bc)) = live.take() {
                            l.shutdown().await;
                            if let Some(ref s) = sender {
                                let _ = s.broadcast_neighbors(Signal::StreamStopped.encode()?).await;
                            }
                            {
                                let mut st = state.lock().expect("poisoned");
                                st.is_streaming = false;
                                st.status_msg = "Stream stopped.".into();
                                write_tray_state(false, st.is_connected, &st.config.video.quality, st.config.video.fps, 0);
                            }
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
    // Try PairCode first, fall back to legacy token
    let (bot_id, pin) = match PairCode::parse(input) {
        Ok((bot_id, pin)) => (bot_id, pin),
        Err(_) => {
            return do_link_legacy(node, input, config, ui_tx).await;
        }
    };

    tracing::info!("Pairing with bot {} using PIN", bot_id.fmt_short());

    let pairing_topic = meshcast_signal::derive_pairing_topic(&pin);
    let pairing_sub = node
        .gossip
        .subscribe_and_join(pairing_topic, vec![bot_id])
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    let (pair_sender, mut pair_receiver) = pairing_sub.split();

    pair_sender
        .broadcast_neighbors(meshcast_signal::PairSignal::PairRequest { pin }.encode()?)
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;

    let real_topic = tokio::time::timeout(std::time::Duration::from_secs(15), async {
        while let Some(event) = pair_receiver.next().await {
            if let Ok(meshcast_signal::Event::Received(msg)) = event {
                match meshcast_signal::PairSignal::decode(&msg.content) {
                    Ok(meshcast_signal::PairSignal::PairAccepted { topic, server_name }) => {
                        return Ok((meshcast_signal::TopicId::from_bytes(topic), server_name));
                    }
                    Ok(meshcast_signal::PairSignal::PairRejected { reason }) => {
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

    let link_state = meshcast_signal::LinkState::new(
        real_topic,
        &node.endpoint.secret_key(),
        bot_id,
    );
    config.add_link(server_name, LinkConfig::from(link_state));
    config.save().await?;

    let _ = ui_tx.send(UiEvent::Linked { config: config.clone() });
    Ok(())
}

async fn do_link_legacy(
    node: &SignalNode,
    token: &str,
    config: &mut AppConfig,
    ui_tx: &mpsc::UnboundedSender<UiEvent>,
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

    let link_state = meshcast_signal::LinkState::new(
        pair.topic,
        &node.endpoint.secret_key(),
        peer_ids[0],
    );
    config.add_link(format!("Server {}", peer_ids[0].fmt_short()), LinkConfig::from(link_state));
    config.save().await?;

    let _ = ui_tx.send(UiEvent::Linked { config: config.clone() });
    Ok(())
}

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
        // Standard 30fps: use the simple preset API
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
