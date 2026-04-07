#!/usr/bin/env python3
"""Meshcast tray icon — manages the app lifecycle."""
import gi, os, signal, json, subprocess, sys

gi.require_version("Gtk", "3.0")
gi.require_version("AppIndicator3", "0.1")
from gi.repository import Gtk, AppIndicator3, GLib

# Find the app binary
APP_PATH = os.environ.get("MESHCAST_APP", os.path.expanduser("~/.local/bin/meshcast-app"))
if len(sys.argv) > 1:
    APP_PATH = sys.argv[1]

STATE_PATH = os.path.expanduser("~/.config/meshcast/.tray-state")
CMD_PATH = os.path.expanduser("~/.config/meshcast/.tray-cmd")
PID_PATH = os.path.expanduser("~/.config/meshcast/.app-pid")

app_proc = None
last_state = {}

def is_app_running():
    try:
        with open(PID_PATH) as f:
            pid = int(f.read().strip())
        os.kill(pid, 0)
        return True
    except:
        return False

def show_app(_=None):
    global app_proc
    if is_app_running():
        return
    app_proc = subprocess.Popen([APP_PATH], start_new_session=True)

def stop_stream(_=None):
    with open(CMD_PATH, "w") as f:
        f.write("stop")

def quit_app(_=None):
    if is_app_running():
        try:
            with open(PID_PATH) as f:
                pid = int(f.read().strip())
            os.kill(pid, signal.SIGUSR1)  # triggers clean shutdown in the app
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
quit_item.connect("activate", quit_app)
menu.append(show_item)
menu.append(stop_item)
menu.append(sep)
menu.append(quit_item)
menu.show_all()
ind.set_menu(menu)

def update_state():
    global last_state
    try:
        with open(STATE_PATH) as f:
            state = json.load(f)
    except:
        state = {}

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
        tip = f"LIVE: {quality} {fps}fps — {viewers} viewer{'s' if viewers != 1 else ''}"
    elif connected:
        ind.set_icon_full("network-idle", "Meshcast")
        tip = "Meshcast — Connected"
    else:
        ind.set_icon_full("dialog-information", "Meshcast")
        tip = "Meshcast — Ready"

    ind.set_title(tip)
    return True

GLib.timeout_add_seconds(2, update_state)
update_state()

# Auto-launch the app on start
show_app()

Gtk.main()
