#!/usr/bin/env python3
"""Extract translatable messages from Rust sources into a PO template.

    python3 tools/i18n/extract_rs.py OUTPUT.pot FILE.rs...

Recognises the calls of the helix-i18n crate (see its module docs):

    tr("msg")   n_("msg")   tr_dyn is not extracted (its argument is not
    trc("context", "msg")   a literal)
    trn("singular", "plural", n)
    trf!("msg", ...)        trnf!("singular", "plural", n, ...)

Arguments must be plain or raw string literals. A `// Translators: …`
comment on the lines just before a call becomes a comment for
translators in the template. GNU xgettext before 0.24 has no Rust
parser, so Helix keeps this small one (the same as helix-apps'); the
session's desktop file still goes through xgettext (see update-pot.sh).
"""

import re
import sys

KEYWORDS = {
    # name: (number of string arguments, whether the first is a context)
    "tr": (1, False),
    "n_": (1, False),
    "trc": (2, True),
    "trn": (2, False),
    "trf!": (1, False),
    "trnf!": (2, False),
}

IDENT = re.compile(r"[A-Za-z_][A-Za-z0-9_]*")


def unescape(body):
    out = []
    i = 0
    while i < len(body):
        c = body[i]
        if c != "\\":
            out.append(c)
            i += 1
            continue
        n = body[i + 1]
        if n == "\n":
            # Line continuation: skip the newline and leading whitespace.
            i += 2
            while i < len(body) and body[i] in " \t\n\r":
                i += 1
            continue
        simple = {"n": "\n", "t": "\t", "r": "\r", "0": "\0", "\\": "\\", '"': '"', "'": "'"}
        if n in simple:
            out.append(simple[n])
            i += 2
        elif n == "x":
            out.append(chr(int(body[i + 2:i + 4], 16)))
            i += 4
        elif n == "u":
            end = body.index("}", i)
            out.append(chr(int(body[i + 3:end], 16)))
            i = end + 1
        else:
            raise ValueError(f"unknown escape \\{n}")
    return "".join(out)


class Lexer:
    """Just enough of Rust's lexer to find calls and string literals."""

    def __init__(self, text):
        self.s = text
        self.i = 0

    def line(self, pos):
        return self.s.count("\n", 0, pos) + 1

    def skip_ws_comments(self):
        s = self.s
        while self.i < len(s):
            if s[self.i].isspace():
                self.i += 1
            elif s.startswith("//", self.i):
                end = s.find("\n", self.i)
                self.i = len(s) if end < 0 else end
            elif s.startswith("/*", self.i):
                depth = 0
                while self.i < len(s):
                    if s.startswith("/*", self.i):
                        depth += 1
                        self.i += 2
                    elif s.startswith("*/", self.i):
                        depth -= 1
                        self.i += 2
                        if depth == 0:
                            break
                    else:
                        self.i += 1
            else:
                return

    def string_at(self, i):
        """If a string literal starts at i, returns (value, end) else None."""
        s = self.s
        m = re.compile(r'(b?)r(#*)"').match(s, i)
        if m:
            hashes = m.group(2)
            close = '"' + hashes
            end = s.index(close, m.end())
            if m.group(1):
                return None, end + len(close)
            return s[m.end():end], end + len(close)
        byte = s.startswith('b"', i)
        if s.startswith('"', i) or byte:
            j = i + (2 if byte else 1)
            while s[j] != '"':
                j += 2 if s[j] == "\\" else 1
            body = s[i + (2 if byte else 1):j]
            return (None if byte else unescape(body)), j + 1
        return None


TEST_MOD = re.compile(r"#\[cfg\(test\)\]\s*(?:pub(?:\([^)]*\))?\s+)?mod\s+\w+\s*\{")


def blank_tests(text):
    """Blanks every `#[cfg(test)] mod … { … }` block, keeping newlines so
    line numbers stay right."""
    while True:
        m = TEST_MOD.search(text)
        if not m:
            return text
        lx = Lexer(text)
        lx.i = m.end()
        depth = 1
        while lx.i < len(text) and depth:
            c = text[lx.i]
            if text.startswith(("//", "/*"), lx.i):
                lx.skip_ws_comments()
                continue
            lit = lx.string_at(lx.i) if c in "\"rb" else None
            if lit is not None:
                lx.i = lit[1]
                continue
            if c == "'":
                mm = re.compile(r"'(\\.[^']*|[^\\'])'").match(text, lx.i)
                lx.i = mm.end() if mm else lx.i + 1
                continue
            if c == "{":
                depth += 1
            elif c == "}":
                depth -= 1
            lx.i += 1
        block = text[m.start():lx.i]
        text = text[:m.start()] + re.sub(r"[^\n]", " ", block) + text[lx.i:]


def translator_comment(text, pos):
    """`// Translators:` comment lines right above the line of pos."""
    start = text.rfind("\n", 0, pos)
    lines = text[:max(start, 0)].split("\n")
    found = []
    while lines:
        line = lines.pop().strip()
        if line.startswith("//"):
            found.append(line[2:].strip())
        else:
            break
    found.reverse()
    for k, line in enumerate(found):
        if line.startswith("Translators:"):
            return " ".join(found[k:])
    return None


def extract(path, text, out):
    # Unit tests are not translated.
    text = blank_tests(text)
    lx = Lexer(text)
    s = text
    n = len(s)
    while lx.i < n:
        c = s[lx.i]
        if c == "/" and s.startswith(("//", "/*"), lx.i):
            lx.skip_ws_comments()
            continue
        lit = lx.string_at(lx.i) if c in "\"rb" else None
        if lit is not None:
            lx.i = lit[1]
            continue
        if c == "'":
            # Char literal or lifetime.
            m = re.compile(r"'(\\.[^']*|[^\\'])'").match(s, lx.i)
            lx.i = m.end() if m else lx.i + 1
            continue
        m = IDENT.match(s, lx.i)
        if not m:
            lx.i += 1
            continue
        name = m.group(0)
        start = m.start()
        lx.i = m.end()
        prev = s[start - 1] if start else " "
        if prev.isalnum() or prev == "_":
            continue
        if s.startswith("!", lx.i):
            name += "!"
            j = lx.i + 1
        else:
            j = lx.i
        if name not in KEYWORDS:
            continue
        # `fn tr(` is the definition, not a call.
        if re.search(r"\bfn\s+$", s[max(0, start - 10):start]):
            continue
        lx.i = j
        lx.skip_ws_comments()
        if not s.startswith("(", lx.i):
            continue
        lx.i += 1
        count, has_context = KEYWORDS[name]
        args = []
        for k in range(count):
            lx.skip_ws_comments()
            lit = lx.string_at(lx.i)
            if lit is None or lit[0] is None:
                break
            args.append(lit[0])
            lx.i = lit[1]
            lx.skip_ws_comments()
            if k + 1 < count:
                if not s.startswith(",", lx.i):
                    break
                lx.i += 1
        if len(args) != count:
            if name not in ("tr", "n_"):
                print(f"{path}:{lx.line(start)}: {name} without literal arguments", file=sys.stderr)
            continue
        ctx = args[0] if has_context else None
        msgs = args[1:] if has_context else args
        key = (ctx, msgs[0])
        entry = out.setdefault(key, {"plural": None, "refs": [], "comments": []})
        if len(msgs) == 2:
            entry["plural"] = msgs[1]
        entry["refs"].append(f"{path}:{lx.line(start)}")
        note = translator_comment(s, start)
        if note and note not in entry["comments"]:
            entry["comments"].append(note)


def po_quote(text):
    esc = text.replace("\\", "\\\\").replace('"', '\\"').replace("\t", "\\t")
    parts = esc.split("\n")
    if len(parts) == 1:
        return f'"{esc}"'
    lines = [p + "\\n" for p in parts[:-1]] + ([parts[-1]] if parts[-1] else [])
    return '""\n' + "\n".join(f'"{line}"' for line in lines)


def main():
    out_path, files = sys.argv[1], sys.argv[2:]
    entries = {}
    for f in files:
        with open(f, encoding="utf-8") as fh:
            extract(f, fh.read(), entries)
    with open(out_path, "w", encoding="utf-8") as o:
        o.write(
            '# Translations of the Helix desktop (shell, portals, session, greeter).\n'
            '# This file is distributed under the same license as helix-desktop (CC0-1.0).\n'
            '#\n'
            '#, fuzzy\n'
            'msgid ""\nmsgstr ""\n'
            '"Project-Id-Version: helix-desktop\\n"\n'
            '"Report-Msgid-Bugs-To: https://github.com/danielkrainas/helix-desktop/issues\\n"\n'
            '"PO-Revision-Date: YEAR-MO-DA HO:MI+ZONE\\n"\n'
            '"Last-Translator: FULL NAME <EMAIL@ADDRESS>\\n"\n'
            '"Language-Team: LANGUAGE <LL@li.org>\\n"\n'
            '"Language: \\n"\n'
            '"MIME-Version: 1.0\\n"\n'
            '"Content-Type: text/plain; charset=UTF-8\\n"\n'
            '"Content-Transfer-Encoding: 8bit\\n"\n'
            '"Plural-Forms: nplurals=INTEGER; plural=EXPRESSION;\\n"\n\n')
        for (ctx, msgid), e in entries.items():
            for c in e["comments"]:
                o.write(f"#. {c}\n")
            o.write("#: " + " ".join(e["refs"]) + "\n")
            if ctx is not None:
                o.write(f"msgctxt {po_quote(ctx)}\n")
            o.write(f"msgid {po_quote(msgid)}\n")
            if e["plural"] is not None:
                o.write(f"msgid_plural {po_quote(e['plural'])}\n")
                o.write('msgstr[0] ""\nmsgstr[1] ""\n\n')
            else:
                o.write('msgstr ""\n\n')
    print(f"{len(entries)} messages from {len(files)} files", file=sys.stderr)


if __name__ == "__main__":
    main()
