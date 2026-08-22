#!/usr/bin/env python3
"""Meshcast tray icon for Linux.

Runs on the host desktop session (it needs the StatusNotifier D-Bus
interface, which isn't reachable from inside a toolbox/distrobox container).
It only manages two other processes:

  * `meshcast daemon`  — long-lived, talks to the Discord bot, streams
  * `meshcast-app`     — the window; disposable, opened on demand

State flows through ~/.config/meshcast/.tray-state (written by the daemon) and
commands through ~/.config/meshcast/.tray-cmd (read by the daemon).

Environment:
  MESHCAST_BIN         path to the `meshcast` CLI (default: next to this script,
                       then ~/.local/bin/meshcast, then PATH)
  MESHCAST_APP         path to `meshcast-app` (same lookup)
  MESHCAST_TOOLBOX     if set, run the binaries via `toolbox run -c <name>`
                       (only needed if you built inside a toolbox and the
                       binaries don't run on the host)
  MESHCAST_CONFIG_DIR  config dir (default $XDG_CONFIG_HOME/meshcast)

Usage: meshcast-tray.py [--show]
  --show   open the Meshcast window immediately (otherwise only the tray icon
           appears; the window opens on click or when a stream request arrives)
"""
import fcntl
import json
import os
import shutil
import signal
import subprocess
import sys
import time

import gi

gi.require_version("Gtk", "3.0")
from gi.repository import GLib, Gtk  # noqa: E402

# Ubuntu/Debian ship the Ayatana fork; Fedora/Arch ship the original.
try:
    gi.require_version("AyatanaAppIndicator3", "0.1")
    from gi.repository import AyatanaAppIndicator3 as AppIndicator3  # noqa: E402
except (ValueError, ImportError):
    gi.require_version("AppIndicator3", "0.1")
    from gi.repository import AppIndicator3  # noqa: E402

SCRIPT_DIR = os.path.dirname(os.path.abspath(__file__))
SHOW_ON_START = "--show" in sys.argv[1:]


def _find_bin(env_var, name):
    if os.environ.get(env_var):
        return os.environ[env_var]
    for candidate in (
        os.path.join(SCRIPT_DIR, name),
        os.path.expanduser(f"~/.local/bin/{name}"),
    ):
        if os.access(candidate, os.X_OK):
            return candidate
    return shutil.which(name) or name


DAEMON_BIN = _find_bin("MESHCAST_BIN", "meshcast")
APP_BIN = _find_bin("MESHCAST_APP", "meshcast-app")
TOOLBOX = os.environ.get("MESHCAST_TOOLBOX", "")

CONFIG_DIR = os.environ.get("MESHCAST_CONFIG_DIR") or os.path.join(
    os.environ.get("XDG_CONFIG_HOME") or os.path.expanduser("~/.config"), "meshcast"
)
STATE_PATH = os.path.join(CONFIG_DIR, ".tray-state")
CMD_PATH = os.path.join(CONFIG_DIR, ".tray-cmd")
APP_PID_PATH = os.path.join(CONFIG_DIR, ".app-pid")
DAEMON_PID_PATH = os.path.join(CONFIG_DIR, ".daemon-pid")

last_state = {}
had_pending = False  # whether we already opened the window for the current request


def _read_pid(pid_path):
    try:
        with open(pid_path) as f:
            return int(f.read().strip())
    except (OSError, ValueError):
        return None


def _is_pid_alive(pid_path):
    """True only if the PID is a live process *of ours* named meshcast*.

    PID files can be stale after a crash/reboot and the PID reused by something
    unrelated; a permission error means it isn't ours either.
    """
    pid = _read_pid(pid_path)
    if not pid:
        return False
    try:
        os.kill(pid, 0)
    except OSError:  # ESRCH (gone) or EPERM (not ours)
        return False
    try:
        with open(f"/proc/{pid}/comm") as f:
            return f.read().strip().startswith("meshcast")
    except OSError:
        return True


def is_app_running():
    return _is_pid_alive(APP_PID_PATH)


def is_daemon_running():
    return _is_pid_alive(DAEMON_PID_PATH)


def _cmd(binary, *args):
    if TOOLBOX:
        return ["toolbox", "run", "-c", TOOLBOX, binary, *args]
    return [binary, *args]


def _spawn(argv):
    try:
        subprocess.Popen(
            argv,
            start_new_session=True,
            stdin=subprocess.DEVNULL,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        )
    except OSError as e:
        print(f"meshcast-tray: failed to start {argv[0]}: {e}", file=sys.stderr)


_last_daemon_spawn = 0.0


def start_daemon():
    """Start the daemon if it isn't running (at most once per minute, so a
    daemon that dies instantly doesn't get respawned in a tight loop)."""
    global _last_daemon_spawn
    if is_daemon_running():
        return
    now = time.monotonic()
    if now - _last_daemon_spawn < 60:
        return
    _last_daemon_spawn = now
    _spawn(_cmd(DAEMON_BIN, "daemon"))


def show_app(_=None):
    if not is_app_running():
        _spawn(_cmd(APP_BIN))


def send_cmd(cmd):
    try:
        os.makedirs(CONFIG_DIR, exist_ok=True)
        with open(CMD_PATH, "w") as f:
            f.write(cmd)
    except OSError as e:
        print(f"meshcast-tray: failed to write command: {e}", file=sys.stderr)


def stop_stream(_=None):
    send_cmd("stop")


def quit_all(_=None):
    send_cmd("stop")
    app_pid = _read_pid(APP_PID_PATH)
    if app_pid and is_app_running():
        try:
            os.kill(app_pid, signal.SIGUSR1)
        except OSError:
            pass
    daemon_pid = _read_pid(DAEMON_PID_PATH)
    if daemon_pid and is_daemon_running():
        try:
            os.kill(daemon_pid, signal.SIGTERM)
        except OSError:
            pass
    Gtk.main_quit()


# --- Single instance ---------------------------------------------------------
# A second tray (e.g. app-menu launch while the autostarted one runs) just
# opens the window and exits.
os.makedirs(CONFIG_DIR, exist_ok=True)
_lock = open(os.path.join(CONFIG_DIR, ".tray.lock"), "w")
try:
    fcntl.flock(_lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
except OSError:
    if SHOW_ON_START:
        show_app()
    sys.exit(0)

# --- Indicator -------------------------------------------------------------

ind = AppIndicator3.Indicator.new(
    "meshcast", "network-offline", AppIndicator3.IndicatorCategory.APPLICATION_STATUS
)
ind.set_status(AppIndicator3.IndicatorStatus.ACTIVE)

menu = Gtk.Menu()
show_item = Gtk.MenuItem(label="Show Meshcast")
show_item.connect("activate", show_app)
status_item = Gtk.MenuItem(label="Starting…")
status_item.set_sensitive(False)
stop_item = Gtk.MenuItem(label="Stop Stream")
stop_item.connect("activate", stop_stream)
stop_item.set_sensitive(False)
revoke_item = Gtk.MenuItem(label="Revoke remote control")
revoke_item.connect("activate", lambda _=None: send_cmd("revoke"))
revoke_item.set_sensitive(False)
quit_item = Gtk.MenuItem(label="Quit Meshcast")
quit_item.connect("activate", quit_all)
for item in (show_item, status_item, Gtk.SeparatorMenuItem(), stop_item, revoke_item,
             Gtk.SeparatorMenuItem(), quit_item):
    menu.append(item)
menu.show_all()
ind.set_menu(menu)
# Clicking the icon (where the desktop supports it) opens the window.
ind.set_secondary_activate_target(show_item)


def update_state():
    global last_state, had_pending
    try:
        with open(STATE_PATH) as f:
            state = json.load(f)
    except (OSError, ValueError):
        state = {}

    # Auto-open the window when a new stream request arrives.
    pending = state.get("pending_request")
    if pending and not had_pending:
        had_pending = True
        show_app()
    elif not pending:
        had_pending = False

    daemon_alive = is_daemon_running()
    key = (json.dumps(state, sort_keys=True), daemon_alive)
    if key == last_state:
        return True
    last_state = key

    streaming = state.get("streaming", False)
    connected = state.get("connected", False)
    quality = state.get("quality", "")
    fps = state.get("fps", 30)
    viewers = state.get("viewers", 0)
    linked = bool(state.get("linked_servers"))
    controller = state.get("controller")

    stop_item.set_sensitive(streaming)
    revoke_item.set_sensitive(bool(controller))

    if not daemon_alive:
        icon, tip = "network-offline", "Meshcast — daemon not running"
    elif streaming:
        icon = "media-record"
        tip = f"LIVE: {quality} {fps}fps — {viewers} viewer{'s' if viewers != 1 else ''}"
        if controller:
            tip += f" — 🎮 {controller} has control"
    elif connected:
        icon, tip = "network-transmit-receive", "Meshcast — connected to Discord bot"
    elif linked:
        icon, tip = "network-idle", "Meshcast — waiting for bot"
    else:
        icon, tip = "network-offline", "Meshcast — not linked"

    ind.set_icon_full(icon, "Meshcast")
    ind.set_title(tip)
    status_item.set_label(tip)
    return True


def keep_daemon_alive():
    start_daemon()
    return True


GLib.timeout_add_seconds(2, update_state)
GLib.timeout_add_seconds(15, keep_daemon_alive)
update_state()

start_daemon()
if SHOW_ON_START:
    show_app()

Gtk.main()
