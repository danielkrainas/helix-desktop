//! Contract conformance: does an exported interface match the spec XML?
//!
//! The spec (helix-desktop-spec) is the source of truth. A check reports:
//! - members in the contract that are missing or have a different signature,
//! - members exported that are not in the contract (drift),
//! - entries in the "known unimplemented" list that are in fact implemented,
//!   so the list stays honest.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use zbus::zvariant::ObjectPath;
use zbus::Connection;
use zbus_xml::{ArgDirection, Interface, Node, PropertyAccess};

/// Locate the helix-desktop-spec checkout.
///
/// Uses `HELIX_SPEC_DIR` if set, otherwise expects the spec repository to be
/// a sibling of the helix-desktop repository.
pub fn spec_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("HELIX_SPEC_DIR") {
        return PathBuf::from(dir);
    }
    let here = Path::new(env!("CARGO_MANIFEST_DIR"));
    // crates/helix-test-support -> helix-desktop -> parent
    here.join("../../../helix-desktop-spec")
}

/// Read and parse a D-Bus contract, e.g. `"org.gnome.SessionManager.xml"`.
pub fn load_dbus_contract(file: &str) -> String {
    let path = spec_dir().join("contracts/dbus").join(file);
    std::fs::read_to_string(&path).unwrap_or_else(|e| {
        panic!(
            "cannot read contract {}: {e}\n\
             Check out helix-desktop-spec next to helix-desktop, or set HELIX_SPEC_DIR.",
            path.display()
        )
    })
}

/// Fetch the introspection XML a service exports at `path`.
pub async fn introspect(conn: &Connection, destination: &str, path: &str) -> String {
    let proxy = zbus::fdo::IntrospectableProxy::builder(conn)
        .destination(destination.to_owned())
        .expect("valid destination")
        .path(ObjectPath::try_from(path.to_owned()).expect("valid path"))
        .expect("valid path")
        .build()
        .await
        .expect("build Introspectable proxy");
    proxy.introspect().await.expect("Introspect call")
}

/// Normalized shape of one interface: member key -> signature description.
fn shape(iface: &Interface<'_>) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for m in iface.methods() {
        let (mut ins, mut outs) = (String::new(), String::new());
        for a in m.args() {
            match a.direction() {
                Some(ArgDirection::Out) => outs.push_str(&a.ty().inner().to_string()),
                _ => ins.push_str(&a.ty().inner().to_string()),
            }
        }
        out.insert(format!("method {}", m.name()), format!("({ins}) -> ({outs})"));
    }
    for s in iface.signals() {
        let args: String = s.args().iter().map(|a| a.ty().inner().to_string()).collect();
        out.insert(format!("signal {}", s.name()), format!("({args})"));
    }
    for p in iface.properties() {
        let access = match p.access() {
            PropertyAccess::Read => "read",
            PropertyAccess::Write => "write",
            PropertyAccess::ReadWrite => "readwrite",
        };
        out.insert(format!("property {}", p.name()), format!("{} {access}", p.ty().inner()));
    }
    out
}

fn find_interface<'n>(node: &'n Node<'_>, name: &str) -> Option<&'n Interface<'n>> {
    node.interfaces().iter().find(|i| i.name().as_str() == name)
}

/// Compare `actual_xml` (from `Introspect`) against `contract_xml` for
/// `interface`.
///
/// `known_unimplemented` lists contract members deliberately not implemented
/// yet, as `"method Foo"`, `"signal Bar"` or `"property Baz"`.
///
/// Returns a human-readable list of problems; empty means conformant.
pub fn check(
    interface: &str,
    contract_xml: &str,
    actual_xml: &str,
    known_unimplemented: &[&str],
) -> Vec<String> {
    let contract = Node::from_reader(contract_xml.as_bytes()).expect("parse contract XML");
    let actual = Node::from_reader(actual_xml.as_bytes()).expect("parse introspection XML");
    let Some(want) = find_interface(&contract, interface) else {
        return vec![format!("contract does not define {interface}")];
    };
    let Some(have) = find_interface(&actual, interface) else {
        return vec![format!("{interface} is not exported")];
    };
    let (want, have) = (shape(want), shape(have));
    let mut problems = Vec::new();

    for listed in known_unimplemented {
        if !want.contains_key(*listed) {
            problems.push(format!("known-unimplemented entry `{listed}` is not in the contract"));
        } else if have.contains_key(*listed) {
            problems.push(format!(
                "`{listed}` is implemented now; remove it from the known-unimplemented list"
            ));
        }
    }
    for (member, sig) in &want {
        match have.get(member) {
            Some(got) if got == sig => {}
            Some(got) => problems.push(format!("{member}: contract {sig}, exported {got}")),
            None if known_unimplemented.contains(&member.as_str()) => {}
            None => problems.push(format!("{member} {sig}: missing")),
        }
    }
    for (member, sig) in &have {
        if !want.contains_key(member) {
            problems.push(format!("{member} {sig}: exported but not in the contract"));
        }
    }
    problems
}

/// Panic with a readable report if `problems` is non-empty.
pub fn assert_conformant(interface: &str, problems: &[String]) {
    if problems.is_empty() {
        return;
    }
    let mut report = format!("{interface} does not conform to its contract:\n");
    for p in problems {
        let _ = writeln!(report, "  - {p}");
    }
    panic!("{report}");
}

#[cfg(test)]
mod tests {
    use super::*;

    const CONTRACT: &str = r#"<node><interface name="org.example.I">
        <method name="A"><arg type="s" direction="in"/><arg type="u" direction="out"/></method>
        <method name="B"/>
        <signal name="S"><arg type="o"/></signal>
        <property name="P" type="b" access="read"/>
    </interface></node>"#;

    #[test]
    fn exact_match_is_conformant() {
        assert!(check("org.example.I", CONTRACT, CONTRACT, &[]).is_empty());
    }

    #[test]
    fn reports_missing_mismatched_extra_and_stale_entries() {
        let actual = r#"<node><interface name="org.example.I">
            <method name="A"><arg type="s" direction="in"/><arg type="i" direction="out"/></method>
            <method name="Extra"/>
            <signal name="S"><arg type="o"/></signal>
            <property name="P" type="b" access="readwrite"/>
        </interface></node>"#;
        let problems = check("org.example.I", CONTRACT, actual, &["signal S"]);
        let joined = problems.join("\n");
        assert!(joined.contains("method A: contract (s) -> (u), exported (s) -> (i)"), "{joined}");
        assert!(joined.contains("method B () -> (): missing"), "{joined}");
        assert!(joined.contains("method Extra () -> (): exported but not in the contract"), "{joined}");
        assert!(joined.contains("property P: contract b read, exported b readwrite"), "{joined}");
        assert!(joined.contains("`signal S` is implemented now"), "{joined}");
        assert_eq!(problems.len(), 5, "{joined}");
    }

    #[test]
    fn known_unimplemented_members_are_allowed() {
        let actual = r#"<node><interface name="org.example.I">
            <method name="A"><arg type="s" direction="in"/><arg type="u" direction="out"/></method>
            <signal name="S"><arg type="o"/></signal>
            <property name="P" type="b" access="read"/>
        </interface></node>"#;
        assert!(check("org.example.I", CONTRACT, actual, &["method B"]).is_empty());
        let problems = check("org.example.I", CONTRACT, actual, &["method B", "method Nope"]);
        assert_eq!(problems.len(), 1);
    }
}
