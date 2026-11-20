# GTK 4 windows for tests of Agent Access's synthetic input and
# screenshots (helixdesktop/desktop#150). Without arguments: an
# undecorated 400x300 window (org.example.InputTest) with, from the top, a
# 100 px button, a 40 px text field and a 40 px password field (6 px
# apart), and Ctrl+S; it prints what is done to it on standard output, one
# line each. With --vault: an undecorated 700x500 window
# (org.example.Vault) that is all red, to be blacked out.
import sys

import gi

gi.require_version("Gtk", "4.0")
gi.require_version("Gdk", "4.0")
from gi.repository import Gdk, Gtk  # noqa: E402

vault = "--vault" in sys.argv
app = Gtk.Application(application_id="org.example.Vault" if vault else "org.example.InputTest")


def say(*words):
    print(*words, flush=True)


def style(css):
    provider = Gtk.CssProvider()
    try:
        provider.load_from_string(css)
    except AttributeError:
        provider.load_from_data(css.encode(), -1)
    Gtk.StyleContext.add_provider_for_display(Gdk.Display.get_default(), provider, Gtk.STYLE_PROVIDER_PRIORITY_APPLICATION)


def activate(a):
    win = Gtk.ApplicationWindow(application=a, title="Vault" if vault else "Input Test")
    win.set_decorated(False)
    if vault:
        style("window { background: #ff0000; }")
        win.set_default_size(700, 500)
        win.present()
        say("ready")
        return
    style("window { background: #ffffff; } button { min-height: 100px; } entry { min-height: 40px; }")
    win.set_default_size(400, 300)
    box = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=6)
    press = Gtk.Button(label="Press me")
    press.set_size_request(-1, 100)
    count = [0]

    def pressed(_b):
        count[0] += 1
        say("clicked", count[0])

    press.connect("clicked", pressed)
    entry = Gtk.Entry()
    entry.set_size_request(-1, 40)
    entry.connect("changed", lambda e: say("text", e.get_text()))
    password = Gtk.PasswordEntry()
    password.set_size_request(-1, 40)
    password.connect("changed", lambda e: say("password changed"))
    for w in (press, entry, password):
        w.set_vexpand(False)
        box.append(w)
    controller = Gtk.ShortcutController()
    controller.set_propagation_phase(Gtk.PropagationPhase.CAPTURE)

    def save(*_args):
        say("saved")
        return True

    controller.add_shortcut(Gtk.Shortcut.new(Gtk.ShortcutTrigger.parse_string("<Control>s"), Gtk.CallbackAction.new(save)))
    win.add_controller(controller)
    win.set_child(box)
    win.present()
    say("ready")


app.connect("activate", activate)
app.run(sys.argv[:1])
