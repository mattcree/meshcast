//! Shared types and helpers for Meshcast.
//!
//! This crate is the contract between the three binaries:
//!
//! * `meshcast-bot` (Discord bot) and `meshcast daemon` talk over iroh-gossip
//!   using [`Signal`] (steady state) and [`PairSignal`] (one-time pairing).
//! * `meshcast daemon` and `meshcast-app` (GUI) / the tray script talk over
//!   small files in the config directory — see [`ipc`].
//!
//! **Wire-format note:** [`Signal`] and [`PairSignal`] are encoded with
//! `postcard`, which identifies enum variants by *index*. Only ever append new
//! variants at the end; never reorder or remove existing ones.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use bytes::Bytes;
use iroh::endpoint::presets;
use iroh::protocol::Router;
use iroh::{Endpoint, EndpointAddr, SecretKey};
use iroh_gossip::net::{Gossip, GOSSIP_ALPN};
use serde::{Deserialize, Serialize};

// Re-exports for consumers
pub use iroh::EndpointId;
pub use iroh_gossip::api::{Event, GossipSender};
pub use iroh_gossip::proto::TopicId;

pub mod ipc;
pub mod process;

// ---------------------------------------------------------------------------
// Gossip protocol
// ---------------------------------------------------------------------------

/// Messages exchanged between bot and desktop app over gossip.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Signal {
    /// Bot → app: the user ran `/stream` and clicked Start.
    StartStream {
        title: String,
        quality: String,
        fps: u32,
        server: String,
    },
    /// App → bot: capture is running, here is the iroh-live ticket.
    StreamReady {
        ticket: String,
    },
    /// Bot → app: stop the current stream.
    StopStream,
    /// App → bot: the stream has stopped (for any reason).
    StreamStopped,
    /// Bot → app: the user clicked Watch, open the viewer for this ticket.
    WatchStream {
        ticket: String,
    },
    /// Bot → app (streamer): current viewer count.
    ViewerUpdate {
        count: u32,
    },
    Ping,
    Pong,
    /// App → bot: capture could not be started (user declined or an error).
    StreamFailed {
        reason: String,
    },
}

impl Signal {
    pub fn encode(&self) -> Result<Bytes> {
        Ok(Bytes::from(postcard::to_allocvec(self)?))
    }

    pub fn decode(bytes: &[u8]) -> Result<Self> {
        Ok(postcard::from_bytes(bytes)?)
    }
}

/// Signal for the PIN exchange during pairing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum PairSignal {
    /// App sends PIN to bot to request pairing.
    PairRequest { pin: String },
    /// Bot responds with the gossip topic and server name if PIN is valid.
    PairAccepted {
        topic: [u8; 32],
        server_name: String,
    },
    /// Bot rejects the PIN.
    PairRejected { reason: String },
}

impl PairSignal {
    pub fn encode(&self) -> Result<Bytes> {
        Ok(Bytes::from(postcard::to_allocvec(self)?))
    }

    pub fn decode(data: &[u8]) -> Result<Self> {
        Ok(postcard::from_bytes(data)?)
    }
}

// ---------------------------------------------------------------------------
// Stream settings
// ---------------------------------------------------------------------------

/// Supported resolution presets, in ascending order.
pub const QUALITIES: [&str; 3] = ["360p", "720p", "1080p"];
/// Supported frame rates.
pub const FPS_OPTIONS: [u32; 2] = [30, 60];
pub const DEFAULT_QUALITY: &str = "720p";
pub const DEFAULT_FPS: u32 = 30;
/// Maximum length of a stream title (also bounded by Discord embed limits).
pub const MAX_TITLE_LEN: usize = 80;

/// Normalise a quality string to one of [`QUALITIES`], falling back to the default.
pub fn normalize_quality(q: &str) -> &'static str {
    let q = q.trim().to_ascii_lowercase();
    QUALITIES
        .iter()
        .copied()
        .find(|known| *known == q)
        .unwrap_or(DEFAULT_QUALITY)
}

/// Normalise an FPS value to one of [`FPS_OPTIONS`], falling back to the default.
pub fn normalize_fps(fps: u32) -> u32 {
    if FPS_OPTIONS.contains(&fps) {
        fps
    } else {
        DEFAULT_FPS
    }
}

/// Trim and bound a user-supplied stream title; strips control characters.
pub fn sanitize_title(title: &str) -> String {
    let cleaned: String = title
        .chars()
        .filter(|c| !c.is_control())
        .collect::<String>()
        .trim()
        .to_string();
    if cleaned.is_empty() {
        return String::new();
    }
    cleaned.chars().take(MAX_TITLE_LEN).collect()
}

// ---------------------------------------------------------------------------
// Daemon state (shared with GUI and tray via files)
// ---------------------------------------------------------------------------

/// Daemon state written to the state file as JSON.
/// Read by the tray script and the thin app window.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct DaemonState {
    pub streaming: bool,
    pub connected: bool,
    #[serde(default)]
    pub quality: String,
    #[serde(default)]
    pub fps: u32,
    #[serde(default)]
    pub viewers: u32,
    #[serde(default)]
    pub stream_ticket: Option<String>,
    #[serde(default)]
    pub linked_servers: Vec<String>,
    #[serde(default)]
    pub pending_request: Option<StreamRequest>,
    #[serde(default)]
    pub error: Option<String>,
}

/// A pending stream request from the Discord bot, awaiting user consent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StreamRequest {
    pub title: String,
    pub server: String,
    pub quality: String,
    pub fps: u32,
}

// ---------------------------------------------------------------------------
// Pairing
// ---------------------------------------------------------------------------

/// Number of characters in the pairing PIN (~40 bits of entropy).
pub const PIN_LEN: usize = 8;
const PIN_CHARSET: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZ23456789"; // no 0/O/1/I confusion

/// Short pairing code: bot endpoint ID + PIN.
/// Format: `XXXX-XXXX-...-XXXXXXXX` (base32 endpoint ID with dashes, then the PIN).
pub struct PairCode;

impl PairCode {
    /// Generate a random PIN from [`PIN_CHARSET`].
    pub fn generate_pin() -> String {
        use rand::Rng;
        let mut rng = rand::thread_rng();
        (0..PIN_LEN)
            .map(|_| PIN_CHARSET[rng.gen_range(0..PIN_CHARSET.len())] as char)
            .collect()
    }

    /// Encode a full pairing code from bot endpoint ID + PIN.
    pub fn encode_full(bot_endpoint_id: EndpointId, pin: &str) -> String {
        let id_base32 = data_encoding::BASE32_NOPAD.encode(bot_endpoint_id.as_bytes());
        let chunked: Vec<&str> = id_base32
            .as_bytes()
            .chunks(4)
            .map(|c| std::str::from_utf8(c).unwrap_or(""))
            .collect();
        format!("{}-{pin}", chunked.join("-"))
    }

    /// Parse a full pairing code. Returns `(bot_endpoint_id, pin)`.
    pub fn parse(input: &str) -> Result<(EndpointId, String)> {
        let input = input.trim().to_uppercase();
        let parts: Vec<&str> = input.split('-').filter(|p| !p.is_empty()).collect();
        if parts.len() < 2 {
            anyhow::bail!("Invalid pairing code. Use the full code from /link in Discord.");
        }

        let pin = (*parts.last().expect("len >= 2")).to_string();
        if pin.len() != PIN_LEN || !pin.bytes().all(|b| PIN_CHARSET.contains(&b)) {
            anyhow::bail!("Invalid pairing code format");
        }

        let id_base32: String = parts[..parts.len() - 1].join("");
        let id_bytes = data_encoding::BASE32_NOPAD
            .decode(id_base32.as_bytes())
            .context("Invalid pairing code")?;
        let arr: [u8; 32] = id_bytes
            .as_slice()
            .try_into()
            .map_err(|_| anyhow::anyhow!("Invalid pairing code (bad length)"))?;
        let endpoint_id = EndpointId::from_bytes(&arr)
            .map_err(|e| anyhow::anyhow!("Invalid pairing code: {e}"))?;

        Ok((endpoint_id, pin))
    }
}

/// Derive the temporary gossip topic used for the pairing exchange from a PIN.
///
/// Both sides compute the same topic, which lets the app rendezvous with the
/// bot without knowing the real (secret) link topic yet. Uses BLAKE3 key
/// derivation so the result is stable across platforms and Rust versions.
pub fn derive_pairing_topic(pin: &str) -> TopicId {
    let pin = pin.trim().to_uppercase();
    let bytes = blake3::derive_key("meshcast pairing topic v1", pin.as_bytes());
    TopicId::from_bytes(bytes)
}

// ---------------------------------------------------------------------------
// Persisted link state
// ---------------------------------------------------------------------------

/// App-side persisted link — survives restarts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LinkState {
    pub topic: [u8; 32],
    pub secret_key: [u8; 32],
    pub peer_id: [u8; 32],
}

impl LinkState {
    pub fn new(topic: TopicId, secret_key: &SecretKey, peer_id: EndpointId) -> Self {
        Self {
            topic: *topic.as_bytes(),
            secret_key: secret_key.to_bytes(),
            peer_id: *peer_id.as_bytes(),
        }
    }

    pub fn topic_id(&self) -> TopicId {
        TopicId::from_bytes(self.topic)
    }

    pub fn secret_key(&self) -> SecretKey {
        SecretKey::from_bytes(&self.secret_key)
    }

    pub fn peer_endpoint_id(&self) -> Result<EndpointId> {
        EndpointId::from_bytes(&self.peer_id).context("Stored peer ID is not a valid key")
    }
}

/// A single bot-side link: the gossip topic shared with one Discord user's app.
///
/// Older state files stored a full [`LinkState`] here; the extra fields are
/// ignored on load, so this is backwards compatible.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BotLink {
    pub topic: [u8; 32],
}

impl BotLink {
    pub fn topic_id(&self) -> TopicId {
        TopicId::from_bytes(self.topic)
    }
}

/// Bot-side persistent link store.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct BotLinkStore {
    /// Bot's secret key for a stable endpoint identity across restarts.
    pub bot_secret_key: Option<[u8; 32]>,
    /// Per-user links keyed by Discord user ID string.
    pub links: std::collections::HashMap<String, BotLink>,
}

impl BotLinkStore {
    pub async fn load(path: &Path) -> Result<Self> {
        match tokio::fs::read_to_string(path).await {
            Ok(data) => serde_json::from_str(&data).context("Bot state file is corrupt"),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e).context("Failed to read bot link store"),
        }
    }

    pub async fn save(&self, path: &Path) -> Result<()> {
        let data = serde_json::to_string_pretty(self)?;
        write_private_file(path, data.as_bytes())
    }

    pub fn bot_secret_key(&self) -> Option<SecretKey> {
        self.bot_secret_key.map(|b| SecretKey::from_bytes(&b))
    }
}

// ---------------------------------------------------------------------------
// App configuration
// ---------------------------------------------------------------------------

/// Shared app configuration — used by both CLI and GUI app.
/// Stored at `<config dir>/config.toml` (see [`AppConfig::config_dir`]).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AppConfig {
    #[serde(default)]
    pub video: VideoConfig,
    #[serde(default)]
    pub audio: AudioConfig,
    /// Legacy single link (pre-0.4). Migrated into `links` on load.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub link: Option<LinkConfig>,
    /// Links to Discord bots (one per bot instance).
    #[serde(default)]
    pub links: Vec<ServerLink>,
}

/// A named link to a Discord bot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerLink {
    /// Display name (the Discord server the link was created from).
    pub name: String,
    #[serde(flatten)]
    pub config: LinkConfig,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VideoConfig {
    #[serde(default = "default_quality")]
    pub quality: String,
    #[serde(default = "default_fps")]
    pub fps: u32,
    #[serde(default = "default_codec")]
    pub codec: String,
}

fn default_quality() -> String {
    DEFAULT_QUALITY.into()
}
fn default_fps() -> u32 {
    DEFAULT_FPS
}
fn default_codec() -> String {
    "h264".into()
}

impl Default for VideoConfig {
    fn default() -> Self {
        Self {
            quality: default_quality(),
            fps: default_fps(),
            codec: default_codec(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AudioConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
}

fn default_true() -> bool {
    true
}

impl Default for AudioConfig {
    fn default() -> Self {
        Self { enabled: true }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LinkConfig {
    pub topic: [u8; 32],
    pub secret_key: [u8; 32],
    pub peer_id: [u8; 32],
}

impl From<LinkState> for LinkConfig {
    fn from(state: LinkState) -> Self {
        Self {
            topic: state.topic,
            secret_key: state.secret_key,
            peer_id: state.peer_id,
        }
    }
}

impl From<LinkConfig> for LinkState {
    fn from(cfg: LinkConfig) -> Self {
        Self {
            topic: cfg.topic,
            secret_key: cfg.secret_key,
            peer_id: cfg.peer_id,
        }
    }
}

impl AppConfig {
    /// Directory holding config, state and PID files.
    ///
    /// Resolution order: `$MESHCAST_CONFIG_DIR`, then the platform config dir
    /// (`$XDG_CONFIG_HOME/meshcast` or `~/.config/meshcast` on Linux,
    /// `~/Library/Application Support/meshcast` on macOS, `%APPDATA%\meshcast`
    /// on Windows).
    pub fn config_dir() -> PathBuf {
        if let Some(dir) = std::env::var_os("MESHCAST_CONFIG_DIR") {
            return PathBuf::from(dir);
        }
        dirs_next::config_dir()
            .or_else(|| dirs_next::home_dir().map(|h| h.join(".config")))
            .unwrap_or_default()
            .join("meshcast")
    }

    pub fn config_path() -> PathBuf {
        Self::config_dir().join("config.toml")
    }

    pub fn state_path() -> PathBuf {
        Self::config_dir().join(".tray-state")
    }

    pub fn cmd_path() -> PathBuf {
        Self::config_dir().join(".tray-cmd")
    }

    pub fn daemon_pid_path() -> PathBuf {
        Self::config_dir().join(".daemon-pid")
    }

    pub fn app_pid_path() -> PathBuf {
        Self::config_dir().join(".app-pid")
    }

    /// Parse config from TOML, migrating legacy fields.
    pub fn from_toml(data: &str) -> Result<Self> {
        let mut cfg: Self = toml::from_str(data).context("Config file is not valid TOML")?;
        cfg.migrate();
        Ok(cfg)
    }

    /// Serialise to TOML.
    pub fn to_toml(&self) -> Result<String> {
        Ok(toml::to_string_pretty(self)?)
    }

    fn migrate(&mut self) {
        if let Some(legacy) = self.link.take() {
            if self.links.is_empty() {
                self.links.push(ServerLink {
                    name: "Discord".into(),
                    config: legacy,
                });
            }
        }
    }

    pub async fn load() -> Result<Self> {
        let path = Self::config_path();
        match tokio::fs::read_to_string(&path).await {
            Ok(data) => Self::from_toml(&data),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e).context("Failed to read config"),
        }
    }

    pub async fn save(&self) -> Result<()> {
        self.save_sync()
    }

    /// Load config synchronously (for use without tokio).
    pub fn load_sync() -> Result<Self> {
        let path = Self::config_path();
        match std::fs::read_to_string(&path) {
            Ok(data) => Self::from_toml(&data),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e).context("Failed to read config"),
        }
    }

    /// Save config synchronously (for use without tokio).
    pub fn save_sync(&self) -> Result<()> {
        let data = self.to_toml()?;
        write_private_file(&Self::config_path(), data.as_bytes())
    }

    pub fn is_linked(&self) -> bool {
        !self.links.is_empty()
    }

    /// All link states, in configuration order.
    pub fn link_states(&self) -> Vec<LinkState> {
        self.links
            .iter()
            .map(|sl| LinkState::from(sl.config.clone()))
            .collect()
    }

    /// The first link state, if any (used for the daemon's endpoint identity).
    pub fn link_state(&self) -> Option<LinkState> {
        self.links
            .first()
            .map(|sl| LinkState::from(sl.config.clone()))
    }

    /// Add a link. A link with the same name or the same topic is replaced.
    pub fn add_link(&mut self, name: String, config: LinkConfig) {
        self.links
            .retain(|l| l.name != name && l.config.topic != config.topic);
        self.links.push(ServerLink { name, config });
        self.link = None;
    }

    /// Remove a link by name. Returns true if something was removed.
    pub fn remove_link(&mut self, name: &str) -> bool {
        let before = self.links.len();
        self.links.retain(|l| l.name != name);
        self.links.len() < before
    }
}

/// Write a file atomically (temp file + rename), creating parent directories,
/// with `0600` permissions on Unix.
pub fn write_private_file(path: &Path, data: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    std::fs::create_dir_all(&parent)
        .with_context(|| format!("Failed to create {}", parent.display()))?;
    let tmp = parent.join(format!(
        ".{}.tmp-{}",
        path.file_name().and_then(|n| n.to_str()).unwrap_or("file"),
        std::process::id()
    ));
    {
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let mut f = opts
            .open(&tmp)
            .with_context(|| format!("Failed to write {}", tmp.display()))?;
        use std::io::Write;
        f.write_all(data)?;
        f.sync_all().ok();
    }
    std::fs::rename(&tmp, path).with_context(|| format!("Failed to replace {}", path.display()))?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Tickets
// ---------------------------------------------------------------------------

/// Validate and sanitise a ticket string before use.
pub fn validate_ticket(ticket: &str) -> Result<&str> {
    let ticket = ticket.trim();
    if ticket.is_empty() {
        anyhow::bail!("Empty ticket");
    }
    if ticket.len() > 2048 {
        anyhow::bail!("Ticket is too long");
    }
    // iroh-live tickets are: "iroh-live:" + base32/base64url + "/" + name
    if !ticket
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | ':' | '/' | '+' | '='))
    {
        anyhow::bail!("Ticket contains invalid characters");
    }
    Ok(ticket)
}

/// Strip a `meshcast://watch/` URI prefix if present, returning the bare ticket.
pub fn parse_ticket_uri(raw: &str) -> &str {
    let raw = raw.trim();
    raw.strip_prefix("meshcast://watch/")
        .or_else(|| raw.strip_prefix("meshcast:///watch/"))
        .or_else(|| raw.strip_prefix("meshcast:watch/"))
        .unwrap_or(raw)
        .trim_end_matches('/')
}

/// Build a `meshcast://watch/<ticket>` URI.
pub fn ticket_uri(ticket: &str) -> String {
    format!("meshcast://watch/{ticket}")
}

// ---------------------------------------------------------------------------
// Signal node
// ---------------------------------------------------------------------------

/// Lightweight iroh node for gossip-only communication.
pub struct SignalNode {
    pub endpoint: Endpoint,
    pub gossip: Gossip,
    router: Router,
}

impl SignalNode {
    /// Create a new signal node. If `secret_key` is provided, uses it for
    /// stable identity across restarts.
    pub async fn new(secret_key: Option<SecretKey>) -> Result<Self> {
        let mut builder = Endpoint::builder(presets::N0);
        if let Some(key) = secret_key {
            builder = builder.secret_key(key);
        }
        let endpoint = builder.bind().await.context("Failed to bind endpoint")?;

        let gossip = Gossip::builder().spawn(endpoint.clone());

        let router = Router::builder(endpoint.clone())
            .accept(GOSSIP_ALPN, gossip.clone())
            .spawn();

        // Wait for relay connection so our address includes the relay URL
        endpoint.online().await;

        tracing::info!(
            endpoint_id = %endpoint.id().fmt_short(),
            "Signal node online"
        );

        Ok(Self {
            endpoint,
            gossip,
            router,
        })
    }

    /// Get our full endpoint address (includes relay URL).
    pub fn addr(&self) -> EndpointAddr {
        self.endpoint.addr()
    }

    /// Gracefully shut down gossip and the endpoint.
    pub async fn shutdown(self) {
        if let Err(e) = self.router.shutdown().await {
            tracing::debug!("Router shutdown: {e}");
        }
        self.endpoint.close().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signal_roundtrip() {
        let signals = vec![
            Signal::StartStream {
                title: "Game Night".into(),
                quality: "1080p".into(),
                fps: 60,
                server: "Shlug Life".into(),
            },
            Signal::StreamReady {
                ticket: "iroh-live:abc/meshcast".into(),
            },
            Signal::StopStream,
            Signal::StreamStopped,
            Signal::WatchStream {
                ticket: "iroh-live:abc/meshcast".into(),
            },
            Signal::ViewerUpdate { count: 3 },
            Signal::Ping,
            Signal::Pong,
            Signal::StreamFailed {
                reason: "declined".into(),
            },
        ];
        for s in signals {
            let bytes = s.encode().unwrap();
            assert_eq!(Signal::decode(&bytes).unwrap(), s);
        }
    }

    #[test]
    fn signal_wire_format_is_stable() {
        // Variant indices are the wire format. Guard against accidental reordering.
        assert_eq!(Signal::StopStream.encode().unwrap().as_ref(), &[2]);
        assert_eq!(Signal::StreamStopped.encode().unwrap().as_ref(), &[3]);
        assert_eq!(Signal::Ping.encode().unwrap().as_ref(), &[6]);
        assert_eq!(Signal::Pong.encode().unwrap().as_ref(), &[7]);
    }

    #[test]
    fn pair_signal_roundtrip() {
        let s = PairSignal::PairAccepted {
            topic: [7u8; 32],
            server_name: "Test".into(),
        };
        assert_eq!(PairSignal::decode(&s.encode().unwrap()).unwrap(), s);
    }

    #[test]
    fn pair_code_roundtrip() {
        let key = SecretKey::from_bytes(&[42u8; 32]);
        let id = key.public();
        let pin = PairCode::generate_pin();
        assert_eq!(pin.len(), PIN_LEN);
        assert!(pin.bytes().all(|b| PIN_CHARSET.contains(&b)));

        let code = PairCode::encode_full(id, &pin);
        let (parsed_id, parsed_pin) = PairCode::parse(&code).unwrap();
        assert_eq!(parsed_id, id);
        assert_eq!(parsed_pin, pin);

        // Tolerates whitespace and lowercase
        let messy = format!("  {}  ", code.to_lowercase());
        assert_eq!(PairCode::parse(&messy).unwrap().0, id);
    }

    #[test]
    fn pair_code_rejects_garbage() {
        assert!(PairCode::parse("").is_err());
        assert!(PairCode::parse("ABCD-EFGH").is_err());
        assert!(PairCode::parse("notacode").is_err());
        assert!(PairCode::parse("AAAA-AAAA-0000000O").is_err()); // invalid PIN chars
    }

    #[test]
    fn pairing_topic_is_deterministic_and_case_insensitive() {
        let a = derive_pairing_topic("ABCDEFGH");
        let b = derive_pairing_topic(" abcdefgh ");
        assert_eq!(a, b);
        assert_ne!(a, derive_pairing_topic("ABCDEFGJ"));
        // Pin a known value so bot/app built from different commits stay compatible.
        assert_eq!(
            a,
            TopicId::from_bytes(blake3::derive_key("meshcast pairing topic v1", b"ABCDEFGH"))
        );
    }

    #[test]
    fn quality_and_fps_normalisation() {
        assert_eq!(normalize_quality("1080P"), "1080p");
        assert_eq!(normalize_quality(" 360p "), "360p");
        assert_eq!(normalize_quality("4k"), DEFAULT_QUALITY);
        assert_eq!(normalize_fps(60), 60);
        assert_eq!(normalize_fps(144), DEFAULT_FPS);
    }

    #[test]
    fn title_sanitising() {
        assert_eq!(sanitize_title("  Game\nNight  "), "GameNight");
        assert_eq!(sanitize_title(""), "");
        assert_eq!(sanitize_title("   "), "");
        let long = "x".repeat(500);
        assert_eq!(sanitize_title(&long).chars().count(), MAX_TITLE_LEN);
    }

    #[test]
    fn ticket_validation_and_uri() {
        assert!(validate_ticket("iroh-live:abc_DEF-123/meshcast").is_ok());
        assert!(validate_ticket("  iroh-live:abc/x  ").is_ok());
        assert!(validate_ticket("").is_err());
        assert!(validate_ticket("bad ticket").is_err());
        assert!(validate_ticket("bad;ticket").is_err());
        assert!(validate_ticket(&"a".repeat(3000)).is_err());

        assert_eq!(
            parse_ticket_uri("meshcast://watch/iroh-live:abc/x"),
            "iroh-live:abc/x"
        );
        assert_eq!(
            parse_ticket_uri("meshcast:///watch/iroh-live:abc/x"),
            "iroh-live:abc/x"
        );
        assert_eq!(parse_ticket_uri("iroh-live:abc/x"), "iroh-live:abc/x");
        assert_eq!(ticket_uri("t"), "meshcast://watch/t");
    }

    #[test]
    fn config_roundtrip_and_legacy_migration() {
        let mut cfg = AppConfig::default();
        cfg.add_link(
            "Server A".into(),
            LinkConfig {
                topic: [1; 32],
                secret_key: [2; 32],
                peer_id: [3; 32],
            },
        );
        let toml = cfg.to_toml().unwrap();
        let back = AppConfig::from_toml(&toml).unwrap();
        assert_eq!(back.links, cfg.links);
        assert_eq!(back.video, cfg.video);

        // Legacy single `link` table migrates into `links`.
        let legacy = r#"
[video]
quality = "720p"
fps = 30
codec = "h264"

[link]
topic = [9,9,9,9,9,9,9,9,9,9,9,9,9,9,9,9,9,9,9,9,9,9,9,9,9,9,9,9,9,9,9,9]
secret_key = [1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1]
peer_id = [2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2]
"#;
        let migrated = AppConfig::from_toml(legacy).unwrap();
        assert!(migrated.link.is_none());
        assert_eq!(migrated.links.len(), 1);
        assert_eq!(migrated.links[0].config.topic, [9; 32]);

        // Re-adding with the same topic replaces rather than duplicates.
        let mut dup = migrated.clone();
        dup.add_link(
            "Renamed".into(),
            LinkConfig {
                topic: [9; 32],
                secret_key: [1; 32],
                peer_id: [2; 32],
            },
        );
        assert_eq!(dup.links.len(), 1);
        assert_eq!(dup.links[0].name, "Renamed");
        assert!(dup.remove_link("Renamed"));
        assert!(!dup.is_linked());
    }

    #[test]
    fn bot_store_reads_legacy_entries() {
        // Old format stored a full LinkState per user; extra fields must be ignored.
        let legacy = r#"{
            "bot_secret_key": null,
            "links": {
                "123": { "topic": [5,5,5,5,5,5,5,5,5,5,5,5,5,5,5,5,5,5,5,5,5,5,5,5,5,5,5,5,5,5,5,5],
                         "secret_key": [0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0],
                         "peer_id": [0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0] }
            }
        }"#;
        let store: BotLinkStore = serde_json::from_str(legacy).unwrap();
        assert_eq!(store.links["123"].topic, [5; 32]);
    }

    #[test]
    fn private_file_write_is_atomic_and_private() {
        let dir = std::env::temp_dir().join(format!("meshcast-test-{}", std::process::id()));
        let path = dir.join("nested").join("file.txt");
        write_private_file(&path, b"hello").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "hello");
        write_private_file(&path, b"world").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "world");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
