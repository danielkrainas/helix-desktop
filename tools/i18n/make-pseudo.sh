#!/bin/sh
# Build a pseudo-translation of the template for testing layouts and
# finding untranslated text:
#
#   tools/i18n/make-pseudo.sh OUTDIR
#   HELIX_LOCALEDIR=OUTDIR LANGUAGE=en@pseudo LANG=en_US.UTF-8 …
#
# Every translated message shows in ⟦àççéñţéđ⟧ brackets; plain English
# text on screen is a string that is not marked for translation.
set -eu
out=${1:?usage: tools/i18n/make-pseudo.sh OUTDIR}
root=$(cd "$(dirname "$0")/../.." && pwd)
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
msgen "$root/po/helix-desktop.pot" -o "$tmp/en.po"
msgfilter --keep-header -i "$tmp/en.po" -o "$tmp/pseudo.po" python3 "$root/tools/i18n/pseudo.py"
sed -i -e '/^#, fuzzy$/d' \
    -e 's/^"Plural-Forms: .*/"Plural-Forms: nplurals=2; plural=(n != 1);\\n"/' \
    -e 's/^"Language: .*/"Language: en@pseudo\\n"/' "$tmp/pseudo.po"
mkdir -p "$out/en@pseudo/LC_MESSAGES"
msgfmt --check -o "$out/en@pseudo/LC_MESSAGES/helix-desktop.mo" "$tmp/pseudo.po"
echo "$out/en@pseudo/LC_MESSAGES/helix-desktop.mo: $(msgfmt --statistics -o /dev/null "$tmp/pseudo.po" 2>&1)"
