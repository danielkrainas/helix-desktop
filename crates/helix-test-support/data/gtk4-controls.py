# A GTK 4 window of common controls, for tests of Agent Access's
# *Control apps* methods (helixdesktop/desktop#149). It prints what is
# done to it on standard output, one line each.
import sys

import gi

gi.require_version("Gtk", "4.0")
from gi.repository import Gtk  # noqa: E402

app = Gtk.Application(application_id="org.example.A11yTest")


def say(*words):
    print(*words, flush=True)


def activate(a):
    win = Gtk.ApplicationWindow(application=a, title="A11y Test")
    win.set_default_size(420, 520)
    box = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=6, margin_top=12, margin_bottom=12, margin_start=12, margin_end=12)
    clicks = Gtk.Label(label="Clicks: 0")
    press = Gtk.Button(label="Press me")
    count = [0]

    def pressed(_b):
        count[0] += 1
        clicks.set_label(f"Clicks: {count[0]}")
        say("clicked", count[0])

    press.connect("clicked", pressed)
    name = Gtk.Entry()
    name.set_text("hello")
    name.update_property([Gtk.AccessibleProperty.LABEL], ["Name field"])
    name.connect("changed", lambda e: say("name length", len(e.get_text())))
    password = Gtk.PasswordEntry()
    password.set_text("hunter2")
    password.update_property([Gtk.AccessibleProperty.LABEL], ["Password"])
    password.connect("changed", lambda e: say("password changed"))
    pin = Gtk.Entry(visibility=False)
    pin.set_text("secret")
    pin.update_property([Gtk.AccessibleProperty.LABEL], ["PIN"])
    pin.connect("changed", lambda e: say("pin changed"))
    level = Gtk.Scale.new_with_range(Gtk.Orientation.HORIZONTAL, 0, 100, 1)
    level.update_property([Gtk.AccessibleProperty.LABEL], ["Level"])
    level.connect("value-changed", lambda s: say("level", int(s.get_value())))
    delete = Gtk.Button(label="Delete All")
    deleted = Gtk.Label(label="Nothing deleted")

    def deleting(_b):
        deleted.set_label("Everything deleted")
        say("deleted")

    delete.connect("clicked", deleting)
    for w in (clicks, press, name, password, pin, level, delete, deleted):
        box.append(w)
    win.set_child(box)
    win.present()
    say("ready")


app.connect("activate", activate)
app.run(sys.argv[:1])
