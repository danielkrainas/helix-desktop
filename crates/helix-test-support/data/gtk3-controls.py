# A GTK 3 window (AT-SPI through ATK's bridge, found by its process on
# the accessibility bus), for tests of Agent Access's *Control apps*
# methods (helixdesktop/desktop#149). It prints what is done to it on
# standard output, one line each.
import sys

import gi

gi.require_version("Gtk", "3.0")
from gi.repository import Gtk  # noqa: E402


def say(*words):
    print(*words, flush=True)


win = Gtk.Window(title="Notes Test")
win.set_default_size(320, 200)
box = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=6)
notes = Gtk.Entry()
notes.get_accessible().set_name("Notes")
notes.connect("focus-in-event", lambda *a: say("notes focused"))
ok = Gtk.Button(label="OK")
ok.connect("clicked", lambda b: say("ok"))
box.pack_start(Gtk.Label(label="Notes"), False, False, 0)
box.pack_start(notes, False, False, 0)
box.pack_start(ok, False, False, 0)
win.add(box)
win.connect("destroy", Gtk.main_quit)
win.show_all()
ok.grab_focus()
say("ready")
Gtk.main()
