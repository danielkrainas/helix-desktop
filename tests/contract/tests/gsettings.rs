//! The installed GNOME schemas that Helix reuses must agree with the
//! GSettings contracts (helix-desktop-spec/contracts/gsettings/*.md).
//!
//! A key with a different type always fails. A key missing from the
//! installed schema only warns, because the contracts describe GNOME 51 and
//! development machines often have older schemas; set
//! `HELIX_STRICT_GSETTINGS=1` to make missing keys fail too (e.g. when
//! checking a GNOME 51 system).

use helix_test_support::conformance::spec_dir;

/// (key, type) rows from a contract table.
fn contract_keys(schema_id: &str) -> Vec<(String, String)> {
    let path = spec_dir().join(format!("contracts/gsettings/{schema_id}.md"));
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()));
    text.lines()
        .filter(|l| l.starts_with("| `"))
        .map(|l| {
            let cells: Vec<_> = l.split('|').map(|c| c.trim().trim_matches('`')).collect();
            (cells[1].to_owned(), cells[2].to_owned())
        })
        .collect()
}

#[test]
fn installed_schemas_match_contracts() {
    let strict = std::env::var_os("HELIX_STRICT_GSETTINGS").is_some_and(|v| v == "1");
    let mut problems = Vec::new();
    let mut missing = Vec::new();
    for schema in helix_settings::PORTAL_SCHEMAS {
        let keys = contract_keys(schema);
        assert!(!keys.is_empty(), "no keys parsed for {schema}");
        if !helix_settings::has_schema(schema) {
            eprintln!("{schema} not installed; skipping");
            continue;
        }
        for (key, ty) in keys {
            match helix_settings::key_type(schema, &key) {
                Some(found) if found == ty => {}
                Some(found) => problems.push(format!("{schema} {key}: contract {ty}, installed {found}")),
                None => missing.push(format!("{schema} {key}")),
            }
        }
    }
    if !missing.is_empty() {
        let list = missing.join("\n  ");
        if strict {
            problems.push(format!("missing from installed schemas:\n  {list}"));
        } else {
            eprintln!(
                "warning: installed schemas are older than the contract; missing keys:\n  {list}"
            );
        }
    }
    assert!(problems.is_empty(), "GSettings contract mismatches:\n  {}", problems.join("\n  "));
}
