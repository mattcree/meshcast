//! Shared logging/telemetry setup for the three binaries.
//!
//! Every real launch path (tray → daemon → viewer, app → daemon) detaches
//! stdout/stderr, so without a file sink crashes are invisible. [`init`] adds a
//! size-capped log file next to the config data plus a panic hook, keeps the
//! stderr layer for terminal runs, and applies default filters that don't hide
//! the networking crates most likely to fail on a home connection.

use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::EnvFilter;

/// The workspace version, baked in at build time.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// One log file, capped in size (rotated to `.1` once when full).
const MAX_LOG_BYTES: u64 = 4 * 1024 * 1024;

/// Directory for log files: `$XDG_STATE_HOME/meshcast` (Linux),
/// `~/Library/Logs/meshcast` (macOS), `%LOCALAPPDATA%\meshcast\logs` (Windows).
/// Falls back to the config dir.
pub fn log_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("MESHCAST_LOG_DIR") {
        return PathBuf::from(dir);
    }
    #[cfg(target_os = "linux")]
    {
        if let Some(base) = std::env::var_os("XDG_STATE_HOME") {
            return PathBuf::from(base).join("meshcast");
        }
        if let Some(home) = dirs_next::home_dir() {
            return home.join(".local/state/meshcast");
        }
    }
    #[cfg(target_os = "macos")]
    {
        if let Some(home) = dirs_next::home_dir() {
            return home.join("Library/Logs/meshcast");
        }
    }
    #[cfg(target_os = "windows")]
    {
        if let Some(data) = dirs_next::data_local_dir() {
            return data.join("meshcast").join("logs");
        }
    }
    crate::AppConfig::config_dir().join("logs")
}

/// Path of a component's current log file (`daemon`, `app`, `bot`, `watch`).
pub fn log_path(component: &str) -> PathBuf {
    log_dir().join(format!("{component}.log"))
}

/// A shared append handle used as a `tracing` writer.
#[derive(Clone)]
struct FileWriter(Arc<Mutex<File>>);

impl Write for FileWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self.0.lock() {
            Ok(mut f) => f.write(buf),
            Err(_) => Ok(buf.len()),
        }
    }
    fn flush(&mut self) -> io::Result<()> {
        self.0.lock().map(|mut f| f.flush()).unwrap_or(Ok(()))
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for FileWriter {
    type Writer = FileWriter;
    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

/// Open the component's log file, rotating it to `.1` if it's over the cap.
fn open_log(component: &str) -> Option<File> {
    let dir = log_dir();
    std::fs::create_dir_all(&dir).ok()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700));
    }
    let path = log_path(component);
    if std::fs::metadata(&path)
        .map(|m| m.len() > MAX_LOG_BYTES)
        .unwrap_or(false)
    {
        let _ = std::fs::rename(&path, path.with_extension("log.1"));
    }
    let mut opts = OpenOptions::new();
    opts.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    opts.open(&path).ok()
}

/// Default env-filter directive for a component. A bare `warn` base means
/// warnings/errors from *every* crate (relay, gossip, quinn…) show up, with our
/// own crates at info.
fn default_filter(component: &str) -> String {
    match component {
        "bot" => "warn,meshcast_bot=info,meshcast_signal=info".into(),
        _ => "warn,meshcast=info,meshcast_app=info,meshcast_signal=info,iroh_live=info".into(),
    }
}

/// Initialise logging for `component`. Honours `RUST_LOG`/`MESHCAST_LOG`;
/// writes to stderr and (best effort) to a rotating file; installs a panic
/// hook that records the panic to the log before the process dies.
pub fn init(component: &str) {
    let filter = EnvFilter::try_from_env("MESHCAST_LOG")
        .or_else(|_| EnvFilter::try_from_default_env())
        .unwrap_or_else(|_| EnvFilter::new(default_filter(component)));

    let stderr_layer = tracing_subscriber::fmt::layer().with_writer(io::stderr);

    let file_layer = open_log(component).map(|f| {
        tracing_subscriber::fmt::layer()
            .with_ansi(false)
            .with_writer(FileWriter(Arc::new(Mutex::new(f))))
    });

    tracing_subscriber::registry()
        .with(filter)
        .with(stderr_layer)
        .with(file_layer)
        .init();

    tracing::info!(version = VERSION, component, "Meshcast starting");
    install_panic_hook(component);
}

fn install_panic_hook(component: &str) {
    let component = component.to_string();
    let default = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let loc = info
            .location()
            .map(|l| format!("{}:{}", l.file(), l.line()))
            .unwrap_or_else(|| "?".into());
        let msg = info
            .payload()
            .downcast_ref::<&str>()
            .copied()
            .or_else(|| info.payload().downcast_ref::<String>().map(|s| s.as_str()))
            .unwrap_or("(non-string panic)");
        tracing::error!(component = %component, "PANIC at {loc}: {msg}");
        // Best-effort direct write in case the subscriber is wedged.
        if let Some(mut f) = open_log(&component) {
            let _ = writeln!(f, "PANIC [{component} {VERSION}] at {loc}: {msg}");
        }
        default(info);
    }));
}

/// Read the last `n` lines of a component's log (for `meshcast status`).
pub fn tail(component: &str, n: usize) -> Vec<String> {
    let Ok(data) = std::fs::read_to_string(log_path(component)) else {
        return Vec::new();
    };
    let lines: Vec<&str> = data.lines().collect();
    lines[lines.len().saturating_sub(n)..]
        .iter()
        .map(|s| s.to_string())
        .collect()
}
