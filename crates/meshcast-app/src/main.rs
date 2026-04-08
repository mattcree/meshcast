use anyhow::Result;
use eframe::egui;
use meshcast_signal::{AppConfig, DaemonState};

// --- Platform-specific: tray icon + daemon management (macOS/Windows) ---

#[cfg(not(target_os = "linux"))]
fn create_tray_icon() -> Option<tray_icon::TrayIcon> {
    use tray_icon::menu::{Menu, MenuItemBuilder};
    use tray_icon::TrayIconBuilder;

    let menu = Menu::new();
    let show_item = MenuItemBuilder::new()
        .text("Show Meshcast")
        .id(tray_icon::menu::MenuId("show".into()))
        .build();
    let quit_item = MenuItemBuilder::new()
        .text("Quit Meshcast")
        .id(tray_icon::menu::MenuId("quit".into()))
        .build();
    menu.append(&show_item).ok();
    menu.append(&quit_item).ok();

    let size = 16u32;
    let mut rgba = vec![0u8; (size * size * 4) as usize];
    for pixel in rgba.chunks_exact_mut(4) {
        pixel[0] = 0x58;
        pixel[1] = 0x65;
        pixel[2] = 0xF2;
        pixel[3] = 0xFF;
    }
    let icon = tray_icon::Icon::from_rgba(rgba, size, size).ok()?;

    TrayIconBuilder::new()
        .with_menu(Box::new(menu))
        .with_tooltip("Meshcast")
        .with_icon(icon)
        .build()
        .ok()
}

#[cfg(not(target_os = "linux"))]
fn is_daemon_running() -> bool {
    AppConfig::daemon_pid_path()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|s| s.trim().parse::<u32>().ok())
        .is_some()
    // On Windows/macOS we just check if the PID file exists;
    // a full liveness check would need platform-specific APIs
}

#[cfg(not(target_os = "linux"))]
fn start_daemon() {
    if is_daemon_running() {
        return;
    }
    let daemon_bin = std::env::var("MESHCAST_DAEMON")
        .map(std::path::PathBuf::from)
        .or_else(|_| {
            std::env::current_exe().map(|exe| {
                exe.parent()
                    .map(|p| p.join("meshcast"))
                    .unwrap_or_else(|| "meshcast".into())
            })
        })
        .unwrap_or_else(|_| "meshcast".into());

    let _ = std::process::Command::new(&daemon_bin)
        .arg("daemon")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}

#[cfg(not(target_os = "linux"))]
fn kill_daemon() {
    // On macOS/Windows, read PID and kill. Best-effort.
    if let Ok(pid_str) = std::fs::read_to_string(AppConfig::daemon_pid_path()) {
        if let Ok(_pid) = pid_str.trim().parse::<u32>() {
            // Platform-specific kill would go here; for now the daemon
            // will exit when it detects the PID file is removed or via stop command
            send_cmd("stop");
        }
    }
}

// --- Shared ---

/// Write a command to `.tray-cmd` for the daemon to read.
fn send_cmd(cmd: &str) {
    let _ = std::fs::write(AppConfig::cmd_path(), cmd);
}

/// Read daemon state from `.tray-state`.
fn read_state() -> DaemonState {
    match std::fs::read_to_string(AppConfig::state_path()) {
        Ok(data) => serde_json::from_str(&data).unwrap_or_default(),
        Err(_) => DaemonState::default(),
    }
}

struct AppState {
    config: AppConfig,
    daemon: DaemonState,
    link_token_input: String,
    status_msg: String,
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "meshcast_app=info".into()),
        )
        .init();

    // Linux: handle SIGUSR1 (sent by tray script quit)
    #[cfg(target_os = "linux")]
    {
        use std::sync::atomic::{AtomicBool, Ordering};
        static QUIT_FLAG: AtomicBool = AtomicBool::new(false);
        unsafe {
            libc::signal(libc::SIGUSR1, handle_sigusr1 as libc::sighandler_t);
        }
        extern "C" fn handle_sigusr1(_: libc::c_int) {
            QUIT_FLAG.store(true, std::sync::atomic::Ordering::Relaxed);
        }
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

    // macOS/Windows: start daemon if not already running
    #[cfg(not(target_os = "linux"))]
    start_daemon();

    let config = AppConfig::load_sync().unwrap_or_default();
    let daemon = read_state();

    let state = AppState {
        config,
        daemon,
        link_token_input: String::new(),
        status_msg: String::new(),
    };

    // Write PID file
    let pid_path = AppConfig::app_pid_path();
    if let Some(parent) = pid_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(&pid_path, std::process::id().to_string());

    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Meshcast")
            .with_inner_size([420.0, 360.0])
            .with_min_inner_size([360.0, 300.0]),
        ..Default::default()
    };

    // macOS/Windows: create tray icon
    #[cfg(not(target_os = "linux"))]
    let tray = create_tray_icon();

    eframe::run_native(
        "Meshcast",
        native_options,
        Box::new(move |cc| {
            // Dark theme with blurple accent
            let mut visuals = egui::Visuals::dark();
            visuals.widgets.noninteractive.bg_fill = egui::Color32::from_rgb(30, 31, 34);
            visuals.widgets.inactive.bg_fill = egui::Color32::from_rgb(43, 45, 49);
            visuals.widgets.hovered.bg_fill = egui::Color32::from_rgb(53, 55, 60);
            visuals.widgets.active.bg_fill = egui::Color32::from_rgb(88, 101, 242);
            visuals.window_fill = egui::Color32::from_rgb(30, 31, 34);
            visuals.panel_fill = egui::Color32::from_rgb(30, 31, 34);
            visuals.widgets.noninteractive.corner_radius = egui::CornerRadius::same(6);
            visuals.widgets.inactive.corner_radius = egui::CornerRadius::same(6);
            visuals.widgets.hovered.corner_radius = egui::CornerRadius::same(6);
            visuals.widgets.active.corner_radius = egui::CornerRadius::same(6);
            cc.egui_ctx.set_visuals(visuals);

            Ok(Box::new(MeshcastApp {
                state,
                #[cfg(not(target_os = "linux"))]
                quit: false,
                #[cfg(not(target_os = "linux"))]
                hidden: false,
                #[cfg(not(target_os = "linux"))]
                had_pending: false,
                #[cfg(not(target_os = "linux"))]
                _tray: tray,
            }))
        }),
    )
    .map_err(|e| anyhow::anyhow!("eframe error: {e}"))?;

    // Clean up
    let _ = std::fs::remove_file(&pid_path);

    Ok(())
}

struct MeshcastApp {
    state: AppState,
    #[cfg(not(target_os = "linux"))]
    quit: bool,
    #[cfg(not(target_os = "linux"))]
    hidden: bool,
    #[cfg(not(target_os = "linux"))]
    had_pending: bool,
    #[cfg(not(target_os = "linux"))]
    _tray: Option<tray_icon::TrayIcon>,
}

impl eframe::App for MeshcastApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // --- macOS/Windows: hide-on-close + tray events ---
        #[cfg(not(target_os = "linux"))]
        {
            // Hide-on-close: intercept window close, minimize to tray
            if ctx.input(|i| i.viewport().close_requested()) && !self.quit {
                ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
                ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(true));
                self.hidden = true;
            }

            // Quit: kill daemon and actually close
            if self.quit {
                kill_daemon();
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                return;
            }

            // Tray menu events
            if let Ok(event) = tray_icon::menu::MenuEvent::receiver().try_recv() {
                match event.id().0.as_str() {
                    "show" => {
                        ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
                        ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
                        self.hidden = false;
                    }
                    "quit" => {
                        self.quit = true;
                    }
                    _ => {}
                }
            }

            // Tray icon click
            if let Ok(tray_icon::TrayIconEvent::Click { .. }) =
                tray_icon::TrayIconEvent::receiver().try_recv()
            {
                ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
                ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
                self.hidden = false;
            }
        }

        // Poll daemon state
        self.state.daemon = read_state();

        // macOS/Windows: auto-show window on new stream request
        #[cfg(not(target_os = "linux"))]
        {
            if self.state.daemon.pending_request.is_some() && !self.had_pending {
                self.had_pending = true;
                if self.hidden {
                    ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
                    ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
                    self.hidden = false;
                }
            } else if self.state.daemon.pending_request.is_none() {
                self.had_pending = false;
            }
        }

        // Reload config if daemon shows different linked servers
        if self.state.daemon.linked_servers
            != self
                .state
                .config
                .links
                .iter()
                .map(|l| l.name.clone())
                .collect::<Vec<_>>()
        {
            self.state.config = AppConfig::load_sync().unwrap_or_default();
        }

        if let Some(ref err) = self.state.daemon.error {
            self.state.status_msg = err.clone();
        }

        ctx.request_repaint_after(std::time::Duration::from_millis(250));

        let blurple = egui::Color32::from_rgb(88, 101, 242);
        let panel_frame = egui::Frame::new()
            .fill(egui::Color32::from_rgb(30, 31, 34))
            .inner_margin(egui::Margin::same(16));

        let daemon = &self.state.daemon;
        let is_linked = !self.state.config.links.is_empty() || self.state.config.link.is_some();

        egui::CentralPanel::default()
            .frame(panel_frame)
            .show(ctx, |ui| {
                // Header
                ui.heading(
                    egui::RichText::new("Meshcast")
                        .color(egui::Color32::WHITE)
                        .strong(),
                );
                ui.add_space(4.0);

                // Status pill
                let (color, label) = if daemon.streaming {
                    (egui::Color32::from_rgb(237, 66, 69), "LIVE")
                } else if daemon.connected {
                    (egui::Color32::from_rgb(87, 242, 135), "Connected")
                } else if is_linked {
                    (egui::Color32::from_rgb(254, 231, 92), "Offline")
                } else {
                    (egui::Color32::from_rgb(128, 132, 142), "Not linked")
                };
                ui.horizontal(|ui| {
                    ui.colored_label(color, egui::RichText::new(format!("● {label}")).strong());
                    if !self.state.status_msg.is_empty() {
                        ui.label(
                            egui::RichText::new(&self.state.status_msg)
                                .color(egui::Color32::from_rgb(148, 155, 164)),
                        );
                    }
                });

                ui.add_space(12.0);
                ui.separator();
                ui.add_space(8.0);

                // Server list
                let server_names: Vec<String> = self
                    .state
                    .config
                    .links
                    .iter()
                    .map(|l| l.name.clone())
                    .collect();

                if !server_names.is_empty() {
                    ui.label(
                        egui::RichText::new("Servers")
                            .color(egui::Color32::from_rgb(185, 187, 190))
                            .small(),
                    );
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
                        self.state.config.remove_link(&name);
                        let _ = self.state.config.save_sync();
                        send_cmd("reload");
                    }
                    ui.add_space(4.0);
                } else {
                    ui.label(
                        egui::RichText::new(
                            "Type /link in a Discord server to get a pairing code.",
                        )
                        .color(egui::Color32::from_rgb(148, 155, 164)),
                    );
                }

                // Pairing input
                ui.horizontal(|ui| {
                    let response = ui.add(
                        egui::TextEdit::singleline(&mut self.state.link_token_input)
                            .hint_text("Paste pairing code...")
                            .desired_width(ui.available_width() - 70.0),
                    );
                    let link_clicked = ui
                        .add(
                            egui::Button::new(
                                egui::RichText::new("Link").color(egui::Color32::WHITE),
                            )
                            .fill(blurple),
                        )
                        .clicked();
                    if (response.lost_focus()
                        && ui.input(|i| i.key_pressed(egui::Key::Enter)))
                        || link_clicked
                    {
                        let token = self.state.link_token_input.clone();
                        if !token.is_empty() {
                            send_cmd(&format!("link:{token}"));
                            self.state.link_token_input.clear();
                            self.state.status_msg = "Connecting...".into();
                        }
                    }
                });

                ui.add_space(8.0);
                ui.separator();
                ui.add_space(8.0);

                // Stream section (only when linked)
                if is_linked {
                    // Consent dialog for incoming stream request
                    if let Some(ref req) = daemon.pending_request {
                        ui.group(|ui| {
                            ui.label(
                                egui::RichText::new("Stream Request")
                                    .color(egui::Color32::from_rgb(254, 231, 92))
                                    .heading(),
                            );
                            if !req.server.is_empty() {
                                ui.label(
                                    egui::RichText::new(&req.server)
                                        .color(egui::Color32::WHITE)
                                        .strong(),
                                );
                            }
                            ui.label(format!("\"{}\"", req.title));
                            ui.add_space(8.0);
                            ui.horizontal(|ui| {
                                let approve = ui
                                    .add_sized(
                                        [120.0, 32.0],
                                        egui::Button::new(
                                            egui::RichText::new("Share Screen")
                                                .color(egui::Color32::WHITE),
                                        )
                                        .fill(blurple),
                                    )
                                    .clicked();
                                let reject = ui
                                    .add_sized(
                                        [120.0, 32.0],
                                        egui::Button::new("Decline")
                                            .fill(egui::Color32::from_rgb(55, 57, 63)),
                                    )
                                    .clicked();

                                if approve {
                                    send_cmd("approve");
                                }
                                if reject {
                                    send_cmd("reject");
                                    self.state.status_msg = "Stream declined.".into();
                                }
                            });
                        });
                        return;
                    }

                    // Stream status
                    if daemon.streaming {
                        ui.label(
                            egui::RichText::new(format!(
                                "{} {}fps \u{2014} {} viewer{}",
                                daemon.quality,
                                daemon.fps,
                                daemon.viewers,
                                if daemon.viewers == 1 { "" } else { "s" }
                            ))
                            .color(egui::Color32::from_rgb(185, 187, 190)),
                        );
                        ui.add_space(8.0);
                        if ui
                            .add_sized(
                                [ui.available_width(), 32.0],
                                egui::Button::new(
                                    egui::RichText::new("Stop Stream")
                                        .color(egui::Color32::WHITE),
                                )
                                .fill(egui::Color32::from_rgb(237, 66, 69)),
                            )
                            .clicked()
                        {
                            send_cmd("stop");
                        }
                    } else {
                        ui.label(
                            egui::RichText::new(
                                "Ready. Use /stream in Discord to start streaming.",
                            )
                            .color(egui::Color32::from_rgb(148, 155, 164)),
                        );
                    }
                }
            });
    }
}
