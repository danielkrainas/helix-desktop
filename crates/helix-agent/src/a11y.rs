//! The *Control apps* commands (helixdesktop/spec
//! `contracts/agent-access/cli.md`; helixdesktop/desktop#149): `a11y tree`,
//! `find`, `wait`, `do`, `press`, `focus`, `select` and `set`. The shell
//! reads and uses the app's controls; the CLI never talks to the
//! accessibility bus. `tree` comes from the shell as a flat list of nodes
//! (D-Bus nests only so deep) and is printed as a tree.

use std::collections::HashMap;

use helix_i18n::{tr, trf};
use serde_json::{Map, Value as Json};
use zbus::zvariant::{OwnedValue, Value};

use crate::errors::Failure;
use crate::read::{call_timeout, millis, own_timeout, window_id};
use crate::{cli, json, Done};

type Record = HashMap<String, OwnedValue>;

/// `a11y wait`'s `--timeout` default (seconds).
pub const WAIT_TIMEOUT: f64 = 60.0;

/// Run the command at `path` (`"a11y press"`).
pub fn run(p: &cli::Parsed, path: &str) -> Result<Done, Failure> {
    let timeout = p.timeout.unwrap_or(crate::DEFAULT_TIMEOUT);
    let mut options = crate::options(p);
    let window = window_id(&p.args[0])?;
    let done = || Ok(Done { json: serde_json::json!({}), text: String::new() });
    match path {
        "a11y tree" => {
            for (name, key) in [("depth", "depth"), ("max-nodes", "max-nodes")] {
                if let Some(n) = p.opt(name) {
                    let value = n.parse::<u32>().ok().filter(|v| key == "depth" || *v > 0).ok_or_else(|| Failure::usage(trf!("“{value}” is not a number for --{option}", value = n, option = name)))?;
                    options.insert(key, Value::from(value));
                }
            }
            if let Some(node) = p.opt("node") {
                options.insert("node", Value::from(node.to_owned()));
            }
            if p.has("bounds") {
                options.insert("bounds", Value::from(true));
            }
            let (tree,): (Record,) = crate::call_for(p, "A11yTree", &(window, options), timeout)?;
            Ok(tree_output(&tree))
        }
        "a11y find" | "a11y wait" => {
            match_options(p, &mut options)?;
            if path == "a11y wait" {
                let wait = own_timeout(p, WAIT_TIMEOUT)?;
                // 0: as long as it takes (the most the shell is asked for).
                let ms = if wait == 0.0 { u32::MAX } else { millis(wait).max(1) };
                options.insert("timeout-ms", Value::from(ms));
                options.insert("limit", Value::from(1u32));
                let (nodes,): (Vec<Record>,) = crate::call_for(p, "A11yFind", &(window, options), call_timeout(p, wait))?;
                let node = nodes.first().ok_or_else(Failure::timed_out)?;
                return Ok(Done { json: json::record(node), text: nodes_text(std::slice::from_ref(node)) });
            }
            if let Some(n) = p.opt("limit") {
                let limit = n.parse::<u32>().ok().filter(|v| *v > 0).ok_or_else(|| Failure::usage(trf!("“{value}” is not a number from 1", value = n)))?;
                options.insert("limit", Value::from(limit));
            }
            let (nodes,): (Vec<Record>,) = crate::call_for(p, "A11yFind", &(window, options), timeout)?;
            let mut out = Map::new();
            out.insert("nodes".into(), Json::Array(nodes.iter().map(json::record).collect()));
            let text = if nodes.is_empty() { tr("No matching controls.").to_owned() } else { nodes_text(&nodes) };
            Ok(Done { json: Json::Object(out), text })
        }
        "a11y do" | "a11y press" | "a11y focus" | "a11y select" => {
            let action = match path {
                "a11y do" => p.args[2].as_str(),
                "a11y press" => "default",
                "a11y focus" => "focus",
                _ => "select",
            };
            crate::call_for::<_, ()>(p, "A11yAction", &(window, p.args[1].as_str(), action, options), timeout)?;
            done()
        }
        "a11y set" => {
            let raw = p.args[2].as_str();
            let value = if p.has("number") {
                let n = raw.parse::<f64>().ok().filter(|n| n.is_finite()).ok_or_else(|| Failure::usage(trf!("“{value}” is not a number", value = raw)))?;
                Value::from(n)
            } else {
                Value::from(raw.to_owned())
            };
            crate::call_for::<_, ()>(p, "A11ySetValue", &(window, p.args[1].as_str(), value, options), timeout)?;
            done()
        }
        _ => Err(Failure::not_supported()),
    }
}

/// `--role`, `--name` and `--state` as `A11yFind`'s options.
fn match_options(p: &cli::Parsed, options: &mut HashMap<&'static str, Value<'static>>) -> Result<(), Failure> {
    if let Some(role) = p.opt("role") {
        options.insert("role", Value::from(role.to_owned()));
    }
    if let Some(name) = p.opt("name") {
        options.insert("name", Value::from(name.to_owned()));
    }
    let states: Vec<String> = p.opts_all("state").into_iter().map(str::to_owned).collect();
    if !states.is_empty() {
        options.insert("states", Value::from(states));
    }
    Ok(())
}

fn text(r: &Json, key: &str) -> String {
    r.get(key).and_then(Json::as_str).unwrap_or("").to_owned()
}

/// The shell's flat list as a tree: each node with `children`, without
/// `parent`, under `root`.
pub fn nest(nodes: &[Json]) -> Option<Json> {
    let mut by_parent: HashMap<String, Vec<usize>> = HashMap::new();
    for (i, n) in nodes.iter().enumerate().skip(1) {
        by_parent.entry(text(n, "parent")).or_default().push(i);
    }
    fn build(i: usize, nodes: &[Json], by_parent: &HashMap<String, Vec<usize>>, depth: usize) -> Json {
        let mut node = nodes[i].clone();
        let id = text(&node, "id");
        if let Some(map) = node.as_object_mut() {
            map.remove("parent");
            let kids: Vec<Json> = if depth < 256 { by_parent.get(&id).map(|k| k.iter().map(|c| build(*c, nodes, by_parent, depth + 1)).collect()).unwrap_or_default() } else { Vec::new() };
            map.insert("children".into(), Json::Array(kids));
        }
        node
    }
    (!nodes.is_empty()).then(|| build(0, nodes, &by_parent, 0))
}

/// `a11y tree`'s output: JSON `{"window", "app-id", "truncated", "root"}`,
/// text an indented outline.
fn tree_output(tree: &Record) -> Done {
    let all = json::record(tree);
    let nodes: Vec<Json> = all.get("nodes").and_then(Json::as_array).cloned().unwrap_or_default();
    let mut out = Map::new();
    for key in ["window", "app-id", "truncated"] {
        if let Some(v) = all.get(key) {
            out.insert(key.into(), v.clone());
        }
    }
    let root = nest(&nodes).unwrap_or(Json::Null);
    let mut lines = Vec::new();
    outline(&root, 0, &mut lines);
    if all.get("truncated").and_then(Json::as_bool) == Some(true) {
        lines.push(tr("(Some controls were left out: use --node, --depth or --max-nodes.)").to_owned());
    }
    out.insert("root".into(), root);
    Done { json: Json::Object(out), text: lines.join("\n") }
}

/// One node as a line: role, name, value, states that matter, actions, id.
fn line(n: &Json) -> String {
    let mut parts = vec![text(n, "role")];
    let name = text(n, "name");
    if !name.is_empty() {
        parts.push(format!("“{name}”"));
    }
    if n.get("password").and_then(Json::as_bool) == Some(true) {
        parts.push(tr("(password field)").to_owned());
    } else if let Some(v) = n.get("value") {
        let shown = match v {
            Json::String(s) => format!("“{}”", s.replace('\n', "⏎")),
            other => other.to_string(),
        };
        parts.push(format!("= {shown}"));
    }
    let states: Vec<&str> = n.get("states").and_then(Json::as_array).map(|s| s.iter().filter_map(Json::as_str).filter(|s| matches!(*s, "focused" | "checked" | "selected" | "expanded" | "editable")).collect()).unwrap_or_default();
    if !states.is_empty() {
        parts.push(format!("[{}]", states.join(", ")));
    }
    let actions: Vec<&str> = n.get("actions").and_then(Json::as_array).map(|a| a.iter().filter_map(Json::as_str).collect()).unwrap_or_default();
    if !actions.is_empty() {
        parts.push(format!("({})", actions.join(", ")));
    }
    parts.push(format!("#{}", text(n, "id")));
    parts.join(" ")
}

fn outline(n: &Json, depth: usize, out: &mut Vec<String>) {
    if n.is_null() {
        return;
    }
    out.push(format!("{}{}", "  ".repeat(depth), line(n)));
    for c in n.get("children").and_then(Json::as_array).into_iter().flatten() {
        outline(c, depth + 1, out);
    }
}

fn nodes_text(nodes: &[Record]) -> String {
    nodes.iter().map(|n| line(&json::record(n))).collect::<Vec<_>>().join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nests_the_flat_list() {
        let nodes = vec![
            serde_json::json!({"id": "w", "parent": "", "role": "frame", "name": "Notes"}),
            serde_json::json!({"id": "p", "parent": "w", "role": "panel", "name": ""}),
            serde_json::json!({"id": "b", "parent": "p", "role": "button", "name": "OK", "actions": ["click"]}),
            serde_json::json!({"id": "t", "parent": "p", "role": "text", "name": "Title", "value": "Hi", "states": ["editable", "focused"]}),
            serde_json::json!({"id": "x", "parent": "w", "role": "password text", "name": "Password", "password": true}),
        ];
        let root = nest(&nodes).unwrap();
        assert_eq!(root["id"], "w");
        assert!(root.get("parent").is_none());
        assert_eq!(root["children"][0]["children"][1]["value"], "Hi");
        assert_eq!(root["children"][1]["id"], "x");
        let mut lines = Vec::new();
        outline(&root, 0, &mut lines);
        assert_eq!(lines[0], "frame “Notes” #w");
        assert_eq!(lines[2], "    button “OK” (click) #b");
        assert_eq!(lines[3], "    text “Title” = “Hi” [editable, focused] #t");
        assert_eq!(lines[4], "  password text “Password” (password field) #x");
        assert!(nest(&[]).is_none());
    }
}
