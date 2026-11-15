#!/bin/sh
# Fill in the compiled translations staged by packaging/stage.sh in an
# nfpm description: reads it on stdin, writes it to stdout with the line
# "  # @LOCALES@" replaced by one entry per catalogue (files only, as the
# rest of the contents).
#
#   sed … packaging/nfpm.yaml | packaging/nfpm-locales.sh STAGE > nfpm.yaml
set -eu
stage=${1:?usage: packaging/nfpm-locales.sh STAGE}
entries=$(mktemp)
trap 'rm -f "$entries"' EXIT
if [ -d "$stage/usr/share/locale" ]; then
    (cd "$stage" && find usr/share/locale -type f -name '*.mo' | LC_ALL=C sort) | while read -r f; do
        printf '  - src: ./%s/%s\n    dst: /%s\n' "$stage" "$f" "$f"
    done > "$entries"
fi
awk -v file="$entries" '
    $0 == "  # @LOCALES@" {
        while ((getline line < file) > 0) print line
        next
    }
    { print }
'
