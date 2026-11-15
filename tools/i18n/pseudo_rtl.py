#!/usr/bin/env python3
"""Right-to-left test filter: writes each message's Latin letters with
Hebrew ones (a letter-for-letter stand-in, not a translation), keeping
placeholders ({name}), markup, mnemonics and digits, so a right-to-left
layout can be checked with text that a right-to-left locale would show.

    tools/i18n/make-test-rtl.sh OUTDIR    builds OUTDIR/he/LC_MESSAGES/helix-desktop.mo

Used as a `msgfilter` program (reads one message on stdin). The result
is for testing only and is never installed: Helix ships no machine-made
translations.
"""

import re
import sys

# One Hebrew letter per Latin letter, by rough sound; not a translation.
HEBREW = dict(zip(
    "abcdefghijklmnopqrstuvwxyz",
    "אבכדעפגהיזקלמנוףקרסטוווכיז",
))
KEEP = re.compile(r"(\{[^{}]*\}|<[^>]*>|&[a-z#0-9]+;|%[a-zA-Z%-]+|\\n)")


def convert(text):
    if not text.strip():
        return text
    # The text direction message (see helix-i18n is_rtl).
    if text == "left-to-right":
        return "right-to-left"
    # Date and time patterns stay as they are (the C library writes the
    # names in the locale's language).
    if re.fullmatch(r"[%A-Za-z:,. \-]*", text) and "%" in text:
        return text
    parts = KEEP.split(text)
    out = []
    for k, part in enumerate(parts):
        if k % 2:
            out.append(part)
        else:
            out.append("".join(HEBREW.get(c.lower(), c) for c in part))
    return "".join(out)


if __name__ == "__main__":
    sys.stdout.write(convert(sys.stdin.read()))
