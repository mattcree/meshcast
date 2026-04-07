use std::path::Path;

use anyhow::{Context, Result};
use bytes::Bytes;
use iroh::endpoint::presets;
use iroh::protocol::Router;
use iroh::{Endpoint, EndpointAddr, SecretKey};
use iroh_gossip::net::{Gossip, GOSSIP_ALPN};
use serde::{Deserialize, Serialize};

// Re-exports for consumers
pub use iroh::EndpointId;
pub use iroh_gossip::api::Event;
pub use iroh_gossip::proto::TopicId;

/// Messages exchanged between bot and desktop app over gossip.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Signal {
    StartStream { title: String, quality: String, fps: u32 },
    StreamReady { ticket: String },
    StopStream,
    StreamStopped,
    WatchStream { ticket: String },
    ViewerUpdate { count: u32 },
    Ping,
    Pong,
}

impl Signal {
    pub fn encode(&self) -> Result<Bytes> {
        Ok(Bytes::from(postcard::to_allocvec(self)?))
    }

    pub fn decode(bytes: &[u8]) -> Result<Self> {
        Ok(postcard::from_bytes(bytes)?)
    }
}

/// Pairing token exchanged between bot and desktop app.
/// Contains the gossip topic and the bot's endpoint address.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PairToken {
    pub topic: TopicId,
    pub peers: Vec<EndpointAddr>,
}

impl PairToken {
    pub fn new(topic: TopicId, peers: Vec<EndpointAddr>) -> Self {
        Self { topic, peers }
    }

    /// Encode as "meshcast1<BASE32>" string (legacy format).
    pub fn encode(&self) -> Result<String> {
        let bytes = postcard::to_allocvec(self)?;
        let encoded = data_encoding::BASE32_NOPAD.encode(&bytes);
        Ok(format!("meshcast1{encoded}"))
    }

    /// Decode from "meshcast1<BASE32>" string (legacy format).
    pub fn decode(s: &str) -> Result<Self> {
        let s = s.trim();
        let encoded = s
            .strip_prefix("meshcast1")
            .context("Token must start with 'meshcast1'")?;
        let bytes = data_encoding::BASE32_NOPAD
            .decode(encoded.to_uppercase().as_bytes())
            .context("Invalid base32 in token")?;
        Ok(postcard::from_bytes(&bytes)?)
    }
}

/// Short pairing code — contains bot's endpoint ID + 6-digit PIN.
/// Short pairing code — bot endpoint ID + 8-char PIN.
/// Format: "XXXX-XXXX-...-XXXXXXXX" (base32 endpoint ID with dashes, then 8-char PIN)
pub struct PairCode;

impl PairCode {
    /// Generate an 8-character alphanumeric PIN (~40 bits entropy).
    pub fn generate_pin() -> String {
        const CHARSET: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZ23456789"; // no 0/O/1/I confusion
        (0..8)
            .map(|_| {
                let idx = rand::random::<usize>() % CHARSET.len();
                CHARSET[idx] as char
            })
            .collect()
    }

    /// Encode a full pairing code from bot endpoint ID + PIN.
    pub fn encode_full(bot_endpoint_id: EndpointId, pin: &str) -> String {
        let id_bytes = bot_endpoint_id.as_bytes();
        let id_base32 = data_encoding::BASE32_NOPAD.encode(id_bytes);
        // Format with dashes every 4 chars for readability
        let chunked: Vec<&str> = id_base32.as_bytes().chunks(4)
            .map(|c| std::str::from_utf8(c).unwrap_or(""))
            .collect();
        format!("{}-{pin}", chunked.join("-"))
    }

    /// Parse a pairing code. Returns (bot_endpoint_id, pin).
    /// Accepts either:
    /// - Full code: "XXXX-XXXX-...-XXXXXX" (base32 endpoint ID with dashes + PIN)
    /// - PIN only: "123456" (6 digits, requires cached bot endpoint ID)
    pub fn parse(input: &str) -> Result<(Option<EndpointId>, String)> {
        let input = input.trim().to_uppercase();

        // Check if it's just a PIN (8 alphanumeric chars, or legacy 6-digit)
        if (input.len() == 8 || input.len() == 6) && !input.contains('-') {
            return Ok((None, input));
        }

        // Full code: strip dashes, split into endpoint ID + PIN
        let parts: Vec<&str> = input.split('-').collect();
        if parts.len() < 2 {
            anyhow::bail!("Invalid pairing code format");
        }

        // Last part is the PIN (8 alphanumeric chars)
        let pin = parts.last().unwrap().to_string();
        if pin.len() < 6 || pin.len() > 8 {
            anyhow::bail!("Pairing code must end with a PIN (6-8 characters)");
        }

        // Everything before the last dash is the base32 endpoint ID
        let id_base32: String = parts[..parts.len() - 1].join("");
        let id_bytes = data_encoding::BASE32_NOPAD
            .decode(id_base32.as_bytes())
            .context("Invalid pairing code")?;

        if id_bytes.len() != 32 {
            anyhow::bail!("Invalid endpoint ID in pairing code (expected 32 bytes, got {})", id_bytes.len());
        }

        let mut arr = [0u8; 32];
        arr.copy_from_slice(&id_bytes);
        let endpoint_id = EndpointId::from_bytes(&arr)
            .map_err(|e| anyhow::anyhow!("Invalid endpoint ID: {e}"))?;

        Ok((Some(endpoint_id), pin))
    }
}

/// Derive a temporary gossip topic from a PIN for the pairing exchange.
/// Both sides compute the same topic from the PIN, enabling rendezvous.
pub fn derive_pairing_topic(pin: &str) -> TopicId {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    // Simple deterministic derivation — not cryptographic, just needs to be unique
    let mut hasher = DefaultHasher::new();
    "meshcast-pair-".hash(&mut hasher);
    pin.hash(&mut hasher);
    let h1 = hasher.finish();
    let mut hasher2 = DefaultHasher::new();
    h1.hash(&mut hasher2);
    pin.hash(&mut hasher2);
    let h2 = hasher2.finish();
    let mut hasher3 = DefaultHasher::new();
    h2.hash(&mut hasher3);
    "meshcast-pair-salt".hash(&mut hasher3);
    let h3 = hasher3.finish();
    let mut hasher4 = DefaultHasher::new();
    h3.hash(&mut hasher4);
    h1.hash(&mut hasher4);
    let h4 = hasher4.finish();
    let mut bytes = [0u8; 32];
    bytes[..8].copy_from_slice(&h1.to_le_bytes());
    bytes[8..16].copy_from_slice(&h2.to_le_bytes());
    bytes[16..24].copy_from_slice(&h3.to_le_bytes());
    bytes[24..32].copy_from_slice(&h4.to_le_bytes());
    TopicId::from_bytes(bytes)
}

/// Signal for the PIN exchange during pairing.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum PairSignal {
    /// App sends PIN to bot to request pairing.
    PairRequest { pin: String },
    /// Bot responds with the gossip topic if PIN is valid.
    PairAccepted { topic: [u8; 32] },
    /// Bot rejects the PIN.
    PairRejected { reason: String },
}

impl PairSignal {
    pub fn encode(&self) -> Result<bytes::Bytes> {
        Ok(bytes::Bytes::from(postcard::to_allocvec(self)?))
    }

    pub fn decode(data: &[u8]) -> Result<Self> {
        Ok(postcard::from_bytes(data)?)
    }
}

/// Persisted link state — survives restarts.
#[derive(Debug, Clone, Serialize, Deserialize)]
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

    pub fn peer_endpoint_id(&self) -> EndpointId {
        EndpointId::from_bytes(&self.peer_id).expect("valid 32-byte key")
    }

    pub async fn load(path: &Path) -> Result<Self> {
        let data = tokio::fs::read_to_string(path)
            .await
            .context("Failed to read link state")?;
        Ok(serde_json::from_str(&data)?)
    }

    pub async fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let data = serde_json::to_string_pretty(self)?;
        tokio::fs::write(path, data).await?;
        // Best-effort chmod 0600
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let perms = std::fs::Permissions::from_mode(0o600);
            let _ = std::fs::set_permissions(path, perms);
        }
        Ok(())
    }
}

/// Bot-side persistent link store.
/// Stores multiple user links keyed by Discord user ID, plus the bot's own secret key.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct BotLinkStore {
    /// Bot's secret key (hex-encoded 32 bytes) for stable endpoint identity.
    pub bot_secret_key: Option<[u8; 32]>,
    /// Per-user link states keyed by Discord user ID string.
    pub links: std::collections::HashMap<String, LinkState>,
}

impl BotLinkStore {
    pub async fn load(path: &Path) -> Result<Self> {
        match tokio::fs::read_to_string(path).await {
            Ok(data) => Ok(serde_json::from_str(&data)?),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e).context("Failed to read bot link store"),
        }
    }

    pub async fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let data = serde_json::to_string_pretty(self)?;
        tokio::fs::write(path, data).await?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let perms = std::fs::Permissions::from_mode(0o600);
            let _ = std::fs::set_permissions(path, perms);
        }
        Ok(())
    }

    pub fn bot_secret_key(&self) -> Option<SecretKey> {
        self.bot_secret_key.map(|b| SecretKey::from_bytes(&b))
    }
}

/// Shared app configuration — used by both CLI and GUI app.
/// Stored at `~/.config/meshcast/config.toml`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    #[serde(default)]
    pub video: VideoConfig,
    #[serde(default)]
    pub audio: AudioConfig,
    #[serde(default)]
    pub link: Option<LinkConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VideoConfig {
    #[serde(default = "default_quality")]
    pub quality: String,
    #[serde(default = "default_fps")]
    pub fps: u32,
    #[serde(default = "default_codec")]
    pub codec: String,
}

fn default_quality() -> String { "720p".into() }
fn default_fps() -> u32 { 30 }
fn default_codec() -> String { "h264".into() }

impl Default for VideoConfig {
    fn default() -> Self {
        Self {
            quality: default_quality(),
            fps: default_fps(),
            codec: default_codec(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AudioConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
}

fn default_true() -> bool { true }

impl Default for AudioConfig {
    fn default() -> Self {
        Self { enabled: true }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
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

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            video: VideoConfig::default(),
            audio: AudioConfig::default(),
            link: None,
        }
    }
}

impl AppConfig {
    pub fn config_dir() -> std::path::PathBuf {
        dirs_next::home_dir()
            .unwrap_or_default()
            .join(".config/meshcast")
    }

    pub fn config_path() -> std::path::PathBuf {
        Self::config_dir().join("config.toml")
    }

    pub async fn load() -> Result<Self> {
        let path = Self::config_path();
        match tokio::fs::read_to_string(&path).await {
            Ok(data) => Ok(toml::from_str(&data)?),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e).context("Failed to read config"),
        }
    }

    pub async fn save(&self) -> Result<()> {
        let path = Self::config_path();
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let data = toml::to_string_pretty(self)?;
        tokio::fs::write(&path, data).await?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
        }
        Ok(())
    }

    pub fn link_state(&self) -> Option<LinkState> {
        self.link.clone().map(LinkState::from)
    }
}

/// Validate and sanitize a ticket string before use.
pub fn validate_ticket(ticket: &str) -> Result<&str> {
    let ticket = ticket.trim();
    if ticket.is_empty() {
        anyhow::bail!("Empty ticket");
    }
    // iroh-live tickets are: "iroh-live:" + base64url + "/" + name
    // Only allow safe characters
    if !ticket.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | ':' | '/' | '+' | '=')) {
        anyhow::bail!("Ticket contains invalid characters");
    }
    Ok(ticket)
}

/// Launch a viewer subprocess for the given ticket, detached from the parent.
/// Cross-platform: uses setsid on Linux, direct spawn elsewhere.
pub fn launch_viewer(meshcast_bin: &std::path::Path, ticket: &str) -> Result<()> {
    let ticket = validate_ticket(ticket)?;

    #[cfg(target_os = "linux")]
    {
        std::process::Command::new("setsid")
            .args([meshcast_bin.as_os_str(), std::ffi::OsStr::new("watch"), std::ffi::OsStr::new(ticket)])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .context("Failed to launch viewer")?;
    }

    #[cfg(not(target_os = "linux"))]
    {
        std::process::Command::new(meshcast_bin)
            .args(["watch", ticket])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .context("Failed to launch viewer")?;
    }

    Ok(())
}

/// Lightweight iroh node for gossip-only communication.
pub struct SignalNode {
    pub endpoint: Endpoint,
    pub gossip: Gossip,
    _router: Router,
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
            _router: router,
        })
    }

    /// Get our full endpoint address (includes relay URL).
    pub fn addr(&self) -> EndpointAddr {
        self.endpoint.addr()
    }
}
