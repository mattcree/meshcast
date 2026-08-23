//! File-based IPC between the daemon, the GUI window and the tray script.
//!
//! The daemon owns the **state file** (JSON [`DaemonState`]) and rewrites it
//! atomically whenever something changes. The GUI and tray only read it.
//!
//! The GUI and tray send commands by writing the **command file**; the daemon
//! polls it, reads the command and deletes the file. Commands are single-line
//! strings, see [`Command`].
//!
//! This deliberately avoids sockets so that the Python tray script (which has
//! to run on the host on immutable distros) stays trivial.

use std::path::Path;

use anyhow::{Context, Result};

use crate::{write_private_file, AppConfig, DaemonState};

/// Commands the GUI/tray can send to the daemon.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    /// Stop the active stream.
    Stop,
    /// Approve the pending stream request and start capture; `control` enables
    /// remote-control requests for this stream.
    Approve { control: bool },
    /// Decline the pending stream request.
    Reject,
    /// Re-read config and reconnect (after links changed).
    Reload,
    /// Pair with a bot using a pairing code.
    Link(String),
    /// Allow the pending remote-control request.
    Grant,
    /// Deny the pending remote-control request.
    Deny,
    /// End the current remote-control session.
    Revoke,
    /// Unknown / malformed command.
    Unknown(String),
}

impl Command {
    pub fn parse(s: &str) -> Self {
        let s = s.trim();
        match s {
            "stop" => Command::Stop,
            "approve" => Command::Approve { control: false },
            "approve:control" => Command::Approve { control: true },
            "reject" => Command::Reject,
            "reload" => Command::Reload,
            "grant" => Command::Grant,
            "deny" => Command::Deny,
            "revoke" => Command::Revoke,
            _ => match s.strip_prefix("link:") {
                Some(code) => Command::Link(code.trim().to_string()),
                None => Command::Unknown(s.to_string()),
            },
        }
    }

    pub fn to_wire(&self) -> String {
        match self {
            Command::Stop => "stop".into(),
            Command::Approve { control: false } => "approve".into(),
            Command::Approve { control: true } => "approve:control".into(),
            Command::Reject => "reject".into(),
            Command::Reload => "reload".into(),
            Command::Link(code) => format!("link:{code}"),
            Command::Grant => "grant".into(),
            Command::Deny => "deny".into(),
            Command::Revoke => "revoke".into(),
            Command::Unknown(s) => s.clone(),
        }
    }
}

/// Write the daemon state file atomically.
pub fn write_state(state: &DaemonState) -> Result<()> {
    let json = serde_json::to_vec(state)?;
    write_private_file(&AppConfig::state_path(), &json)
}

/// Read the daemon state file; returns the default state if missing/corrupt.
pub fn read_state() -> DaemonState {
    read_state_from(&AppConfig::state_path())
}

pub fn read_state_from(path: &Path) -> DaemonState {
    match std::fs::read_to_string(path) {
        Ok(data) => serde_json::from_str(&data).unwrap_or_default(),
        Err(_) => DaemonState::default(),
    }
}

/// Remove the state file (daemon exit).
pub fn clear_state() {
    let _ = std::fs::remove_file(AppConfig::state_path());
}

/// Send a command to the daemon.
pub fn send_command(cmd: &Command) -> Result<()> {
    write_private_file(&AppConfig::cmd_path(), cmd.to_wire().as_bytes())
        .context("Failed to write command file")
}

/// Read and delete the command file. Returns `None` if there is no command.
pub fn take_command() -> Option<Command> {
    take_command_from(&AppConfig::cmd_path())
}

pub fn take_command_from(path: &Path) -> Option<Command> {
    let raw = std::fs::read_to_string(path).ok()?;
    let raw = raw.trim();
    if raw.is_empty() {
        // A writer may still be filling the file; leave it for the next tick.
        return None;
    }
    let _ = std::fs::remove_file(path);
    Some(Command::parse(raw))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_parse_roundtrip() {
        for c in [
            Command::Stop,
            Command::Approve { control: false },
            Command::Approve { control: true },
            Command::Reject,
            Command::Reload,
            Command::Link("ABCD-1234".into()),
            Command::Grant,
            Command::Deny,
            Command::Revoke,
        ] {
            assert_eq!(Command::parse(&c.to_wire()), c);
        }
        assert_eq!(Command::parse(" stop\n"), Command::Stop);
        assert_eq!(Command::parse("link: XYZ "), Command::Link("XYZ".into()));
        assert_eq!(Command::parse("bogus"), Command::Unknown("bogus".into()));
    }

    #[test]
    fn take_command_consumes_file() {
        let dir = std::env::temp_dir().join(format!("meshcast-ipc-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(".tray-cmd");
        assert!(take_command_from(&path).is_none());
        std::fs::write(&path, "approve").unwrap();
        assert_eq!(
            take_command_from(&path),
            Some(Command::Approve { control: false })
        );
        assert!(!path.exists());
        std::fs::write(&path, "   ").unwrap();
        assert!(take_command_from(&path).is_none());
        assert!(path.exists(), "empty file must not be consumed");
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn state_read_tolerates_missing_and_corrupt() {
        let dir = std::env::temp_dir().join(format!("meshcast-state-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(".tray-state");
        assert!(!read_state_from(&path).streaming);
        std::fs::write(&path, "{not json").unwrap();
        assert!(!read_state_from(&path).streaming);
        std::fs::write(&path, r#"{"streaming":true,"connected":true}"#).unwrap();
        assert!(read_state_from(&path).streaming);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
