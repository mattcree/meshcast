#!/usr/bin/env python3
"""Meshcast tray icon — manages daemon + app lifecycle separately."""
import gi, os, signal, json, subprocess, sys

gi.require_version("Gtk", "3.0")
gi.require_version("AppIndicator3", "0.1")
from gi.repository import Gtk, AppIndicator3, GLib

# Binaries — run inside toolbox for library access (libxdo, pipewire, etc.)
TOOLBOX = os.environ.get("MESHCAST_TOOLBOX", "meshcast")
DAEMON_BIN = os.environ.get("MESHCAST_DAEMON", os.path.expanduser("~/.local/bin/meshcast"))
APP_BIN = os.environ.get("MESHCAST_APP", os.path.expanduser("~/.local/bin/meshcast-app"))
if len(sys.argv) > 1:
    DAEMON_BIN = sys.argv[1]
if len(sys.argv) > 2:
    APP_BIN = sys.argv[2]

# State files
STATE_PATH = os.path.expanduser("~/.config/meshcast/.tray-state")
CMD_PATH = os.path.expanduser("~/.config/meshcast/.tray-cmd")
APP_PID_PATH = os.path.expanduser("~/.config/meshcast/.app-pid")
DAEMON_PID_PATH = os.path.expanduser("~/.config/meshcast/.daemon-pid")

last_state = {}
had_pending = False  # track whether we already opened app for current request

def _is_pid_alive(pid_path):
    try:
        with open(pid_path) as f:
            pid = int(f.read().strip())
        os.kill(pid, 0)
        return True
    except:
        return False

def is_app_running():
    return _is_pid_alive(APP_PID_PATH)

def is_daemon_running():
    return _is_pid_alive(DAEMON_PID_PATH)

def _toolbox_cmd(binary, *args):
    """Wrap a command with toolbox run if TOOLBOX is set."""
    if TOOLBOX:
        return ["toolbox", "run", "-c", TOOLBOX, binary, *args]
    return [binary, *args]

def start_daemon():
    if is_daemon_running():
        return
    subprocess.Popen(_toolbox_cmd(DAEMON_BIN, "daemon"), start_new_session=True,
                     stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)

def show_app(_=None):
    if is_app_running():
        return
    subprocess.Popen(_toolbox_cmd(APP_BIN), start_new_session=True)

def stop_stream(_=None):
    with open(CMD_PATH, "w") as f:
        f.write("stop")

def quit_all(_=None):
    # Stop any active stream
    try:
        with open(CMD_PATH, "w") as f:
            f.write("stop")
    except:
        pass
    # Kill app window
    if is_app_running():
        try:
            with open(APP_PID_PATH) as f:
                pid = int(f.read().strip())
            os.kill(pid, signal.SIGUSR1)
        except:
            pass
    # Kill daemon
    if is_daemon_running():
        try:
            with open(DAEMON_PID_PATH) as f:
                pid = int(f.read().strip())
            os.kill(pid, signal.SIGTERM)
        except:
            pass
    Gtk.main_quit()

# Create indicator
ind = AppIndicator3.Indicator.new(
    "meshcast", "dialog-information",
    AppIndicator3.IndicatorCategory.APPLICATION_STATUS
)
ind.set_status(AppIndicator3.IndicatorStatus.ACTIVE)

# Menu
menu = Gtk.Menu()
show_item = Gtk.MenuItem(label="Show Meshcast")
show_item.connect("activate", show_app)
stop_item = Gtk.MenuItem(label="Stop Stream")
stop_item.connect("activate", stop_stream)
stop_item.set_sensitive(False)
sep = Gtk.SeparatorMenuItem()
quit_item = Gtk.MenuItem(label="Quit Meshcast")
quit_item.connect("activate", quit_all)
menu.append(show_item)
menu.append(stop_item)
menu.append(sep)
menu.append(quit_item)
menu.show_all()
ind.set_menu(menu)

def update_state():
    global last_state, had_pending
    try:
        with open(STATE_PATH) as f:
            state = json.load(f)
    except:
        state = {}

    # Auto-open app window when a new stream request arrives
    pending = state.get("pending_request")
    if pending and not had_pending:
        had_pending = True
        show_app()
    elif not pending:
        had_pending = False

    if state == last_state:
        return True
    last_state = state

    streaming = state.get("streaming", False)
    connected = state.get("connected", False)
    quality = state.get("quality", "")
    fps = state.get("fps", 30)
    viewers = state.get("viewers", 0)

    stop_item.set_sensitive(streaming)

    if streaming:
        ind.set_icon_full("media-record", "Meshcast")
        tip = f"LIVE: {quality} {fps}fps \u2014 {viewers} viewer{'s' if viewers != 1 else ''}"
    elif connected:
        ind.set_icon_full("network-idle", "Meshcast")
        tip = "Meshcast \u2014 Connected"
    else:
        ind.set_icon_full("dialog-information", "Meshcast")
        tip = "Meshcast \u2014 Ready"

    ind.set_title(tip)
    return True

GLib.timeout_add_seconds(2, update_state)
update_state()

# Start daemon (long-lived), then open app window (disposable)
start_daemon()
show_app()

Gtk.main()
