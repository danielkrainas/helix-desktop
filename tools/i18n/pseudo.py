#!/usr/bin/env python3
"""Pseudo-translation filter for testing: accents letters, keeps
placeholders ({name}), markup (<b>, &amp;), mnemonics (_) and
separators, and brackets each message so untranslated text stands out.

    tools/i18n/make-pseudo.sh OUTDIR    builds OUTDIR/en@pseudo/LC_MESSAGES/helix-desktop.mo

Used as a `msgfilter` program (reads one message on stdin).
"""

import re
import sys

ACCENTED = dict(zip(
    "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ",
    "àƀçđéƒĝĥîĵķĺɱñöþǫŕšţûṽŵẋýžÀƁÇĐÉƑĜĤÎĴĶĹṀÑÖÞǪŔŠŢÛṼŴẊÝŽ",
))
KEEP = re.compile(r"(\{[^{}]*\}|<[^>]*>|&[a-z#0-9]+;|%[-_0^#O]?[A-Za-z%]|\\n)")


def pseudo(text):
    if not text.strip():
        return text
    if text.endswith(";") and ";" in text[:-1] and " " not in text.strip():
        # desktop Keywords lists: keep the separators
        return ";".join(accent(k) for k in text.split(";"))
    return "⟦" + accent(text) + "⟧"


def accent(text):
    parts = KEEP.split(text)
    out = []
    for k, part in enumerate(parts):
        if k % 2:
            out.append(part)
        else:
            out.append("".join(ACCENTED.get(c, c) for c in part))
    return "".join(out)


if __name__ == "__main__":
    sys.stdout.write(pseudo(sys.stdin.read()))
