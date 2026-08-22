//! `meshcast-app` — the desktop window.
//!
//! A thin egui UI over the daemon: it reads the daemon's state file and sends
//! commands back through the command file (see `meshcast_signal::ipc`). It
//! never touches the network itself, so closing the window never interrupts a
//! stream. If no daemon is running it starts one.
//!
//! On macOS/Windows this process also owns the tray icon. On Linux the tray is
//! `scripts/meshcast-tray.py` (GTK and winit can't share the main thread).

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::Result;
use eframe::egui;
use meshcast_signal::ipc::{self, Command};
use meshcast_signal::process;
use meshcast_signal::{AppConfig, DaemonState};

/// Set by the SIGUSR1 handler (tray "Quit") or the tray menu; the UI loop exits.
static QUIT_REQUESTED: AtomicBool = AtomicBool::new(false);

const BG: egui::Color32 = egui::Color32::from_rgb(30, 31, 34);
const BLURPLE: egui::Color32 = egui::Color32::from_rgb(88, 101, 242);
const GREEN: egui::Color32 = egui::Color32::from_rgb(87, 242, 135);
const YELLOW: egui::Color32 = egui::Color32::from_rgb(254, 231, 92);
const RED: egui::Color32 = egui::Color32::from_rgb(237, 66, 69);
const MUTED: egui::Color32 = egui::Color32::from_rgb(148, 155, 164);
const LIGHT: egui::Color32 = egui::Color32::from_rgb(185, 187, 190);

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "meshcast_app=info".into()),
        )
        .init();

    #[cfg(unix)]
    install_sigusr1_handler();

    // Single instance: if a window is already open there's nothing to do.
    let pid_path = AppConfig::app_pid_path();
    if process::pid_file_alive(&pid_path) {
        tracing::info!("Meshcast window is already open");
        return Ok(());
    }
    process::write_pid_file(&pid_path)?;

    ensure_daemon();

    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Meshcast")
            .with_inner_size([420.0, 380.0])
            .with_min_inner_size([360.0, 300.0]),
        ..Default::default()
    };

    #[cfg(not(target_os = "linux"))]
    let tray = tray::create();

    let result = eframe::run_native(
        "Meshcast",
        native_options,
        Box::new(move |cc| {
            apply_theme(&cc.egui_ctx);
            Ok(Box::new(MeshcastApp {
                config: AppConfig::load_sync().unwrap_or_default(),
                daemon: ipc::read_state(),
                link_input: String::new(),
                status_msg: String::new(),
                status_msg_at: None,
                quitting: false,
                #[cfg(not(target_os = "linux"))]
                tray: tray::State::new(tray),
            }))
        }),
    );

    process::remove_own_pid_file(&pid_path);
    result.map_err(|e| anyhow::anyhow!("eframe error: {e}"))
}

/// Start `meshcast daemon` if it isn't already running.
fn ensure_daemon() {
    if process::pid_file_alive(&AppConfig::daemon_pid_path()) {
        return;
    }
    let bin = process::find_meshcast_bin();
    match process::launch_daemon(&bin) {
        Ok(_) => tracing::info!("Started daemon ({})", bin.display()),
        Err(e) => tracing::error!("Couldn't start daemon: {e:#}"),
    }
}

#[cfg(unix)]
fn install_sigusr1_handler() {
    extern "C" fn handle(_: libc::c_int) {
        QUIT_REQUESTED.store(true, Ordering::Relaxed);
    }
    // SAFETY: installing an async-signal-safe handler that only stores an atomic.
    unsafe {
        libc::signal(libc::SIGUSR1, handle as *const () as libc::sighandler_t);
    }
}

fn apply_theme(ctx: &egui::Context) {
    let mut visuals = egui::Visuals::dark();
    visuals.widgets.noninteractive.bg_fill = BG;
    visuals.widgets.inactive.bg_fill = egui::Color32::from_rgb(43, 45, 49);
    visuals.widgets.hovered.bg_fill = egui::Color32::from_rgb(53, 55, 60);
    visuals.widgets.active.bg_fill = BLURPLE;
    visuals.window_fill = BG;
    visuals.panel_fill = BG;
    for w in [
        &mut visuals.widgets.noninteractive,
        &mut visuals.widgets.inactive,
        &mut visuals.widgets.hovered,
        &mut visuals.widgets.active,
    ] {
        w.corner_radius = egui::CornerRadius::same(6);
    }
    ctx.set_visuals(visuals);
}

struct MeshcastApp {
    config: AppConfig,
    daemon: DaemonState,
    link_input: String,
    status_msg: String,
    status_msg_at: Option<Instant>,
    quitting: bool,
    #[cfg(not(target_os = "linux"))]
    tray: tray::State,
}

impl MeshcastApp {
    fn set_status(&mut self, msg: impl Into<String>) {
        self.status_msg = msg.into();
        self.status_msg_at = Some(Instant::now());
    }

    fn send(&mut self, cmd: Command) {
        if let Err(e) = ipc::send_command(&cmd) {
            self.set_status(format!("Couldn't talk to the daemon: {e}"));
        }
    }
}

impl eframe::App for MeshcastApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        if QUIT_REQUESTED.load(Ordering::Relaxed) && !self.quitting {
            self.quitting = true;
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            return;
        }

        #[cfg(not(target_os = "linux"))]
        self.tray.poll(ctx, &self.daemon);

        // Poll daemon state
        self.daemon = ipc::read_state();
        let daemon_alive = process::pid_file_alive(&AppConfig::daemon_pid_path());

        // Reload config if the daemon's view of links differs from ours
        let our_links: Vec<String> = self.config.links.iter().map(|l| l.name.clone()).collect();
        if daemon_alive && self.daemon.linked_servers != our_links {
            self.config = AppConfig::load_sync().unwrap_or_default();
        }

        if let Some(err) = self.daemon.error.clone() {
            if self.status_msg != err {
                self.set_status(err);
            }
        } else if self
            .status_msg_at
            .is_some_and(|t| t.elapsed() > Duration::from_secs(8))
        {
            self.status_msg.clear();
            self.status_msg_at = None;
        }

        ctx.request_repaint_after(Duration::from_millis(250));

        let panel_frame = egui::Frame::new()
            .fill(BG)
            .inner_margin(egui::Margin::same(16));
        let is_linked = self.config.is_linked();
        let daemon = self.daemon.clone();

        egui::CentralPanel::default()
            .frame(panel_frame)
            .show(ctx, |ui| {
                ui.heading(
                    egui::RichText::new("Meshcast")
                        .color(egui::Color32::WHITE)
                        .strong(),
                );
                ui.add_space(4.0);

                // Status pill
                let (color, label) = if !daemon_alive {
                    (RED, "Daemon not running")
                } else if daemon.streaming {
                    (RED, "LIVE")
                } else if daemon.connected {
                    (GREEN, "Connected")
                } else if is_linked {
                    (YELLOW, "Waiting for bot…")
                } else {
                    (MUTED, "Not linked")
                };
                ui.horizontal_wrapped(|ui| {
                    ui.colored_label(color, egui::RichText::new(format!("● {label}")).strong());
                    if !self.status_msg.is_empty() {
                        ui.label(egui::RichText::new(&self.status_msg).color(MUTED));
                    }
                });

                if !daemon_alive {
                    ui.add_space(8.0);
                    if ui.button("Start daemon").clicked() {
                        ensure_daemon();
                        self.set_status("Starting daemon…");
                    }
                }

                ui.add_space(12.0);
                ui.separator();
                ui.add_space(8.0);

                // Linked servers
                let server_names: Vec<String> =
                    self.config.links.iter().map(|l| l.name.clone()).collect();
                if server_names.is_empty() {
                    ui.label(
                        egui::RichText::new(
                            "Type /link in a Discord server that has the Meshcast bot, then paste the code below.",
                        )
                        .color(MUTED),
                    );
                } else {
                    ui.label(egui::RichText::new("Linked servers").color(LIGHT).small());
                    let mut to_remove = None;
                    for name in &server_names {
                        ui.horizontal(|ui| {
                            ui.colored_label(GREEN, egui::RichText::new(format!("● {name}")));
                            if ui.small_button("Unlink").clicked() {
                                to_remove = Some(name.clone());
                            }
                        });
                    }
                    if let Some(name) = to_remove {
                        self.config.remove_link(&name);
                        if let Err(e) = self.config.save_sync() {
                            self.set_status(format!("Couldn't save config: {e}"));
                        } else {
                            self.send(Command::Reload);
                            self.set_status(format!("Unlinked from {name}"));
                        }
                    }
                    ui.add_space(4.0);
                }

                // Pairing input
                ui.horizontal(|ui| {
                    let response = ui.add(
                        egui::TextEdit::singleline(&mut self.link_input)
                            .hint_text("Paste pairing code…")
                            .desired_width(ui.available_width() - 70.0),
                    );
                    let link_clicked = ui
                        .add(
                            egui::Button::new(
                                egui::RichText::new("Link").color(egui::Color32::WHITE),
                            )
                            .fill(BLURPLE),
                        )
                        .clicked();
                    let enter = response.lost_focus()
                        && ui.input(|i| i.key_pressed(egui::Key::Enter));
                    if enter || link_clicked {
                        let code = self.link_input.trim().to_string();
                        if !code.is_empty() {
                            self.send(Command::Link(code));
                            self.link_input.clear();
                            self.set_status("Connecting to bot…");
                        }
                    }
                });

                ui.add_space(8.0);
                ui.separator();
                ui.add_space(8.0);

                if !is_linked {
                    return;
                }

                // Consent dialog for an incoming stream request
                if let Some(req) = &daemon.pending_request {
                    ui.group(|ui| {
                        ui.label(egui::RichText::new("Stream request").color(YELLOW).heading());
                        if !req.server.is_empty() {
                            ui.label(
                                egui::RichText::new(&req.server)
                                    .color(egui::Color32::WHITE)
                                    .strong(),
                            );
                        }
                        ui.label(format!("\"{}\" — {} {}fps", req.title, req.quality, req.fps));
                        ui.add_space(8.0);
                        ui.horizontal(|ui| {
                            let approve = ui
                                .add_sized(
                                    [130.0, 32.0],
                                    egui::Button::new(
                                        egui::RichText::new("Share Screen")
                                            .color(egui::Color32::WHITE),
                                    )
                                    .fill(BLURPLE),
                                )
                                .clicked();
                            let reject = ui
                                .add_sized(
                                    [110.0, 32.0],
                                    egui::Button::new("Decline")
                                        .fill(egui::Color32::from_rgb(55, 57, 63)),
                                )
                                .clicked();
                            if approve {
                                self.send(Command::Approve);
                                self.set_status("Starting capture…");
                            }
                            if reject {
                                self.send(Command::Reject);
                                self.set_status("Stream declined.");
                            }
                        });
                    });
                    return;
                }

                if daemon.streaming {
                    ui.label(
                        egui::RichText::new(format!(
                            "{} {}fps \u{2014} {} viewer{}",
                            daemon.quality,
                            daemon.fps,
                            daemon.viewers,
                            if daemon.viewers == 1 { "" } else { "s" }
                        ))
                        .color(LIGHT),
                    );
                    ui.add_space(8.0);
                    if ui
                        .add_sized(
                            [ui.available_width(), 32.0],
                            egui::Button::new(
                                egui::RichText::new("Stop Stream").color(egui::Color32::WHITE),
                            )
                            .fill(RED),
                        )
                        .clicked()
                    {
                        self.send(Command::Stop);
                    }
                } else {
                    ui.label(
                        egui::RichText::new("Ready. Use /stream in Discord to start streaming.")
                            .color(MUTED),
                    );
                }

                // Settings
                ui.add_space(8.0);
                ui.separator();
                ui.add_space(8.0);
                ui.label(egui::RichText::new("Default stream settings").color(LIGHT).small());
                let mut changed = false;
                ui.horizontal(|ui| {
                    ui.label("Audio (microphone)");
                    changed |= ui.checkbox(&mut self.config.audio.enabled, "").changed();
                });
                if changed {
                    match self.config.save_sync() {
                        Ok(()) => self.send(Command::Reload),
                        Err(e) => self.set_status(format!("Couldn't save config: {e}")),
                    }
                }
            });
    }
}

// ---------------------------------------------------------------------------
// Tray icon (macOS / Windows only — on Linux see scripts/meshcast-tray.py)
// ---------------------------------------------------------------------------

#[cfg(not(target_os = "linux"))]
mod tray {
    use super::*;
    use tray_icon::menu::{Menu, MenuEvent, MenuItemBuilder, PredefinedMenuItem};
    use tray_icon::{Icon, TrayIcon, TrayIconBuilder, TrayIconEvent};

    pub fn create() -> Option<TrayIcon> {
        let menu = Menu::new();
        let show_item = MenuItemBuilder::new()
            .text("Show Meshcast")
            .id(tray_icon::menu::MenuId("show".into()))
            .build();
        let stop_item = MenuItemBuilder::new()
            .text("Stop Stream")
            .id(tray_icon::menu::MenuId("stop".into()))
            .build();
        let quit_item = MenuItemBuilder::new()
            .text("Quit Meshcast")
            .id(tray_icon::menu::MenuId("quit".into()))
            .build();
        menu.append(&show_item).ok()?;
        menu.append(&stop_item).ok()?;
        menu.append(&PredefinedMenuItem::separator()).ok()?;
        menu.append(&quit_item).ok()?;

        let size = 16u32;
        let mut rgba = vec![0u8; (size * size * 4) as usize];
        for pixel in rgba.chunks_exact_mut(4) {
            pixel.copy_from_slice(&[0x58, 0x65, 0xF2, 0xFF]);
        }
        let icon = Icon::from_rgba(rgba, size, size).ok()?;

        TrayIconBuilder::new()
            .with_menu(Box::new(menu))
            .with_tooltip("Meshcast")
            .with_icon(icon)
            .build()
            .ok()
    }

    pub struct State {
        _tray: Option<TrayIcon>,
        hidden: bool,
        had_pending: bool,
    }

    impl State {
        pub fn new(tray: Option<TrayIcon>) -> Self {
            Self {
                _tray: tray,
                hidden: false,
                had_pending: false,
            }
        }

        fn show(&mut self, ctx: &egui::Context) {
            ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
            ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
            self.hidden = false;
        }

        pub fn poll(&mut self, ctx: &egui::Context, daemon: &DaemonState) {
            // Hide-on-close: the daemon keeps running in the background.
            if ctx.input(|i| i.viewport().close_requested())
                && !QUIT_REQUESTED.load(Ordering::Relaxed)
            {
                ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
                ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(true));
                self.hidden = true;
            }

            while let Ok(event) = MenuEvent::receiver().try_recv() {
                match event.id().0.as_str() {
                    "show" => self.show(ctx),
                    "stop" => {
                        let _ = ipc::send_command(&Command::Stop);
                    }
                    "quit" => {
                        let _ = ipc::send_command(&Command::Stop);
                        if let Some(pid) = process::read_pid_file(&AppConfig::daemon_pid_path()) {
                            process::terminate(pid);
                        }
                        QUIT_REQUESTED.store(true, Ordering::Relaxed);
                    }
                    _ => {}
                }
            }
            while let Ok(event) = TrayIconEvent::receiver().try_recv() {
                if let TrayIconEvent::Click { .. } = event {
                    self.show(ctx);
                }
            }

            // Pop the window up when a stream request arrives.
            if daemon.pending_request.is_some() {
                if !self.had_pending {
                    self.had_pending = true;
                    if self.hidden {
                        self.show(ctx);
                    }
                }
            } else {
                self.had_pending = false;
            }
        }
    }
}
