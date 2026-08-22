//! Remote-control channel: a viewer drives the streamer's mouse/keyboard.
//!
//! The streamer's daemon serves [`CONTROL_ALPN`] on its signal endpoint. A
//! viewer window dials it, opens one bidirectional stream, sends
//! [`ControlMsg::Hello`] with a one-time token and then streams input events.
//! Frames are `u32`-LE length-prefixed postcard. See `docs/REMOTE-CONTROL.md`.
//!
//! **Wire-format note:** append-only enums, like [`crate::Signal`].

use std::path::PathBuf;

use anyhow::{Context, Result};
use iroh::endpoint::{RecvStream, SendStream};
use iroh::EndpointAddr;
use serde::{Deserialize, Serialize};

use crate::AppConfig;

/// ALPN of the control protocol.
pub const CONTROL_ALPN: &[u8] = b"meshcast/control/1";
/// Largest accepted frame (events are tiny; this bounds memory on the server).
pub const MAX_FRAME: usize = 16 * 1024;
/// Protocol version carried in `Hello`.
pub const CONTROL_VERSION: u16 = 1;
/// Viewer must send `Hello` within this long after connecting.
pub const HELLO_TIMEOUT_SECS: u64 = 5;
/// Idle controller (no events) is auto-revoked after this long.
pub const IDLE_REVOKE_SECS: u64 = 600;
/// Events per second accepted per controller; excess is dropped.
pub const MAX_EVENTS_PER_SEC: u32 = 500;

/// Pointer buttons.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum PointerButton {
    Left,
    Middle,
    Right,
    Back,
    Forward,
}

/// Platform-neutral non-text keys. Printable characters travel as
/// [`ControlMsg::Text`] instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum NamedKey {
    Escape,
    Enter,
    Tab,
    Backspace,
    Delete,
    Insert,
    Space,
    ArrowLeft,
    ArrowRight,
    ArrowUp,
    ArrowDown,
    Home,
    End,
    PageUp,
    PageDown,
    Shift,
    Control,
    Alt,
    Super,
    CapsLock,
    F1,
    F2,
    F3,
    F4,
    F5,
    F6,
    F7,
    F8,
    F9,
    F10,
    F11,
    F12,
    /// A printable key used as a *key* (with Ctrl/Alt/Super held), e.g. the
    /// `c` in Ctrl+C. Plain typing travels as [`ControlMsg::Text`] instead.
    Char(char),
}

/// Messages on the control stream (both directions).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ControlMsg {
    /// viewer → streamer, must be first.
    Hello {
        version: u16,
        token: String,
    },
    /// streamer → viewer: accepted; size of the captured surface in pixels.
    Welcome {
        width: u32,
        height: u32,
    },
    /// streamer → viewer: rejected (bad token, already taken, disabled).
    Denied {
        reason: String,
    },
    /// streamer → viewer: control was revoked; connection closes after this.
    Revoked {
        reason: String,
    },
    /// viewer → streamer: pointer position as a fraction (0..1) of the video frame.
    PointerMove {
        x: f32,
        y: f32,
    },
    PointerButton {
        button: PointerButton,
        pressed: bool,
    },
    /// Scroll in "lines" (positive dy = scroll down/content up, like a wheel).
    Scroll {
        dx: f32,
        dy: f32,
    },
    Key {
        key: NamedKey,
        pressed: bool,
    },
    /// Typed text (printable characters, possibly several at once).
    Text {
        text: String,
    },
    /// viewer → streamer: release everything currently held (viewer lost focus).
    Release,
    Ping,
    Pong,
}

impl ControlMsg {
    pub fn encode(&self) -> Result<Vec<u8>> {
        Ok(postcard::to_allocvec(self)?)
    }

    pub fn decode(bytes: &[u8]) -> Result<Self> {
        Ok(postcard::from_bytes(bytes)?)
    }

    /// Is this an input event (as opposed to protocol chatter)?
    pub fn is_input(&self) -> bool {
        matches!(
            self,
            ControlMsg::PointerMove { .. }
                | ControlMsg::PointerButton { .. }
                | ControlMsg::Scroll { .. }
                | ControlMsg::Key { .. }
                | ControlMsg::Text { .. }
        )
    }
}

/// Write one length-prefixed frame.
pub async fn write_frame(send: &mut SendStream, msg: &ControlMsg) -> Result<()> {
    let body = msg.encode()?;
    anyhow::ensure!(body.len() <= MAX_FRAME, "control frame too large");
    let len = (body.len() as u32).to_le_bytes();
    send.write_all(&len)
        .await
        .context("control stream closed")?;
    send.write_all(&body)
        .await
        .context("control stream closed")?;
    Ok(())
}

/// Read one frame; `Ok(None)` on clean end-of-stream.
pub async fn read_frame(recv: &mut RecvStream) -> Result<Option<ControlMsg>> {
    let mut len_buf = [0u8; 4];
    match recv.read_exact(&mut len_buf).await {
        Ok(()) => {}
        Err(iroh::endpoint::ReadExactError::FinishedEarly(0)) => return Ok(None),
        Err(e) => return Err(anyhow::anyhow!("control stream read: {e}")),
    }
    let len = u32::from_le_bytes(len_buf) as usize;
    anyhow::ensure!(len <= MAX_FRAME, "control frame too large ({len} bytes)");
    let mut body = vec![0u8; len];
    recv.read_exact(&mut body)
        .await
        .map_err(|e| anyhow::anyhow!("control stream read: {e}"))?;
    Ok(Some(ControlMsg::decode(&body)?))
}

/// Generate a one-time control token (32 random bytes, base32).
pub fn generate_token() -> String {
    let bytes: [u8; 32] = rand::random();
    data_encoding::BASE32_NOPAD.encode(&bytes)
}

/// Validate a token string's shape (prevents garbage reaching the comparison).
pub fn validate_token(token: &str) -> bool {
    token.len() == 52 && token.bytes().all(|b| b.is_ascii_alphanumeric())
}

/// A granted control token, as handed from the viewer's daemon to the viewer
/// window through a file in the config directory.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ControlGrant {
    pub ticket: String,
    pub token: String,
    /// Address of the streamer's control server (its signal endpoint).
    pub addr: EndpointAddr,
    /// Display name of the streamer, for the banner.
    #[serde(default)]
    pub streamer: String,
}

/// Directory for per-stream control grant files.
pub fn control_dir() -> PathBuf {
    AppConfig::config_dir().join("control")
}

/// Path of the grant file for a stream ticket (content-addressed so the
/// ticket never appears in a file name).
pub fn grant_path(ticket: &str) -> PathBuf {
    let hash = blake3::hash(ticket.trim().as_bytes());
    control_dir().join(format!("{}.json", &hash.to_hex()[..24]))
}

/// Write a grant for the viewer window to pick up.
pub fn write_grant(grant: &ControlGrant) -> Result<()> {
    let data = serde_json::to_vec(grant)?;
    crate::write_private_file(&grant_path(&grant.ticket), &data)
}

/// Read (and keep) the grant for a ticket, if any.
pub fn read_grant(ticket: &str) -> Option<ControlGrant> {
    let data = std::fs::read(grant_path(ticket)).ok()?;
    serde_json::from_slice(&data).ok()
}

/// Remove the grant file for a ticket.
pub fn clear_grant(ticket: &str) {
    let _ = std::fs::remove_file(grant_path(ticket));
}

/// Remove every grant file (daemon start/stop).
pub fn clear_all_grants() {
    if let Ok(entries) = std::fs::read_dir(control_dir()) {
        for e in entries.flatten() {
            let _ = std::fs::remove_file(e.path());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn control_msg_roundtrip_and_indices() {
        let msgs = vec![
            ControlMsg::Hello {
                version: 1,
                token: "T".into(),
            },
            ControlMsg::Welcome {
                width: 1920,
                height: 1080,
            },
            ControlMsg::Denied { reason: "x".into() },
            ControlMsg::Revoked { reason: "y".into() },
            ControlMsg::PointerMove { x: 0.5, y: 0.25 },
            ControlMsg::PointerButton {
                button: PointerButton::Left,
                pressed: true,
            },
            ControlMsg::Scroll { dx: 0.0, dy: -3.0 },
            ControlMsg::Key {
                key: NamedKey::Enter,
                pressed: false,
            },
            ControlMsg::Text { text: "hi".into() },
            ControlMsg::Release,
            ControlMsg::Ping,
            ControlMsg::Pong,
        ];
        for m in msgs {
            assert_eq!(ControlMsg::decode(&m.encode().unwrap()).unwrap(), m);
        }
        // Pin variant indices (wire format).
        assert_eq!(ControlMsg::Release.encode().unwrap(), vec![9]);
        assert_eq!(ControlMsg::Ping.encode().unwrap(), vec![10]);
        assert_eq!(ControlMsg::Pong.encode().unwrap(), vec![11]);
        assert!(ControlMsg::PointerMove { x: 0.0, y: 0.0 }.is_input());
        assert!(!ControlMsg::Ping.is_input());
    }

    #[test]
    fn token_shape() {
        let t = generate_token();
        assert!(validate_token(&t), "{t}");
        assert_ne!(t, generate_token());
        assert!(!validate_token("short"));
        assert!(!validate_token(&"!".repeat(52)));
    }

    #[test]
    fn grant_path_hides_ticket() {
        let p = grant_path("iroh-live:abc/meshcast");
        let name = p.file_name().unwrap().to_string_lossy().into_owned();
        assert!(name.ends_with(".json"));
        assert!(!name.contains("iroh-live"));
        assert_eq!(grant_path("iroh-live:abc/meshcast "), p); // trimmed
        assert_ne!(grant_path("iroh-live:abd/meshcast"), p);
    }
}
