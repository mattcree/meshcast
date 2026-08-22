//! Process helpers: PID files, liveness checks, and launching detached
//! child processes (the viewer window, the daemon).

use std::path::Path;
use std::process::{Child, Command, Stdio};

use anyhow::{Context, Result};

use crate::{validate_ticket, write_private_file};

/// Write our PID to `path`.
pub fn write_pid_file(path: &Path) -> Result<()> {
    write_private_file(path, std::process::id().to_string().as_bytes())
}

/// Read a PID from `path`, if the file exists and is well-formed.
pub fn read_pid_file(path: &Path) -> Option<u32> {
    std::fs::read_to_string(path)
        .ok()?
        .trim()
        .parse::<u32>()
        .ok()
}

/// Remove a PID file, but only if it still holds our own PID (so a newer
/// instance's file is never clobbered).
pub fn remove_own_pid_file(path: &Path) {
    if read_pid_file(path) == Some(std::process::id()) {
        let _ = std::fs::remove_file(path);
    }
}

/// Is the process recorded in the PID file at `path` still alive?
pub fn pid_file_alive(path: &Path) -> bool {
    read_pid_file(path).is_some_and(pid_alive)
}

/// Best-effort liveness check for an arbitrary PID.
#[cfg(unix)]
pub fn pid_alive(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    // kill(pid, 0) succeeds if the process exists and we may signal it;
    // EPERM means it exists but belongs to someone else — still alive.
    // SAFETY: kill with signal 0 performs no action beyond permission checks.
    let rc = unsafe { libc::kill(pid as libc::pid_t, 0) };
    if rc == 0 {
        return true;
    }
    std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

#[cfg(windows)]
pub fn pid_alive(pid: u32) -> bool {
    use windows_sys::Win32::Foundation::{CloseHandle, STILL_ACTIVE};
    use windows_sys::Win32::System::Threading::{
        GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    };
    if pid == 0 {
        return false;
    }
    // SAFETY: plain Win32 calls with a handle we close ourselves.
    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if handle.is_null() {
            return false;
        }
        let mut code: u32 = 0;
        let ok = GetExitCodeProcess(handle, &mut code);
        CloseHandle(handle);
        ok != 0 && code == STILL_ACTIVE as u32
    }
}

#[cfg(not(any(unix, windows)))]
pub fn pid_alive(_pid: u32) -> bool {
    false
}

/// Ask the process with this PID to terminate (SIGTERM on Unix, TerminateProcess on Windows).
#[cfg(unix)]
pub fn terminate(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    // SAFETY: sending a signal to a PID we read from our own PID file.
    unsafe { libc::kill(pid as libc::pid_t, libc::SIGTERM) == 0 }
}

#[cfg(windows)]
pub fn terminate(pid: u32) -> bool {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{OpenProcess, TerminateProcess, PROCESS_TERMINATE};
    if pid == 0 {
        return false;
    }
    // SAFETY: plain Win32 calls with a handle we close ourselves.
    unsafe {
        let handle = OpenProcess(PROCESS_TERMINATE, 0, pid);
        if handle.is_null() {
            return false;
        }
        let ok = TerminateProcess(handle, 0);
        CloseHandle(handle);
        ok != 0
    }
}

#[cfg(not(any(unix, windows)))]
pub fn terminate(_pid: u32) -> bool {
    false
}

/// Configure `cmd` so the child is detached from our terminal/session and
/// survives our exit.
pub fn detach(cmd: &mut Command) -> &mut Command {
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // New process group: Ctrl+C in our terminal doesn't reach the child.
        cmd.process_group(0);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        cmd.creation_flags(CREATE_NEW_PROCESS_GROUP | DETACHED_PROCESS);
    }
    cmd
}

/// Launch a viewer window (`meshcast watch <ticket>`) as a detached child.
///
/// Returns the [`Child`] so the caller can reap it and track how many viewer
/// windows are open.
pub fn launch_viewer(meshcast_bin: &Path, ticket: &str) -> Result<Child> {
    let ticket = validate_ticket(ticket)?;
    let mut cmd = Command::new(meshcast_bin);
    cmd.arg("watch").arg(ticket);
    detach(&mut cmd)
        .spawn()
        .with_context(|| format!("Failed to launch viewer ({})", meshcast_bin.display()))
}

/// Launch `meshcast daemon` as a detached child.
pub fn launch_daemon(meshcast_bin: &Path) -> Result<Child> {
    let mut cmd = Command::new(meshcast_bin);
    cmd.arg("daemon");
    detach(&mut cmd)
        .spawn()
        .with_context(|| format!("Failed to launch daemon ({})", meshcast_bin.display()))
}

/// Locate the `meshcast` CLI binary: `$MESHCAST_BIN`, then next to the current
/// executable, then on `PATH`.
pub fn find_meshcast_bin() -> std::path::PathBuf {
    if let Some(p) = std::env::var_os("MESHCAST_BIN") {
        return p.into();
    }
    let name = if cfg!(windows) {
        "meshcast.exe"
    } else {
        "meshcast"
    };
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let sibling = dir.join(name);
            if sibling.is_file() {
                return sibling;
            }
        }
    }
    name.into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn own_pid_is_alive_and_bogus_is_not() {
        assert!(pid_alive(std::process::id()));
        assert!(!pid_alive(0));
        // Very unlikely to exist; if it does, the test is still harmless.
        assert!(!pid_alive(u32::MAX - 7));
    }

    #[test]
    fn pid_file_roundtrip() {
        let dir = std::env::temp_dir().join(format!("meshcast-pid-{}", std::process::id()));
        let path = dir.join(".pid");
        assert!(read_pid_file(&path).is_none());
        write_pid_file(&path).unwrap();
        assert_eq!(read_pid_file(&path), Some(std::process::id()));
        assert!(pid_file_alive(&path));
        // Someone else's PID file is left alone.
        std::fs::write(&path, "1").unwrap();
        remove_own_pid_file(&path);
        assert!(path.exists());
        write_pid_file(&path).unwrap();
        remove_own_pid_file(&path);
        assert!(!path.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn launch_viewer_rejects_bad_ticket() {
        assert!(launch_viewer(Path::new("meshcast"), "bad ticket").is_err());
    }
}
