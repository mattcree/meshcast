use std::path::Path;

use anyhow::{Context, Result};
use bytes::Bytes;
use iroh::endpoint::presets;
use iroh::protocol::Router;
use iroh::{Endpoint, EndpointAddr, SecretKey};
use iroh_gossip::net::{Gossip, GOSSIP_ALPN};
use iroh_gossip::proto::TopicId;
use serde::{Deserialize, Serialize};

// Re-exports for consumers
pub use iroh::EndpointId;
pub use iroh_gossip::api::Event;

/// Messages exchanged between bot and desktop app over gossip.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Signal {
    StartStream { title: String },
    StreamReady { ticket: String },
    StopStream,
    StreamStopped,
    WatchStream { ticket: String },
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

    /// Encode as "meshcast1<BASE32>" string.
    pub fn to_string(&self) -> Result<String> {
        let bytes = postcard::to_allocvec(self)?;
        let encoded = data_encoding::BASE32_NOPAD.encode(&bytes);
        Ok(format!("meshcast1{encoded}"))
    }

    /// Decode from "meshcast1<BASE32>" string.
    pub fn from_str(s: &str) -> Result<Self> {
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
