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
    let read_by_helix = helix_settings::PORTAL_SCHEMAS
        .iter()
        .chain(helix_comp::input_settings::SCHEMAS.iter())
        .chain(helix_shell::shell_keys::SCHEMAS.iter())
        .chain(helix_comp::workspaces::SCHEMAS.iter())
        .chain([helix_shell::night_light::SCHEMA].iter())
        .chain(["org.gnome.desktop.wm.keybindings"].iter());
    for schema in read_by_helix {
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

/// Helix's own schemas (in `data/`) must match their contracts exactly:
/// the same keys, types and defaults.
#[test]
fn helix_schemas_match_contracts() {
    for schema in ["org.helix.shell", "org.helix.extensions"] {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(format!("../../data/{schema}.gschema.xml"));
        let xml = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()));
        let mut ours = Vec::new();
        for part in xml.split("<key ").skip(1) {
            let attr = |name: &str| part.split(&format!("{name}=\"")).nth(1).and_then(|r| r.split('"').next()).unwrap_or_default().to_owned();
            let default = part.split("<default>").nth(1).and_then(|r| r.split("</default>").next()).unwrap_or_default().to_owned();
            ours.push((attr("name"), attr("type"), default));
        }
        let path = spec_dir().join(format!("contracts/gsettings/{schema}.md"));
        let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()));
        let contract: Vec<(String, String, String)> = text
            .lines()
            .filter(|l| l.starts_with("| `"))
            .map(|l| {
                let cells: Vec<_> = l.split('|').map(|c| c.trim().trim_matches('`').to_owned()).collect();
                (cells[1].clone(), cells[2].clone(), cells[3].clone())
            })
            .collect();
        assert!(!contract.is_empty(), "no keys parsed for {schema}");
        assert_eq!(ours, contract, "{schema}: data/{schema}.gschema.xml and its contract differ");
    }
}
