#!/bin/sh
# Regenerate po/POTFILES and the translation template po/helix-desktop.pot.
#
#   tools/i18n/update-pot.sh           rewrite both files
#   tools/i18n/update-pot.sh --check   fail if either is out of date (CI)
#
# Rust sources go through tools/i18n/extract_rs.py (GNU xgettext before
# 0.24 cannot parse Rust); the session's desktop file through xgettext,
# which knows its translatable keys; polkit policies through xgettext with
# the ITS rules in tools/i18n/its. Needs python3 and the GNU gettext
# tools. The same scheme as helix-apps' tools/i18n.
set -eu

root=$(cd "$(dirname "$0")/../.." && pwd)
cd "$root"
check=false
[ "${1:-}" = "--check" ] && check=true

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

python3 tools/i18n/potfiles.py > "$tmp/POTFILES"
grep -v '^#' "$tmp/POTFILES" > "$tmp/list"
grep '\.rs$' "$tmp/list" > "$tmp/rs" || true
grep '\.desktop$' "$tmp/list" > "$tmp/desktop" || true
grep '\.policy$' "$tmp/list" > "$tmp/policy" || true

# shellcheck disable=SC2046
python3 tools/i18n/extract_rs.py "$tmp/rs.pot" $(cat "$tmp/rs")
xgettext --language=Desktop --from-code=UTF-8 --add-location=file \
    -k -kName -kGenericName -kComment -kKeywords \
    -o "$tmp/desktop.pot" -f "$tmp/desktop"
GETTEXTDATADIRS="$root/tools/i18n" xgettext --from-code=UTF-8 --add-location=file \
    -o "$tmp/policy.pot" -f "$tmp/policy"
# Rust messages keep line numbers (they help find the context); desktop
# and policy ones are file-only, so regenerating them after an unrelated
# edit does not churn.
msgcat --use-first --sort-by-file "$tmp/rs.pot" "$tmp/desktop.pot" "$tmp/policy.pot" \
    | sed '/^"POT-Creation-Date:/d' > "$tmp/helix-desktop.pot"

if $check; then
    status=0
    cmp -s "$tmp/POTFILES" po/POTFILES || { echo "po/POTFILES is out of date; run tools/i18n/update-pot.sh" >&2; status=1; }
    # Compare the messages only: line numbers move with every edit, and
    # gettext versions wrap long lines differently.
    strip() { msgcat --no-wrap --no-location "$1"; }
    strip "$tmp/helix-desktop.pot" > "$tmp/new"
    strip po/helix-desktop.pot > "$tmp/old"
    cmp -s "$tmp/new" "$tmp/old" || { echo "po/helix-desktop.pot is out of date; run tools/i18n/update-pot.sh" >&2; diff "$tmp/old" "$tmp/new" | head -40 >&2; status=1; }
    exit $status
fi

cp "$tmp/POTFILES" po/POTFILES
cp "$tmp/helix-desktop.pot" po/helix-desktop.pot
echo "po/helix-desktop.pot: $(grep -c '^msgid ' po/helix-desktop.pot) messages from $(wc -l < "$tmp/list") files"
