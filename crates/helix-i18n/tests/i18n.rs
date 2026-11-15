//! Catalogues are found and used, and po/POTFILES lists every source with
//! messages (the same rule as tools/i18n/potfiles.py).

use std::path::{Path, PathBuf};
use std::process::Command;

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn ident(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_'
}

/// Whether `text` calls tr/trc/trn/n_ with a literal, or trf!/trnf!.
fn has_messages(text: &str) -> bool {
    let b = text.as_bytes();
    let starts_call = |i: usize, name: &str| b[i..].starts_with(name.as_bytes()) && (i == 0 || !ident(b[i - 1]));
    for i in 0..b.len() {
        for name in ["trf!(", "trnf!("] {
            if starts_call(i, name) {
                return true;
            }
        }
        for name in ["tr(", "trc(", "trn(", "n_("] {
            if starts_call(i, name) && text[i + name.len()..].trim_start().starts_with('"') {
                return true;
            }
        }
    }
    false
}

fn walk(dir: &Path, out: &mut Vec<String>) {
    let mut entries: Vec<_> = std::fs::read_dir(dir).unwrap().flatten().collect();
    entries.sort_by_key(|e| e.file_name());
    for e in entries {
        let p = e.path();
        let name = e.file_name().to_string_lossy().to_string();
        if p.is_dir() {
            if !["tests", "target", "examples", "helix-test-support"].contains(&name.as_str()) {
                walk(&p, out);
            }
        } else if name.ends_with(".rs") {
            let rel = p.strip_prefix(root()).unwrap().to_string_lossy().replace('\\', "/");
            if has_messages(&std::fs::read_to_string(&p).unwrap()) {
                out.push(rel);
            }
        }
    }
}

#[test]
fn potfiles_lists_every_source_with_messages() {
    let mut expected = Vec::new();
    walk(&root().join("crates"), &mut expected);
    expected.sort();
    let listed = std::fs::read_to_string(root().join("po/POTFILES")).unwrap();
    let listed: Vec<String> = listed.lines().filter(|l| l.ends_with(".rs")).map(str::to_string).collect();
    let missing: Vec<_> = expected.iter().filter(|f| !listed.contains(f)).collect();
    let stale: Vec<_> = listed.iter().filter(|f| !expected.contains(f)).collect();
    assert!(missing.is_empty() && stale.is_empty(), "po/POTFILES is out of date (run tools/i18n/update-pot.sh): missing {missing:?}, stale {stale:?}");
}

const TEST_PO: &str = r#"msgid ""
msgstr ""
"Content-Type: text/plain; charset=UTF-8\n"
"Plural-Forms: nplurals=3; plural=(n==1 ? 0 : n%10>=2 && n%10<=4 && (n%100<10 || n%100>=20) ? 1 : 2);\n"

msgid "Power Off"
msgstr "Wyłącz"

msgid "{n} update"
msgid_plural "{n} updates"
msgstr[0] "{n} aktualizacja"
msgstr[1] "{n} aktualizacje"
msgstr[2] "{n} aktualizacji"

msgid "Open {name}"
msgstr "{name} otwórz"

msgctxt "text direction"
msgid "left-to-right"
msgstr "right-to-left"

msgid "{}%"
msgstr "{} %"
"#;

/// A catalogue in `HELIX_LOCALEDIR` is used, with the language's plural
/// rule and the direction its translation names.
#[test]
fn translations_are_used() {
    let dir = std::env::temp_dir().join(format!("helix-i18n-test-{}", std::process::id()));
    let messages = dir.join("pl/LC_MESSAGES");
    std::fs::create_dir_all(&messages).unwrap();
    let po = dir.join("pl.po");
    std::fs::write(&po, TEST_PO).unwrap();
    let compiled = Command::new("msgfmt").arg("-o").arg(messages.join("helix-desktop.mo")).arg(&po).status();
    if !compiled.is_ok_and(|s| s.success()) {
        eprintln!("msgfmt (GNU gettext) is not installed; skipping");
        let _ = std::fs::remove_dir_all(&dir);
        return;
    }
    // gettext ignores LANGUAGE in the C locale; any other installed one
    // will do for messages.
    let locale = ["en_US.UTF-8", "C.UTF-8"]
        .into_iter()
        .find(|l| Command::new("locale").env("LC_ALL", l).output().is_ok_and(|o| o.stderr.is_empty()))
        .unwrap_or("C.UTF-8");
    std::env::set_var("HELIX_LOCALEDIR", &dir);
    std::env::set_var("LANGUAGE", "pl");
    std::env::set_var("LC_ALL", locale);
    std::env::remove_var("HELIX_TEXT_DIRECTION");
    helix_i18n::init_for_tests();
    let translated = helix_i18n::tr("Power Off");
    let _ = std::fs::remove_dir_all(&dir);
    if translated == "Power Off" && locale.starts_with('C') {
        // glibc before 2.35 treats C.UTF-8 like C and ignores LANGUAGE.
        eprintln!("no locale in which LANGUAGE applies; skipping");
        return;
    }
    assert_eq!(translated, "Wyłącz");
    assert_eq!(helix_i18n::trn("{n} update", "{n} updates", 1), "1 aktualizacja");
    assert_eq!(helix_i18n::trn("{n} update", "{n} updates", 3), "3 aktualizacje");
    assert_eq!(helix_i18n::trn("{n} update", "{n} updates", 5), "5 aktualizacji");
    assert_eq!(helix_i18n::trf!("Open {name}", name = "x"), "x otwórz");
    assert!(helix_i18n::is_rtl(), "the catalogue names right-to-left");
    assert_eq!(helix_i18n::format_percent(45.0), "45 %");
    assert_eq!(helix_i18n::tr("Not in the catalogue"), "Not in the catalogue");
}
