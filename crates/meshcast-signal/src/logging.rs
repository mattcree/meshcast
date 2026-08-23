//! Logging setup shared by the binaries: stderr + (optionally) a rolling log
//! file, a panic hook, sane default filters, and a version banner.
//!
//! Log files live in [`log_dir`] (`$MESHCAST_LOG_DIR`, else the platform state
//! dir — `~/.local/state/meshcast` on Linux — else `<config dir>/logs`), one
//! file per component per day, five kept. They exist so "nothing happens"
//! reports can be diagnosed after the fact: every real launch path (tray,
//! autostart, app → daemon) discards stderr.

use std::path::PathBuf;

use tracing_appender::rolling::{RollingFileAppender, Rotation};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::EnvFilter;

use crate::AppConfig;

/// Directory for log files.
pub fn log_dir() -> PathBuf {
    if let Some(d) = std::env::var_os("MESHCAST_LOG_DIR") {
        return PathBuf::from(d);
    }
    dirs_next::state_dir()
        .map(|d| d.join("meshcast"))
        .unwrap_or_else(|| AppConfig::config_dir().join("logs"))
}

/// Path prefix of a component's log files (e.g. `…/daemon.2026-08-23.log`).
pub fn log_file_hint(component: &str) -> PathBuf {
    log_dir().join(format!("{component}.<date>.log"))
}

/// Initialise tracing for `component` ("daemon", "viewer", "app", "bot").
///
/// * Filter: `$MESHCAST_LOG`, else `$RUST_LOG`, else `default_filter`.
/// * Always logs to stderr; additionally to a daily-rolling file when `to_file`.
/// * Installs a panic hook that records the panic (then delegates to the
///   default hook so the process still aborts normally).
/// * Logs a version banner so any log starts with what was running.
pub fn init(component: &'static str, version: &str, default_filter: &str, to_file: bool) {
    let filter = std::env::var("MESHCAST_LOG")
        .ok()
        .and_then(|f| EnvFilter::try_new(f).ok())
        .or_else(|| EnvFilter::try_from_default_env().ok())
        .unwrap_or_else(|| EnvFilter::new(default_filter));

    let stderr_layer = tracing_subscriber::fmt::layer().with_writer(std::io::stderr);

    let mut file_path_note = None;
    let file_layer = if to_file {
        let dir = log_dir();
        match std::fs::create_dir_all(&dir) {
            Ok(()) => {
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700));
                }
                match RollingFileAppender::builder()
                    .rotation(Rotation::DAILY)
                    .max_log_files(5)
                    .filename_prefix(component)
                    .filename_suffix("log")
                    .build(&dir)
                {
                    Ok(appender) => {
                        file_path_note = Some(dir);
                        Some(
                            tracing_subscriber::fmt::layer()
                                .with_ansi(false)
                                .with_writer(appender),
                        )
                    }
                    Err(e) => {
                        eprintln!("meshcast: log file disabled ({e})");
                        None
                    }
                }
            }
            Err(e) => {
                eprintln!("meshcast: log file disabled (cannot create {}: {e})", dir.display());
                None
            }
        }
    } else {
        None
    };

    // `try_init` so a second call (tests) is harmless.
    let _ = tracing_subscriber::registry()
        .with(filter)
        .with(stderr_layer)
        .with(file_layer)
        .try_init();

    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        tracing::error!(target: "panic", "{info}");
        default_hook(info);
    }));

    tracing::info!(
        component,
        version,
        os = std::env::consts::OS,
        arch = std::env::consts::ARCH,
        log_dir = ?file_path_note,
        "meshcast starting"
    );
}

/// Last `n` lines of the newest log file for `component`, if any.
pub fn tail(component: &str, n: usize) -> Option<(PathBuf, Vec<String>)> {
    let dir = log_dir();
    let mut files: Vec<PathBuf> = std::fs::read_dir(&dir)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with(&format!("{component}.")) && n.ends_with(".log"))
        })
        .collect();
    files.sort();
    let newest = files.pop()?;
    let content = std::fs::read_to_string(&newest).ok()?;
    let lines: Vec<String> = content.lines().map(|l| l.to_string()).collect();
    let start = lines.len().saturating_sub(n);
    Some((newest, lines[start..].to_vec()))
}
