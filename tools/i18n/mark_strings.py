#!/usr/bin/env python3
"""One-off helper that wrapped the apps' user-visible string literals in
the i18n calls (tr, n_, trf!). Kept for marking new code in bulk:

    python3 tools/i18n/mark_strings.py [--dry-run] FILE.rs...

It is heuristic: it wraps literals that read like text (words, spaces,
capitals, mnemonics) and skips identifiers, icon and action names, D-Bus
and GSettings names, logging and assertion macros, comparisons and match
patterns, and unit tests. Literals in `const`/`static` items get `n_`
(the code that shows them must call `tr`). `format!` calls with text and
plain placeholders become `trf!`. Review the diff: the compiler catches
type problems, not a wrapped identifier.
"""

import re
import sys

DENY_MACROS = {
    "eprintln!", "println!", "eprint!", "print!", "panic!", "assert!", "assert_eq!",
    "assert_ne!", "debug_assert!", "debug_assert_eq!", "debug_assert_ne!", "unreachable!",
    "todo!", "unimplemented!", "write!", "writeln!", "concat!", "include_str!",
    "include_bytes!", "env!", "option_env!", "matches!", "debug!", "info!", "warn!",
    "error!", "trace!", "log!", "trf!", "trnf!", "stringify!", "compile_error!",
    "glib_debug!", "g_warning!", "g_debug!", "g_message!", "g_critical!", "cfg!",
}

# Calls whose string arguments are never shown to people.
DENY_CALLS = {
    "call", "call_future", "call_method", "call_sync", "get_property", "set_property",
    "receive_signal", "member", "bool", "str", "bytestring", "bytestrings", "get", "insert",
    "contains", "starts_with", "ends_with", "strip_prefix", "strip_suffix",
    "trim_start_matches", "trim_end_matches", "trim_matches", "split", "splitn", "rsplit",
    "rsplitn", "split_once", "rsplit_once", "replace", "replacen", "find", "rfind", "join",
    "path", "var", "var_os", "set_var", "remove_var", "env_remove", "from_str", "parse",
    "lookup", "settings", "get_string", "set_string", "get_enum_nick", "set_enum_nick",
    "add_css_class", "remove_css_class", "css_classes", "has_css_class", "css_name",
    "icon_name", "set_icon_name", "from_icon_name", "action_name", "set_action_name",
    "lookup_action", "activate_action", "set_accels_for_action", "set_widget_name",
    "widget_name", "add_named", "set_visible_child_name", "child_by_name", "expect",
    "tr", "n_", "trc", "trn", "tr_dyn", "Variant", "to_variant", "from_variant",
    "set_data", "data", "steal_data", "connect", "connect_local", "emit_by_name",
    "property", "set_property_from_value", "notify", "connect_notify", "connect_notify_local",
    "bind_property", "bind", "new_stateful", "sql", "execute", "query", "prepare",
    "eq_ignore_ascii_case", "header", "append_header", "set_header", "uri", "mime_type",
    "content_type", "from_mime_type", "get_attribute", "set_attribute", "attribute_string",
    "attribute_uint64", "attribute_boolean", "query_info", "query_info_future",
    "lookup_icon", "has_icon", "element_factory_make", "make", "ElementFactory", "by_name",
    "property_value", "set_property_from_str", "caps", "from_string", "builder_full",
    "new_for_path", "new_for_uri", "for_path", "for_uri", "parse_uri", "set_name",
    "regex", "Regex", "new_with_flags", "match_", "mime", "keyfile_string", "string",
    "locale_string", "set_locale_string", "boolean", "string_list", "has_key",
    "load_from_data", "load_from_string", "add_provider_for_display", "format_size",
    "to_str", "env", "with_name", "spawn_command_line_async", "args", "arg", "Command",
    "new_proxy", "Proxy", "for_bus", "interface", "destination", "own_name", "watch_name",
    "add_match", "property_name", "entry_text", "set_tag", "tag", "lookup_tag",
    "named", "get_named", "add_tag", "create_tag", "apply_tag_by_name",
    "remove_tag_by_name", "builder", "add_resource_path", "resources_lookup_data",
    "bytes", "open", "create", "read_to_string", "write", "save", "exists", "metadata",
    "file_name", "with_extension", "extension", "push_str", "strip", "unwrap_or_default",
    "ok_or_else", "eq", "ne", "is_type", "type_", "from_name", "lookup_type",
    "add_filter", "add_mime_type", "add_pattern", "add_suffix", "set_pattern",
    "send_request", "request", "fetch", "load_uri", "evaluate_javascript", "call_async_js_function",
    "run_javascript", "load_html", "load_bytes", "add_user_script", "UserScript",
    "UserStyleSheet", "set_accessible_role", "set_cursor_from_name", "from_name_fallback",
    "set_role", "role", "set_application_id", "application_id", "set_app_id",
    "set_default_icon_name", "set_resource_base_path", "Bytes", "rgba", "parse_color",
    "RGBA", "from_hex", "set_source_from_name", "markup_escape_text", "pipeline",
    "parse_launch", "launch", "spawn", "new_from_name", "for_name", "get_object",
    "has_classes", "is_capture_name", "set", "emit_signal", "emit", "with_bang", "select_font_face", "font_family", "set_family",
    "from_string_with_fallback", "FontDescription", "set_accels", "keyval_from_name", "accelerator_parse",
    "object", "template_child", "set_mime_type", "add_markup", "set_markup_with_mnemonic",
    # helix-desktop: D-Bus, settings and icon helpers.
    "call_method", "symbolic", "Names", "prop", "set_prop", "flag", "num", "get_prop", "fc_match",
    "load_matching", "icon", "boolean", "int", "uint", "strv", "set_strv", "child_value", "lookup_value",
    "id", "dbus_name", "has_schema", "GSettings", "schema", "Command::new", "signal", "emit_changed",
    "OwnedObjectPath", "ObjectPath", "try_from", "parse_value", "with_interface", "interface_name",
    "app_info", "DesktopAppInfo", "by_id", "starts_with_any",
}

# Calls whose arguments are always text (even single lowercase words).
TEXT_CALLS = {
    "title", "set_title", "subtitle", "set_subtitle", "label", "set_label", "tooltip_text",
    "set_tooltip_text", "placeholder_text", "set_placeholder_text", "heading", "set_heading",
    "body", "set_body", "description", "set_description", "button_label",
    "set_button_label", "with_label", "with_mnemonic", "toast", "labelled",
    "set_accessible_label", "announce", "announce_change", "announce_settled", "say",
    "accept_label", "set_accept_label", "cancel_label", "set_cancel_label",
}

KEEP_UPPER_ID = re.compile(r"^[A-Z][a-z]+(?:[A-Z][a-z0-9]+)+$")  # CamelCase identifiers
IDENT = re.compile(r"[A-Za-z_][A-Za-z0-9_]*")


FONT_NAMES = {"Sans", "Serif", "Monospace", "Cantarell", "Adwaita Sans", "Adwaita Mono",
              "DejaVu Sans", "DejaVu Sans Mono", "Noto Sans", "Sans Bold", "Monospace Bold"}


def is_text(s, call):
    t = s.strip()
    if t in FONT_NAMES or re.match(r"^(Sans|Serif|Monospace|Cantarell)( Bold| Italic)?( \d+)?$", t):
        return False
    if not t or not re.search(r"[A-Za-z]{2,}", t):
        return False
    if re.search(r"-symbolic|^(win|app|org|com|io|net|gtk|adw|ui|text|image|video|audio|application|inode|x-scheme-handler)[./]|<(Control|Primary|Shift|Alt|Super|Ctrl)>|://|^\.|^/|^@|^#|\\[nt]$|^[%$]|\\u|\bgtk-|^[a-z]+:[a-z]", t):
        return False
    if "{" in t and ("}" in t) and re.search(r"[;:]\s*[a-z-]+\s*:", t):
        return False  # CSS
    if re.match(r"^[a-z0-9_\-.:+/]+$", t):
        # identifier-like; only text in known text calls
        return call in TEXT_CALLS and " " not in t and len(t) > 2 and t.isalpha()
    if t.count("_") == 1 and re.match(r"^[A-Z][a-z]+…?$", t.replace("_", "")):
        return True  # a one-word label with a mnemonic: "_Cancel", "C_alculate"
    if " " not in t and ("/" in t or "_" in t):
        return False
    if t[0] in "<!?*[" or "Usage:" in t:
        return False
    if re.match(r"^[A-Z0-9_-]+$", t):  # UTF-8, CONSTANT_NAMES, "FN"
        return False
    if "\n" in t and ("--" in t or "Usage:" in t):
        return False
    if KEEP_UPPER_ID.match(t):
        return False
    if re.match(r"^[A-Z0-9_]+$", t):  # CONSTANT_NAMES, "FN", "VCARD"
        return False
    if re.match(r"^[A-Za-z][A-Za-z0-9]*(\.[A-Za-z0-9_-]+)+$", t):  # dotted names
        return False
    if re.match(r"^[A-Za-z0-9_-]+=[^ ]*$", t):  # key=value
        return False
    if "=" in t and " " not in t:
        return False
    if re.match(r"^[a-z]", t):
        # Messages start with a capital; lower-case phrases are mostly
        # protocol values, except in calls that only take text.
        return call in TEXT_CALLS
    if re.match(r"^[a-z][a-z0-9_]*(, ?[a-z][a-z0-9_]*)+$", t):  # sql column lists
        return False
    if re.search(r"\b(SELECT|INSERT|UPDATE|DELETE FROM|CREATE TABLE)\b", t):
        return False
    return True


def placeholders(fmt):
    """Returns list of placeholders (name, spec) or None if unsupported."""
    out = []
    i = 0
    while i < len(fmt):
        c = fmt[i]
        if fmt.startswith("{{", i) or fmt.startswith("}}", i):
            i += 2
            continue
        if c == "{":
            j = fmt.index("}", i)
            inner = fmt[i + 1:j]
            name, _, spec = inner.partition(":")
            out.append((name, spec))
            i = j + 1
            continue
        i += 1
    return out


class Frame:
    __slots__ = ("kind", "call", "arg", "start", "const")

    def __init__(self, kind, call, start, const):
        self.kind = kind
        self.call = call
        self.arg = 0
        self.start = start
        self.const = const


def find_string(s, i):
    m = re.compile(r'(b?)r(#*)"').match(s, i)
    if m:
        close = '"' + m.group(2)
        end = s.index(close, m.end())
        return (bool(m.group(1)), True, end + len(close))
    if s.startswith('b"', i) or s.startswith('"', i):
        byte = s[i] == "b"
        j = i + (2 if byte else 1)
        while s[j] != '"':
            j += 2 if s[j] == "\\" else 1
        return (byte, False, j + 1)
    return None


def test_ranges(s):
    """Spans of items under #[cfg(test)] (unit tests and fixtures)."""
    out = []
    for m in re.finditer(r"#\[cfg\((test|any\(test[^)]*\))\)\]", s):
        i = m.end()
        depth = 0
        while i < len(s):
            c = s[i]
            if c == '"' or s.startswith('r"', i) or s.startswith('r#"', i):
                st = find_string(s, i)
                if st:
                    i = st[2]
                    continue
            if s.startswith("//", i):
                i = s.find("\n", i)
                if i < 0:
                    i = len(s)
                continue
            if c == "'":
                mm = re.compile(r"'(\\.[^']*|[^\\'])'").match(s, i)
                i = mm.end() if mm else i + 1
                continue
            if c == "{":
                depth += 1
            elif c == "}":
                depth -= 1
                if depth == 0:
                    i += 1
                    break
            elif c == ";" and depth == 0:
                i += 1
                break
            i += 1
        out.append((m.start(), i))
    return out


def process(path, text, log):
    s = text
    test_at = None
    tests = test_ranges(s)
    edits = []  # (start, end, replacement)
    stack = []
    const_until_depth = []  # depth at which a const/static item ends (';')
    i = 0
    n = len(s) if test_at is None else test_at
    last_ident = None
    last_ident_end = -1
    attr_depth = None
    while i < n:
        c = s[i]
        if s.startswith("//", i):
            j = s.find("\n", i)
            i = n if j < 0 else j
            continue
        if s.startswith("/*", i):
            depth = 0
            while i < n:
                if s.startswith("/*", i):
                    depth += 1
                    i += 2
                elif s.startswith("*/", i):
                    depth -= 1
                    i += 2
                    if depth == 0:
                        break
                else:
                    i += 1
            continue
        if c == "'":
            mm = re.compile(r"'(\\.[^']*|[^\\'])'").match(s, i)
            i = mm.end() if mm else i + 1
            continue
        if c in '"rb':
            st = find_string(s, i)
            if st and (c == '"' or s[i:i + 2] in ('r"', 'r#', 'b"', 'br')):
                byte, raw, end = st
                handle_literal(s, i, end, byte, raw, stack, const_until_depth, attr_depth, edits, log, path, tests)
                i = end
                continue
        mm = IDENT.match(s, i)
        if mm and (i == 0 or not (s[i - 1].isalnum() or s[i - 1] == "_")):
            word = mm.group(0)
            j = mm.end()
            if word in ("const", "static"):
                line_start = s.rfind("\n", 0, i) + 1
                prefix = s[line_start:i].strip()
                rest = s[j:j + 40]
                if prefix in ("", "pub", "pub(crate)", "pub(super)") and re.match(r"\s+(mut\s+)?[A-Za-z_][A-Za-z0-9_]*\s*:", rest):
                    const_until_depth.append(len(stack))
            if s.startswith("!", j) and not s.startswith("!=", j):
                word += "!"
                j += 1
            last_ident = word
            last_ident_end = j
            i = mm.end() if not word.endswith("!") else j
            continue
        if last_ident and s.startswith("::<", i):
            depth = 0
            k = i + 2
            while k < n:
                if s[k] == "<":
                    depth += 1
                elif s[k] == ">":
                    depth -= 1
                    if depth == 0:
                        break
                k += 1
            i = k + 1
            last_ident_end = i
            continue
        if c == "#" and s.startswith("#[", i) or s.startswith("#![", i):
            attr_depth = len(stack)
            i += 1
            continue
        if c in "([{":
            call = None
            between = s[last_ident_end:i] if last_ident_end >= 0 else "x"
            if last_ident and between.strip() == "" or (last_ident and re.match(r"^\s*::\s*<[^>]*>\s*$", between)):
                call = last_ident
            if c != "(" and not (call or "").endswith("!"):
                call = None  # `match x {`, `Struct {`, indexing
            stack.append(Frame(c, call, i, bool(const_until_depth)))
            last_ident = None
            i += 1
            continue
        if c in ")]}":
            if stack:
                stack.pop()
            if attr_depth is not None and len(stack) == attr_depth and c == "]":
                attr_depth = None
            i += 1
            last_ident = None
            continue
        if c == ",":
            if stack:
                stack[-1].arg += 1
        if c == ";" and const_until_depth and const_until_depth[-1] == len(stack):
            const_until_depth.pop()
        if not c.isspace():
            last_ident = None
        i += 1
    edits.sort()
    out = []
    pos = 0
    for a, b, rep in edits:
        if a < pos:
            continue
        out.append(s[pos:a])
        out.append(rep)
        pos = b
    out.append(s[pos:])
    return "".join(out), len(edits)


def handle_literal(s, a, b, byte, raw, stack, const_until, attr_depth, edits, log, path, tests=()):
    if byte or attr_depth is not None:
        return
    if any(x <= a < y for x, y in tests):
        return
    lit = s[a:b]
    if raw:
        body = lit[lit.index('"') + 1:lit.rindex('"')]
    else:
        body = lit[1:-1]
    # Context checks.
    before = s[max(0, a - 12):a].rstrip()
    after = s[b:b + 12].lstrip()
    closure = re.search(r"(^|[(,=]|move)\s*\|[\w&:, ()]*\|$", s[max(0, a - 60):a].rstrip())
    if before.endswith(("==", "!=", "matches!(")) or (before.endswith("|") and not closure) \
            or after.startswith(("=>", "==", "!=", "|")):
        return
    if re.search(r"\b(case|if let|while let)\s*$", before):
        return
    rest_of_line = s[b:s.find("\n", b)]
    if re.match(r"\s*if\b.*=>", rest_of_line):
        return  # pattern with a guard
    # A literal right after `=> ` in a match arm is fine; `"x" =>` is a pattern.
    macros = [f.call for f in stack if f.call and f.call.endswith("!")]
    if any(m in DENY_MACROS for m in macros if m != "format!"):
        # format! nested in a deny macro is also denied.
        return
    frame = stack[-1] if stack else None
    call = frame.call if frame else None
    outer = call
    if call == "Some" and len(stack) >= 2:
        outer = stack[-2].call
    if outer in DENY_CALLS or call in DENY_CALLS:
        return
    in_const = bool(const_until) or any(f.const for f in stack)
    if call == "format!" and frame.arg == 0:
        convert_format(s, a, b, body, raw, stack, edits, log, path)
        return
    if call and call.endswith("!") and call not in ("vec!",):
        return
    if not is_text(body, outer):
        return
    # Tuples in const tables: keep only the first (title) elements of
    # shortcut groups; Keys/Pointer handled by call.
    if in_const:
        if outer == "Keys" and frame.arg != 0:
            return
        if outer == "Pointer" and frame.arg > 1:
            return
        title = re.match(r"\s*,\s*&\[", s[b:b + 10]) is not None
        if " " not in body.strip() and outer not in ("Keys", "Pointer") and not title:
            return
        edits.append((a, b, f"n_({lit})"))
    elif False and static_slice(s, stack):
        edits.append((a, b, f"n_({lit})"))
        line = s.count("\n", 0, a) + 1
        log.append(f"{path}:{line}: n_ in a borrowed array, translate where shown: {lit}")
    else:
        edits.append((a, b, f"tr({lit})"))


def static_slice(s, stack):
    """Whether the literal sits in a `&[...]` array literal (possibly in a
    tuple), which Rust promotes to a static only if it holds constants."""
    for f in reversed(stack):
        if f.kind == "[":
            return s[:f.start].rstrip().endswith("&")
        if f.kind == "(" and f.call is None:
            continue  # tuple
        return False
    return False


def format_is_text(body):
    """A format string reads as a message: words and spaces outside the
    placeholders and none of the punctuation of code, CSS or markup."""
    if "{{" in body or re.search(r"[;=$\\<>]|[A-Za-z0-9]\(", body):
        return False
    rest = re.sub(r"\{[^{}]*\}", "", body)
    if rest.strip() == "" or not re.search(r"[A-Za-z]{3,}", rest):
        return False
    if "{" in body and " " in body.strip():
        return not re.search(r"://|^[a-z_]+[.(]|-symbolic", body)
    return is_text(rest, None)


def convert_format(s, a, b, body, raw, stack, edits, log, path):
    if not format_is_text(body):
        return
    try:
        ph = placeholders(body)
    except ValueError:
        return
    frame = stack[-1]
    line = s.count("\n", 0, a) + 1
    if any(spec for _, spec in ph):
        log.append(f"{path}:{line}: format! with specs left as is: {s[a:b]}")
        return
    # Find the format! call extent: from 'format!(' to matching ')'.
    start = frame.start  # the '('
    mstart = s.rfind("format!", 0, start)
    depth = 0
    j = start
    while True:
        ch = s[j]
        if ch in "([{":
            depth += 1
        elif ch in ")]}":
            depth -= 1
            if depth == 0:
                break
        elif ch == '"':
            j = find_string(s, j)[2] - 1
        elif s.startswith("r#\"", j) or s.startswith('r"', j):
            j = find_string(s, j)[2] - 1
        j += 1
    end = j + 1
    args_text = s[b:j].strip()
    if args_text.startswith(","):
        args_text = args_text[1:].strip()
    args_text = args_text.rstrip(",").strip()
    names = [nm for nm, _ in ph if nm and not nm.isdigit()]
    positional = [nm for nm, _ in ph if nm == "" or nm.isdigit()]
    explicit_named = re.findall(r"(?:^|,)\s*([A-Za-z_][A-Za-z0-9_]*)\s*=(?!=)", args_text)
    lit = s[a:b]
    if names and positional:
        log.append(f"{path}:{line}: mixed placeholders left as is: {lit}")
        return
    if names:
        if args_text and not explicit_named:
            log.append(f"{path}:{line}: named + positional args left as is: {lit}")
            return
        extra = []
        for nm in dict.fromkeys(names):
            if nm not in explicit_named:
                extra.append(f"{nm} = {nm}")
        all_args = ", ".join(x for x in [args_text] + extra if x)
        rep = f"trf!({lit}, {all_args})"
    elif positional:
        if not args_text:
            return
        rep = f"trf!({lit}, {args_text})"
    else:
        rep = f"tr({lit}).to_string()"
    edits.append((mstart, end, rep))


def add_imports(path, text):
    """Adds the `use` lines for the i18n names the file now calls."""
    common = "crate" if "helix-i18n" in path else "helix_i18n"
    body = text
    m = re.search(r"#\[cfg\(test\)\]\s*mod\s+\w+\s*\{", body)
    if m:
        body = body[:m.start()]
    names = [nm for nm in ("n_", "tr") if re.search(r"(?<![A-Za-z0-9_:.])" + nm + r"\(", body)]
    lines = []
    existing = text
    if names and not re.search(r"use " + common + r"::", existing):
        if len(names) == 1:
            lines.append(f"use {common}::{names[0]};")
        else:
            lines.append(f"use {common}::{{{', '.join(names)}}};")
    if "trf!(" in body and not re.search(r"use " + common + r"::trf;", existing):
        lines.append(f"use {common}::trf;")
    if not lines:
        return text
    out = text.split("\n")
    last_use = None
    k = 0
    depth = 0
    for idx, line in enumerate(out):
        if line.startswith("use ") or line.startswith("pub use "):
            # A use may span lines; find its end.
            j = idx
            while not out[j].rstrip().endswith(";"):
                j += 1
            last_use = j
        elif line.startswith(("fn ", "pub fn", "struct ", "pub struct", "impl", "enum ", "pub enum", "const ", "pub const", "static ", "mod ", "pub mod", "#[cfg(test)]", "type ", "pub(crate)", "trait ", "pub trait", "macro_rules")):
            break
    if last_use is None:
        k = 0
        while k < len(out) and (out[k].startswith("//!") or out[k].startswith("#![") or out[k].strip() == ""):
            k += 1
        insert_at = k
        lines = lines + [""]
    else:
        insert_at = last_use + 1
    out[insert_at:insert_at] = lines
    return "\n".join(out)


def main():
    args = sys.argv[1:]
    dry = False
    if args and args[0] == "--dry-run":
        dry = True
        args = args[1:]
    log = []
    total = 0
    for p in args:
        text = open(p, encoding="utf-8").read()
        new, count = process(p, text, log)
        total += count
        if count and not dry:
            open(p, "w", encoding="utf-8").write(add_imports(p, new))
        print(f"{p}: {count}", file=sys.stderr)
    for line in log:
        print(line)
    print(f"total {total}", file=sys.stderr)


if __name__ == "__main__":
    main()
