#!/usr/bin/env python3
"""Drive Lyteboard's real StatusNotifier tray through the current Plasma session bus."""

import argparse
from contextlib import ExitStack
import os
from pathlib import Path
import subprocess
import tempfile

from gi.repository import Gio, GLib

from desktop_gui import Window
from clipboard_cli import KwinSelection
from lan_cli import Device, isolated_keyring, pair, wait_for

WATCHER = "org.kde.StatusNotifierWatcher"
TYPE = "org.kde.StatusNotifierItem"
MENU = "com.canonical.dbusmenu"


def call(bus, owner, path, interface, method, signature, arguments):
    return bus.call_sync(
        owner, path, interface, method, GLib.Variant(signature, arguments),
        None, Gio.DBusCallFlags.NONE, 2000,
    ).unpack()


def property_value(bus, owner, path, interface, name):
    return call(bus, owner, path, "org.freedesktop.DBus.Properties", "Get", "(ss)", (interface, name))[0]


def registered(bus):
    return set(property_value(bus, WATCHER, "/StatusNotifierWatcher", WATCHER, "RegisteredStatusNotifierItems"))


class PlasmaTray:
    def __init__(self, bus, existing):
        self.bus = bus
        def added():
            for locator in registered(bus) - existing:
                owner, _, path = locator.partition("/")
                path = "/" + path if path else "/StatusNotifierItem"
                try:
                    if property_value(bus, owner, path, TYPE, "Id") == "io.github.arielyte.lyteboard":
                        return locator, owner, path
                except GLib.Error:
                    continue
            return None
        self.locator, self.owner, self.path = wait_for(added)
        self.menu = property_value(bus, self.owner, self.path, TYPE, "Menu")

    def item(self, label, enabled=True):
        _, root = call(self.bus, self.owner, self.menu, MENU, "GetLayout", "(iias)", (0, 1, []))
        return next((identifier for identifier, properties, _ in root[2]
                     if properties.get("label") == label
                     and (not enabled or properties.get("enabled", True))), None)

    def click(self, label):
        key = wait_for(lambda: self.item(label))
        call(self.bus, self.owner, self.menu, MENU, "Event", "(isvu)",
             (key, "clicked", GLib.Variant("i", 0), 0))

    def closed(self):
        return self.locator not in registered(self.bus)


def wait_hidden(window):
    wait_for(lambda: window.js("return !document.hasFocus() || document.hidden;"))


def wait_open(window):
    wait_for(lambda: window.js("return document.hasFocus() && !document.hidden;"))


def exercise(args, root, cleanup):
    bus = Gio.bus_get_sync(Gio.BusType.SESSION, None)
    assert property_value(bus, WATCHER, "/StatusNotifierWatcher", WATCHER, "IsStatusNotifierHostRegistered")
    original_items = registered(bus)
    env = isolated_keyring(root, cleanup)
    alpha, beta = (Device(root, name, env) for name in ("Alpha", "Beta"))
    clip = KwinSelection(root, alpha, cleanup)
    for device in (alpha, beta):
        cleanup.callback(device.stop)
        device.start()
    pair(alpha, beta)
    beta_id = alpha.devices()["Beta"][0]
    wait_for(lambda: alpha.online(beta_id))

    # Let the test window use the real Plasma panel. The daemons and clipboard remain isolated.
    gui_env = dict(alpha.env, DBUS_SESSION_BUS_ADDRESS=os.environ["DBUS_SESSION_BUS_ADDRESS"])
    window = Window(args, gui_env, cleanup)
    tray = PlasmaTray(bus, original_items)
    wait_for(lambda: tray.item("Daemon running · 1 connected", enabled=False))
    window.wait_text("Beta")
    window.click("Select online")
    clip.seed(b"Sent from the real Plasma tray")
    tray.click("Send clipboard to selected devices")
    wait_for(lambda: "Sent from the real Plasma tray" in beta.cli("received"))

    window.page("Settings")
    window.check("Beta", True)
    window.check("Automatic sending", True)
    window.check("Automatically apply received content", True)
    tray.click("Pause automatic synchronization")
    window.wait_text("Automatic synchronization is paused.")
    assert "sending off" in alpha.cli("clipboard", "sync", "status")
    assert "application off" in alpha.cli("clipboard", "auto", "status")
    assert window.checked("Automatic sending")
    assert window.checked("Automatically apply received content")
    tray.click("Resume automatic synchronization")
    wait_for(lambda: "sending on" in alpha.cli("clipboard", "sync", "status"))
    assert "application on" in alpha.cli("clipboard", "auto", "status")

    window.check("Minimize to tray", True)
    window.check("Close to tray", True)
    window.click("Minimize to tray")
    wait_hidden(window)
    tray.click("Open Lyteboard")
    wait_open(window)

    window.js("""
        window.dispatchEvent(new KeyboardEvent('keydown', {key: 'w', ctrlKey: true}));
    """)
    wait_hidden(window)
    assert alpha.cli("ping") == "Pong"
    launcher = "io.github.arielyte.lyteboard.desktop"
    subprocess.run(["desktop-file-install", "--dir=" + str(root), "--set-key=Exec",
                    '--set-value="' + str(Path(args.binary).resolve()) + '"',
                    "contrib/desktop/" + launcher], check=True, timeout=5)
    subprocess.run(["gio", "launch", str(root / launcher)], env=dict(gui_env, GDK_BACKEND="wayland"),
                   check=True, timeout=5)
    wait_open(window)
    assert not tray.closed()

    tray.click("Quit GUI")
    wait_for(tray.closed)
    window.session = None
    assert alpha.cli("ping") == "Pong"

    window = Window(args, gui_env, cleanup)
    tray = PlasmaTray(bus, original_items)
    window.page("Settings")
    assert window.checked("Minimize to tray")
    assert window.checked("Close to tray")
    tray.click("Stop daemon and quit")
    wait_for(tray.closed)
    window.session = None
    assert alpha.run("ping", check=False).returncode != 0


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--driver", default="tauri-driver")
    parser.add_argument("--webkit-driver", default="WebKitWebDriver")
    parser.add_argument("--binary", default="target/release/lyteboard-desktop")
    args = parser.parse_args()
    with tempfile.TemporaryDirectory(prefix="lyteboard-tray-") as directory, ExitStack() as cleanup:
        exercise(args, Path(directory), cleanup)
    print("Native Lyteboard tray checks passed using the Plasma host.")


if __name__ == "__main__":
    main()
