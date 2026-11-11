//! The installed GNOME schemas that Helix reuses must provide every key in
//! the GSettings contracts (helix-desktop-spec/contracts/gsettings/*.md),
//! with the same type.

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
    let mut problems = Vec::new();
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
                None => problems.push(format!("{schema} {key}: missing from installed schema")),
            }
        }
    }
    assert!(problems.is_empty(), "GSettings contract mismatches:\n  {}", problems.join("\n  "));
}
