# A fake MPRIS media player for tests (contracts/dbus/org.mpris.MediaPlayer2.Player.md):
# owns org.mpris.MediaPlayer2.<name> on DBUS_SESSION_BUS_ADDRESS and serves
# the root and Player interfaces at /org/mpris/MediaPlayer2 as its JSON
# argument describes it. Prints "ready" once it owns its name, and
# "call <Method> <arguments>" for each call it gets.
#
# {"name": "vlc", "identity": "VLC media player", "desktop-entry": "org.videolan.VLC",
#  "status": "Playing", "position": 42000000,
#  "metadata": {"title": "...", "artist": ["..."], "album": "...", "length": 200000000,
#               "trackid": "/org/videolan/vlc/track/7", "url": "file:///...", "art": "file:///..."},
#  "can": {"CanControl": true, "CanSeek": false, ...}, "serve": true}
#
# Keys left out are not served (no DesktopEntry, no Position). With "serve"
# false it owns its name and serves nothing.

import json
import os
import sys
import warnings

import gi

gi.require_version("Gio", "2.0")
from gi.repository import Gio, GLib  # noqa: E402

# (register_object is all this needs, deprecated or not.)
warnings.filterwarnings("ignore", category=DeprecationWarning)
config = json.loads(sys.argv[1])
state = {"status": config.get("status", "Stopped")}

root_props = '<property name="Identity" type="s" access="read"/>'
if "desktop-entry" in config:
    root_props += '<property name="DesktopEntry" type="s" access="read"/>'
player_props = '<property name="PlaybackStatus" type="s" access="read"/><property name="Metadata" type="a{sv}" access="read"/>'
if "position" in config:
    player_props += '<property name="Position" type="x" access="read"/>'
for can in config.get("can", {}):
    player_props += f'<property name="{can}" type="b" access="read"/>'

XML = f"""<node>
<interface name="org.mpris.MediaPlayer2">{root_props}<method name="Raise"/><method name="Quit"/></interface>
<interface name="org.mpris.MediaPlayer2.Player">
<method name="Play"/><method name="Pause"/><method name="PlayPause"/><method name="Stop"/>
<method name="Next"/><method name="Previous"/>
<method name="Seek"><arg type="x" name="Offset" direction="in"/></method>
<method name="SetPosition"><arg type="o" name="TrackId" direction="in"/><arg type="x" name="Position" direction="in"/></method>
{player_props}
</interface></node>"""


def metadata():
    m = config.get("metadata", {})
    out = {}
    if "title" in m:
        out["xesam:title"] = GLib.Variant("s", m["title"])
    if "artist" in m:
        out["xesam:artist"] = GLib.Variant("as", m["artist"])
    if "album" in m:
        out["xesam:album"] = GLib.Variant("s", m["album"])
    if "length" in m:
        out["mpris:length"] = GLib.Variant("x", m["length"])
    if "trackid" in m:
        out["mpris:trackid"] = GLib.Variant("o", m["trackid"])
    if "url" in m:
        out["xesam:url"] = GLib.Variant("s", m["url"])
    if "art" in m:
        out["mpris:artUrl"] = GLib.Variant("s", m["art"])
    return GLib.Variant("a{sv}", out)


def get_property(conn, sender, path, iface, prop):
    if prop == "Identity":
        return GLib.Variant("s", config.get("identity", ""))
    if prop == "DesktopEntry":
        return GLib.Variant("s", config["desktop-entry"])
    if prop == "PlaybackStatus":
        return GLib.Variant("s", state["status"])
    if prop == "Metadata":
        return metadata()
    if prop == "Position":
        return GLib.Variant("x", config["position"])
    return GLib.Variant("b", config["can"][prop])


def method_call(conn, sender, path, iface, method, params, invocation):
    args = " ".join(str(a) for a in params.unpack())
    print(f"call {method} {args}".rstrip(), flush=True)
    if method == "Play":
        state["status"] = "Playing"
    elif method == "Pause":
        state["status"] = "Paused"
    elif method == "Stop":
        state["status"] = "Stopped"
    elif method == "PlayPause":
        state["status"] = "Paused" if state["status"] == "Playing" else "Playing"
    invocation.return_value(None)


flags = Gio.DBusConnectionFlags.AUTHENTICATION_CLIENT | Gio.DBusConnectionFlags.MESSAGE_BUS_CONNECTION
conn = Gio.DBusConnection.new_for_address_sync(os.environ["DBUS_SESSION_BUS_ADDRESS"], flags, None, None)
if config.get("serve", True):
    node = Gio.DBusNodeInfo.new_for_xml(XML)
    for info in node.interfaces:
        conn.register_object("/org/mpris/MediaPlayer2", info, method_call, get_property, None)
loop = GLib.MainLoop()


def acquired(*_):
    print("ready", flush=True)


def lost(*_):
    loop.quit()


Gio.bus_own_name_on_connection(conn, "org.mpris.MediaPlayer2." + config["name"], Gio.BusNameOwnerFlags.NONE, acquired, lost)
loop.run()
