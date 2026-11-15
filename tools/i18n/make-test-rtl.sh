#!/bin/sh
# Build a right-to-left test catalogue for Hebrew (`he`) from the
# template, for checking mirrored layouts and bidirectional text:
#
#   tools/i18n/make-test-rtl.sh OUTDIR
#   HELIX_LOCALEDIR=OUTDIR LANGUAGE=he LANG=he_IL.UTF-8 …
#
# Every message is written letter for letter in Hebrew script (see
# pseudo_rtl.py): it reads right to left but is not Hebrew. It is a test
# fixture only, never shipped; real translations come from translators
# (TRANSLATING.md). Dates and numbers follow LANG; if the he_IL locale
# is not installed, compile one into a private directory and point
# LOCPATH at it: localedef -i he_IL -f UTF-8 DIR/he_IL.UTF-8
set -eu
out=${1:?usage: tools/i18n/make-test-rtl.sh OUTDIR}
root=$(cd "$(dirname "$0")/../.." && pwd)
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
msgen "$root/po/helix-desktop.pot" -o "$tmp/en.po"
msgfilter --keep-header -i "$tmp/en.po" -o "$tmp/he.po" python3 "$root/tools/i18n/pseudo_rtl.py"
sed -i -e '/^#, fuzzy$/d' \
    -e 's/^"Plural-Forms: .*/"Plural-Forms: nplurals=2; plural=(n != 1);\\n"/' \
    -e 's/^"Language: .*/"Language: he\\n"/' "$tmp/he.po"
mkdir -p "$out/he/LC_MESSAGES"
msgfmt --check -o "$out/he/LC_MESSAGES/helix-desktop.mo" "$tmp/he.po"
echo "$out/he/LC_MESSAGES/helix-desktop.mo: $(msgfmt --statistics -o /dev/null "$tmp/he.po" 2>&1)"
