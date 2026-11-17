# Translating the Helix desktop

The shell (panel, quick settings, notifications, overview, lock screen
and the system dialogs it draws), the portals' own dialogs, the session
and the login screen (`helix-greeter`) share one gettext domain,
`helix-desktop`, and one template, `po/helix-desktop.pot`. A string that
several of them use ("Cancel", "Power Off", "Restart") is translated
once. Helix's applications are translated separately, in the helix-apps
repository, the same way.

## Languages

| Code | Language | Status |
| --- | --- | --- |
| `de` | German | machine-translated, unreviewed |
| `es` | Spanish | machine-translated, unreviewed |
| `fr` | French | machine-translated, unreviewed |
| `it` | Italian | machine-translated, unreviewed |
| `ja` | Japanese | machine-translated, unreviewed |
| `ko` | Korean | machine-translated, unreviewed |
| `nl` | Dutch | machine-translated, unreviewed |
| `pl` | Polish | machine-translated, unreviewed |
| `pt_BR` | Portuguese (Brazil) | machine-translated, unreviewed |
| `ru` | Russian | machine-translated, unreviewed |
| `uk` | Ukrainian | machine-translated, unreviewed |
| `zh_CN` | Chinese (Simplified) | machine-translated, unreviewed |

The first catalogues were machine-translated and shipped without review
(their header says `Last-Translator: Machine translation (unreviewed)`),
as Helix's applications' were. If you are a fluent speaker, reports of
wrong or awkward translations are welcome as issues, and so is a review
of a whole catalogue: correct `po/<lang>.po`, put your name in its
`Last-Translator` header, and change the language's status here to
"reviewed".

## Fonts

Chinese, Japanese and Korean need a font that has their characters;
Helix draws characters its interface font lacks with the font
fontconfig picks for them (`fc-match 'sans-serif:charset=<hex>'`), and
takes the face of a font collection that fontconfig chooses for the
locale's language (Noto Sans CJK SC for `zh_CN`, JP for `ja`). Install
Noto CJK: `fonts-noto-cjk` on Debian and Ubuntu (the `helix-desktop`
package suggests it), `google-noto-sans-cjk-fonts` on Fedora,
`noto-fonts-cjk` on Arch Linux. Without such a font those characters are
drawn as empty boxes. Text in these languages wraps between characters
(not only at spaces), keeping closing punctuation with the character
before it and opening brackets with the one after, as Chinese and
Japanese typesetting do; Korean wraps at its spaces.

## Adding a language

You need the GNU gettext tools (`msginit`, `msgmerge`, `msgfmt`).

1. Create the catalogue from the template, for example for German:

       msginit -i po/helix-desktop.pot -o po/de.po -l de_DE.UTF-8

   `msginit` fills in the language and its plural rule.
2. Add the language code (`de`) on its own line to `po/LINGUAS`.
3. Translate `po/de.po` with a PO editor (Poedit, GNOME Translation
   Editor, Lokalize) or a text editor.
4. Check it: `python3 tools/i18n/check-po.py de` (CI runs it for every
   language). It runs `msgfmt --check` and checks that each translation
   keeps the `{placeholders}`, `%d`-style substitutions, line breaks and,
   in date and time patterns, the same kinds of fields as the original;
   `--untranslated` lists what is left to do.

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
- In date patterns, `%B` is the month name as it is written in a date
  (the genitive in Russian, Ukrainian and Polish: "16 ноября") and `%OB`
  the month on its own ("Ноябрь", in headings). Chinese, Japanese and
  Korean may write months as numbers (`%Y年%-m月`). Keep a 12-hour
  pattern 12-hour (`%l` or `%I` with `%p`) and a 24-hour one 24-hour.
- Typography: use the real ellipsis (`…`) and your language's quotation
  marks; the original's `“{}”` becomes `„{}“`, `«{}»`, `「{}」`… The
  interface has no keyboard mnemonics (no `_` access keys).
- Keep labels of buttons and switches short: longer ones are shortened
  with an ellipsis. A quick settings tile's title gets a little smaller
  when it does not fit (and, without a subtitle, may take two lines)
  before it is shortened.
- Messages are whole sentences; none is built from translated pieces.
  If a message's grammar cannot be made right in your language, please
  report it as an issue.

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
build its catalogue with `msgfmt` into the same directory layout instead,
for example for Japanese:

    mkdir -p /tmp/helix-locale/ja/LC_MESSAGES
    msgfmt -o /tmp/helix-locale/ja/LC_MESSAGES/helix-desktop.mo po/ja.po
    localedef -i ja_JP -f UTF-8 /tmp/helix-locpath/ja_JP.UTF-8     # if ja_JP is not installed
    export HELIX_LOCALEDIR=/tmp/helix-locale LOCPATH=/tmp/helix-locpath LANGUAGE=ja LANG=ja_JP.UTF-8 HELIX_GALLERY_TAG=ja

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

## Tooling shared with helix-apps

[helixdesktop/apps](https://github.com/helixdesktop/apps) has a copy of
these tools in its own `tools/i18n/` for the apps' domains. The two copies
are kept in step by hand:

| Shared | What differs on purpose |
| --- | --- |
| `extract_rs.py` | the i18n crate it documents (`helix-i18n` / `helix_app_common::i18n`), `nc_!` (apps only), the template header |
| `update-pot.sh` | the template name; apps also extract AppStream metainfo |
| `potfiles.py` | which directories and data files are listed |
| `check-po.py` | apps also check markup, access keys and desktop keywords |
| `pseudo.py`, `make-pseudo.sh` | the template name |
| `mark_strings.py` | each repo's helper names and crate path |

`pseudo_rtl.py` and `make-test-rtl.sh` are helix-desktop's own. When you
change a shared file, make the same change in the other repository (or
open an issue there), and say so in the commit.
`tools/i18n/compare-with-apps.sh [PATH]` diffs the two copies with the
project names masked, so only real drift shows.
