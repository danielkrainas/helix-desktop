# Translating the Helix desktop

The shell (panel, quick settings, notifications, overview, lock screen
and the system dialogs it draws), the portals' own dialogs, the session
and the login screen (`helix-greeter`) share one gettext domain,
`helix-desktop`, and one template, `po/helix-desktop.pot`. A string that
several of them use ("Cancel", "Power Off", "Restart") is translated
once. Helix's applications are translated separately, in the helix-apps
repository, the same way.

## Adding a language

You need the GNU gettext tools (`msginit`, `msgmerge`, `msgfmt`).

1. Create the catalogue from the template, for example for German:

       msginit -i po/helix-desktop.pot -o po/de.po -l de_DE.UTF-8

   `msginit` fills in the language and its plural rule.
2. Add the language code (`de`) on its own line to `po/LINGUAS`.
3. Translate `po/de.po` with a PO editor (Poedit, GNOME Translation
   Editor, Lokalize) or a text editor.
4. Check it: `msgfmt --check --statistics -o /dev/null po/de.po`.

`packaging/stage.sh` compiles every language listed in `po/LINGUAS` to
`/usr/share/locale/<lang>/LC_MESSAGES/helix-desktop.mo` (installed by the
`helix-desktop` package, which `helix-greeter` depends on) and merges the
translated session name into `/usr/share/wayland-sessions/helix.desktop`
(`Name[de]=…`), which login screens show.

## Updating a translation

When the sources change, regenerate the template and merge it:

    tools/i18n/update-pot.sh
    msgmerge --update --backup=none po/de.po po/helix-desktop.pot

CI runs `tools/i18n/update-pot.sh --check`, so the template in the
repository always matches the sources.

## Notes for translators

- `{name}` and `{}` are placeholders filled in by Helix. Keep them; you
  may move them. Numbered ones (`{0}`, `{1}`) can be reordered.
- Messages with `msgid_plural` need every form of your language's
  `Plural-Forms`.
- Comments that start with `Translators:` explain a message's context.
- Date and time formats (`%A, %B %e`, `%H:%M`) are GLib/strftime
  patterns: reorder or replace the fields as your language writes dates;
  the names of months and weekdays come from the system locale.
- Right-to-left languages: translate the message "left-to-right"
  (context "text direction") as `right-to-left`. The shell, lock screen,
  dialogs and login screen then lay themselves out mirrored.
- The percentage message `{}%` decides how percentages are written
  (`{} %`, `%{}`).

## Trying a translation without installing it

    mkdir -p /tmp/locale/de/LC_MESSAGES
    msgfmt -o /tmp/locale/de/LC_MESSAGES/helix-desktop.mo po/de.po
    HELIX_LOCALEDIR=/tmp/locale LANGUAGE=de LANG=de_DE.UTF-8 helix-greeter --demo

(in a nested session; see `tests/README.md`). The render galleries take
the same variables (see "Testing layouts" below).

`tools/i18n/make-pseudo.sh DIR` builds an `en@pseudo` catalogue in which
every message is accented and bracketed (`⟦Þöŵéŕ Öƒƒ⟧`); run with
`HELIX_LOCALEDIR=DIR LANGUAGE=en@pseudo` to spot text that is not marked
for translation and layouts that do not stretch.

`tools/i18n/make-test-rtl.sh DIR` builds a right-to-left *test*
catalogue (`he`): every message written letter for letter in Hebrew
script, and the direction set to right-to-left. It is a fixture for
checking mirrored layouts, never a translation, and is never installed.

## Testing layouts

The shell's, lock screen's and greeter's locale galleries draw in the
locale of the environment they run in (`HELIX_GALLERY_TAG` names the
files `locale-<tag>-…`):

    tools/i18n/make-test-rtl.sh /tmp/helix-locale
    localedef -i he_IL -f UTF-8 /tmp/helix-locpath/he_IL.UTF-8   # if he_IL is not installed
    export HELIX_LOCALEDIR=/tmp/helix-locale LOCPATH=/tmp/helix-locpath LANGUAGE=he LANG=he_IL.UTF-8 HELIX_GALLERY_TAG=he
    HELIX_SHELL_RENDER_DIR=/tmp/renders HELIX_LOCK_RENDER_DIR=/tmp/renders \
        cargo test -p helix-shell --lib locale -- --ignored --test-threads=1
    HELIX_GREETER_PNG_DIR=/tmp/renders cargo test -p helix-greeter locale -- --ignored

(`ui::gallery::locale`, `ui::lock::tests::locale_gallery` and the
greeter's `paint::tests::locale`.) For a language with a translation,
build its catalogue with `msgfmt` into the same directory layout instead.

## For developers

Mark every user-visible string, including accessible names and
announcements, with the functions of the `helix-i18n` crate
(`crates/helix-i18n/src/lib.rs`):

| Call | Use |
| --- | --- |
| `tr("Text")` | A literal; returns `&'static str`. |
| `trf!("Open {name}", name = n)` | A message with placeholders. |
| `trnf!("{n} update", "{n} updates", count)` | A count; never build plurals with `if n == 1`. |
| `trc("context", "Text")` | Short text that needs a different translation elsewhere. |
| `n_("Text")` | Marks text in a `const`/`static` table; call `tr` where it is shown. |

Every program calls `helix_i18n::init()` first thing in `main` (the
greeter calls `helix_i18n::adopt_system_locale()` before it). Write whole
sentences with placeholders rather than gluing pieces together; use
`helix_i18n::format_number`, `format_percent`, `format_size`,
`month_name`, `weekday_name` and translated date patterns instead of
hand-written names and formats. Log messages, D-Bus names and errors
meant for developers are not translated.

`tools/i18n/update-pot.sh` regenerates `po/POTFILES` and the template;
`tools/i18n/mark_strings.py` wraps literals in bulk (review its diff).
