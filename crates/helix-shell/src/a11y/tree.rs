//! A [`Window`] as an AccessKit tree.

use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};

use accesskit::{Action, Live, NodeId, TextPosition, TextSelection, Toggled, TreeId, TreeInfo, TreeUpdate};

use super::{Node, Range, Rect, Role, Window};

/// The window's own node.
pub const ROOT: NodeId = NodeId(1);

/// What an action on a node acts on.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Target {
    /// Where it is on its surface.
    pub bounds: Rect,
    pub range: Option<Range>,
    /// A text field: focusing it is a click too.
    pub text_input: bool,
}

/// A window's tree, and what its nodes' actions act on.
pub struct Built {
    pub update: TreeUpdate,
    pub targets: HashMap<NodeId, Target>,
}

fn is_text_input(role: Role) -> bool {
    matches!(role, Role::TextInput | Role::PasswordInput | Role::SearchInput | Role::MultilineTextInput)
}

fn bounds(r: Rect) -> accesskit::Rect {
    accesskit::Rect::new(f64::from(r.x), f64::from(r.y), f64::from(r.x + r.w), f64::from(r.y + r.h))
}

/// Ids from the window's key path: the same object keeps its id while
/// the window changes around it.
struct Ids {
    used: HashSet<u64>,
}

impl Ids {
    fn next(&mut self, parent: NodeId, key: &str) -> NodeId {
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        parent.0.hash(&mut hasher);
        key.hash(&mut hasher);
        let mut id = hasher.finish();
        while id <= ROOT.0 || !self.used.insert(id) {
            id = id.wrapping_mul(0x9E37_79B9_7F4A_7C15).wrapping_add(1);
        }
        NodeId(id)
    }
}

struct Builder {
    ids: Ids,
    nodes: Vec<(NodeId, accesskit::Node)>,
    targets: HashMap<NodeId, Target>,
    focus: Option<NodeId>,
}

impl Builder {
    fn add(&mut self, parent: NodeId, node: &Node) -> NodeId {
        let key = if node.id.is_empty() { format!("{:?}:{}{}", node.role, node.name, node.text.as_deref().unwrap_or("")) } else { node.id.clone() };
        let id = self.ids.next(parent, &key);
        let mut out = accesskit::Node::new(node.role);
        if node.role == Role::Label {
            // A label's name is its text.
            out.set_value(node.text.clone().unwrap_or_else(|| node.name.clone()));
        } else if !node.name.is_empty() {
            out.set_label(node.name.clone());
        }
        if !node.description.is_empty() {
            out.set_description(node.description.clone());
        }
        if let Some(r) = node.bounds {
            out.set_bounds(bounds(r));
        }
        if let Some(on) = node.toggled {
            out.set_toggled(if on { Toggled::True } else { Toggled::False });
        }
        if let Some(on) = node.selected {
            out.set_selected(on);
        }
        if let Some(open) = node.expanded {
            out.set_expanded(open);
        }
        if let Some(range) = node.range {
            out.set_numeric_value(range.value);
            out.set_min_numeric_value(range.min);
            out.set_max_numeric_value(range.max);
            if range.step > 0.0 {
                out.set_numeric_value_step(range.step);
            }
            if node.clickable && !node.disabled {
                out.add_action(Action::SetValue);
                out.add_action(Action::Increment);
                out.add_action(Action::Decrement);
            }
        }
        if node.disabled {
            out.set_disabled();
        }
        if node.live {
            out.set_live(Live::Polite);
        }
        if node.modal {
            out.set_modal();
        }
        if node.focusable {
            out.add_action(Action::Focus);
        }
        if node.clickable && !node.disabled && node.range.is_none() {
            out.add_action(Action::Click);
        }
        if node.focused && self.focus.is_none() {
            self.focus = Some(id);
        }
        let text_input = is_text_input(node.role);
        if node.clickable || node.focusable {
            if let Some(r) = node.bounds {
                self.targets.insert(id, Target { bounds: r, range: node.range, text_input });
            }
        }
        let mut children = Vec::new();
        if text_input {
            // The text as one run, with the caret at its end.
            let text = node.text.clone().unwrap_or_default();
            out.set_value(text.clone());
            let run = self.ids.next(id, "#text");
            let mut node_run = accesskit::Node::new(Role::TextRun);
            let lengths: Vec<u8> = text.chars().map(|c| c.len_utf8() as u8).collect();
            let end = lengths.len();
            node_run.set_value(text);
            node_run.set_character_lengths(lengths);
            if let Some(r) = node.bounds {
                node_run.set_bounds(bounds(r));
            }
            let caret = TextPosition { node: run, character_index: end };
            out.set_text_selection(TextSelection { anchor: caret, focus: caret });
            self.nodes.push((run, node_run));
            children.push(run);
        }
        for child in &node.children {
            children.push(self.add(id, child));
        }
        if !children.is_empty() {
            out.set_children(children);
        }
        self.nodes.push((id, out));
        id
    }
}

/// The tree of `window`: its node (a window named by its title) and
/// everything in it, focus on the focused node.
pub fn build(window: &Window) -> Built {
    let mut b = Builder { ids: Ids { used: HashSet::new() }, nodes: Vec::new(), targets: HashMap::new(), focus: None };
    let children: Vec<NodeId> = window.children.iter().map(|n| b.add(ROOT, n)).collect();
    let mut root = accesskit::Node::new(Role::Window);
    root.set_label(window.title.clone());
    root.set_bounds(bounds(Rect { x: 0, y: 0, ..window.bounds }));
    root.set_children(children);
    b.nodes.push((ROOT, root));
    let mut info = TreeInfo::new(ROOT);
    info.toolkit_name = Some("Helix".into());
    info.toolkit_version = Some(env!("CARGO_PKG_VERSION").into());
    Built {
        update: TreeUpdate { nodes: b.nodes, tree: Some(info), tree_id: TreeId::ROOT, focus: b.focus.unwrap_or(ROOT) },
        targets: b.targets,
    }
}

/// Where a click sets a slider at `bounds` to `value`.
pub fn slider_point(bounds: Rect, range: Range, value: f64) -> (f64, f64) {
    let span = (range.max - range.min).max(f64::EPSILON);
    let f = ((value - range.min) / span).clamp(0.0, 1.0);
    (f64::from(bounds.x) + f * f64::from(bounds.w), bounds.center().1)
}

/// The point to click for `action` on `target`; `None` when the action
/// is not one a click can do.
pub fn click_point(target: &Target, action: Action, data: Option<&accesskit::ActionData>) -> Option<(f64, f64)> {
    match (action, target.range) {
        (Action::Click, None) => Some(target.bounds.center()),
        (Action::Focus, _) if target.text_input => Some(target.bounds.center()),
        (Action::SetValue, Some(range)) => match data {
            Some(accesskit::ActionData::NumericValue(v)) => Some(slider_point(target.bounds, range, *v)),
            Some(accesskit::ActionData::Value(s)) => s.trim().parse::<f64>().ok().map(|v| slider_point(target.bounds, range, v)),
            _ => None,
        },
        (Action::Increment, Some(range)) => Some(slider_point(target.bounds, range, range.value + range.step.max((range.max - range.min) / 20.0))),
        (Action::Decrement, Some(range)) => Some(slider_point(target.bounds, range, range.value - range.step.max((range.max - range.min) / 20.0))),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(built: &Built, id: NodeId) -> &accesskit::Node {
        &built.update.nodes.iter().find(|(i, _)| *i == id).expect("node").1
    }

    fn by_label<'a>(built: &'a Built, name: &str) -> (NodeId, &'a accesskit::Node) {
        built.update.nodes.iter().find(|(_, n)| n.label() == Some(name) || (n.role() == Role::Label && n.value() == Some(name))).map(|(i, n)| (*i, n)).expect(name)
    }

    fn window(children: Vec<Node>) -> Window {
        Window { title: "Test".into(), bounds: Rect::new(0, 0, 800, 600), active: true, children }
    }

    #[test]
    fn roles_names_states() {
        let w = window(vec![
            Node::new(Role::Dialog, "Authentication Required").modal().children([
                Node::label("Password needed"),
                Node::text_field(Role::PasswordInput, "Password", Node::mask("hunter2"), Rect::new(10, 10, 200, 30)).focused(true),
                Node::new(Role::CheckBox, "Remember").at(Rect::new(10, 50, 200, 20)).click().toggled(true),
                Node::button("Cancel", Rect::new(10, 80, 90, 30)),
            ]),
        ]);
        let built = build(&w);
        let root = node(&built, ROOT);
        assert_eq!(root.role(), Role::Window);
        assert_eq!(root.label(), Some("Test"));
        let (dialog_id, dialog) = by_label(&built, "Authentication Required");
        assert_eq!(dialog.role(), Role::Dialog);
        assert!(dialog.is_modal());
        assert_eq!(root.children(), [dialog_id]);
        let (field_id, field) = by_label(&built, "Password");
        assert_eq!(field.role(), Role::PasswordInput);
        assert_eq!(field.value(), Some("•••••••"), "masked, never the password");
        assert_eq!(built.update.focus, field_id);
        // The text run holds the caret at the end.
        let run = node(&built, field.children()[0]);
        assert_eq!(run.role(), Role::TextRun);
        assert_eq!(run.character_lengths().len(), 7);
        assert_eq!(field.text_selection().map(|s| s.focus.character_index), Some(7));
        let (_, check) = by_label(&built, "Remember");
        assert_eq!(check.toggled(), Some(Toggled::True));
        assert!(check.supports_action(Action::Click));
        let (cancel_id, cancel) = by_label(&built, "Cancel");
        assert_eq!(cancel.role(), Role::Button);
        assert_eq!(built.targets[&cancel_id].bounds, Rect::new(10, 80, 90, 30));
        let (_, message) = by_label(&built, "Password needed");
        assert_eq!(message.role(), Role::Label);
        assert_eq!(built.update.tree.as_ref().map(|t| t.root), Some(ROOT));
    }

    #[test]
    fn ids_survive_changes() {
        let clock = |t: &str| Node::button(t, Rect::new(0, 0, 50, 20)).id("clock");
        let a = build(&window(vec![Node::button("Apps", Rect::default()), clock("10:00")]));
        let b = build(&window(vec![clock("10:01"), Node::button("Apps", Rect::default())]));
        assert_eq!(by_label(&a, "10:00").0, by_label(&b, "10:01").0, "the clock keeps its object");
        assert_eq!(by_label(&a, "Apps").0, by_label(&b, "Apps").0);
        // Siblings with the same key still get their own ids.
        let twice = build(&window(vec![Node::button("OK", Rect::default()), Node::button("OK", Rect::default())]));
        let ids: Vec<NodeId> = node(&twice, ROOT).children().to_vec();
        assert_eq!(ids.len(), 2);
        assert_ne!(ids[0], ids[1]);
        assert_eq!(twice.update.focus, ROOT, "nothing focused: the window");
    }

    #[test]
    fn slider_actions_click_along_the_bar() {
        let w = window(vec![Node::new(Role::Slider, "Volume").at(Rect::new(100, 0, 200, 40)).click().range(50.0, 0.0, 100.0, 5.0)]);
        let built = build(&w);
        let (id, slider) = by_label(&built, "Volume");
        assert_eq!(slider.numeric_value(), Some(50.0));
        assert!(slider.supports_action(Action::Increment) && !slider.supports_action(Action::Click));
        let t = built.targets[&id];
        assert_eq!(click_point(&t, Action::Increment, None), Some((210.0, 20.0)));
        assert_eq!(click_point(&t, Action::Decrement, None), Some((190.0, 20.0)));
        assert_eq!(click_point(&t, Action::SetValue, Some(&accesskit::ActionData::NumericValue(100.0))), Some((300.0, 20.0)));
        assert_eq!(click_point(&t, Action::SetValue, Some(&accesskit::ActionData::NumericValue(900.0))), Some((300.0, 20.0)), "clamped");
        assert_eq!(click_point(&t, Action::Click, None), None);
        let button = Target { bounds: Rect::new(0, 0, 10, 10), range: None, text_input: false };
        assert_eq!(click_point(&button, Action::Click, None), Some((5.0, 5.0)));
        assert_eq!(click_point(&button, Action::Focus, None), None, "focusing a button does not press it");
    }
}
