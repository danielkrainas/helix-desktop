//! The shell's own interface on the accessibility bus (helix-desktop-spec
//! `contracts/dbus/org.a11y.atspi.md`).
//!
//! The UI describes what each of its surfaces shows as a [`Window`] of
//! [`Node`]s, from the same state it paints with; [`tree`] turns a window
//! into an AccessKit tree, and [`publish`] keeps one AccessKit adapter per
//! surface, which serves it over AT-SPI on the accessibility bus. Without
//! an accessibility bus (or with nobody listening) nothing is built.

pub mod publish;
pub mod tree;

pub use accesskit::Role;

/// A rectangle in logical pixels: surface coordinates for nodes, layout
/// (screen) coordinates for windows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

impl Rect {
    pub fn new(x: i32, y: i32, w: i32, h: i32) -> Self {
        Self { x, y, w, h }
    }

    pub fn center(&self) -> (f64, f64) {
        (f64::from(self.x) + f64::from(self.w) / 2.0, f64::from(self.y) + f64::from(self.h) / 2.0)
    }
}

/// A slider's value and range.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Range {
    pub value: f64,
    pub min: f64,
    pub max: f64,
    pub step: f64,
}

/// One accessible object of a surface.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Node {
    pub role: Role,
    /// Tells the node from its siblings across updates (default: the
    /// role and the name), so that a changing name keeps its object.
    pub id: String,
    pub name: String,
    pub description: String,
    /// A text field's text (already masked for passwords), or a label's.
    pub text: Option<String>,
    pub bounds: Option<Rect>,
    pub toggled: Option<bool>,
    pub selected: Option<bool>,
    pub expanded: Option<bool>,
    pub range: Option<Range>,
    pub focused: bool,
    pub focusable: bool,
    /// Activating it does what a click on its bounds does.
    pub clickable: bool,
    pub disabled: bool,
    /// Changes are announced (messages, errors, notifications).
    pub live: bool,
    pub modal: bool,
    pub children: Vec<Node>,
}

impl Node {
    pub fn new(role: Role, name: impl Into<String>) -> Self {
        Self { role, name: name.into(), ..Default::default() }
    }

    /// A button at `r`.
    pub fn button(name: impl Into<String>, r: Rect) -> Self {
        Self::new(Role::Button, name).at(r).click()
    }

    /// Words shown as they are.
    pub fn label(text: impl Into<String>) -> Self {
        let text = text.into();
        Self { role: Role::Label, id: String::new(), text: Some(text), ..Default::default() }
    }

    /// A text field named `name` holding `text`.
    pub fn text_field(role: Role, name: impl Into<String>, text: impl Into<String>, r: Rect) -> Self {
        Self { text: Some(text.into()), focusable: true, ..Self::new(role, name).at(r).click() }
    }

    /// The text a password field shows for `secret`: one bullet per
    /// character (the secret itself never leaves the field).
    pub fn mask(secret: &str) -> String {
        "•".repeat(secret.chars().count())
    }

    pub fn at(mut self, r: Rect) -> Self {
        self.bounds = Some(r);
        self
    }

    pub fn id(mut self, id: impl Into<String>) -> Self {
        self.id = id.into();
        self
    }

    pub fn describe(mut self, description: impl Into<String>) -> Self {
        self.description = description.into();
        self
    }

    pub fn click(mut self) -> Self {
        self.clickable = true;
        self
    }

    pub fn focused(mut self, focused: bool) -> Self {
        self.focused = focused;
        self.focusable |= focused;
        self
    }

    pub fn focusable(mut self) -> Self {
        self.focusable = true;
        self
    }

    pub fn toggled(mut self, on: bool) -> Self {
        self.toggled = Some(on);
        self
    }

    pub fn selected(mut self, on: bool) -> Self {
        self.selected = Some(on);
        self
    }

    pub fn expanded(mut self, open: bool) -> Self {
        self.expanded = Some(open);
        self
    }

    pub fn range(mut self, value: f64, min: f64, max: f64, step: f64) -> Self {
        self.range = Some(Range { value, min, max, step });
        self
    }

    pub fn disabled(mut self, disabled: bool) -> Self {
        self.disabled = disabled;
        self
    }

    pub fn live(mut self) -> Self {
        self.live = true;
        self
    }

    pub fn modal(mut self) -> Self {
        self.modal = true;
        self
    }

    pub fn child(mut self, node: Node) -> Self {
        self.children.push(node);
        self
    }

    pub fn children(mut self, nodes: impl IntoIterator<Item = Node>) -> Self {
        self.children.extend(nodes);
        self
    }

    /// The first node, depth first, that `f` accepts.
    pub fn find(&self, f: &impl Fn(&Node) -> bool) -> Option<&Node> {
        if f(self) {
            return Some(self);
        }
        self.children.iter().find_map(|c| c.find(f))
    }
}

/// What one surface shows: a top-level window of the shell's application.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Window {
    pub title: String,
    /// Where the surface is on the screen (layout coordinates).
    pub bounds: Rect,
    /// The surface has the keyboard.
    pub active: bool,
    pub children: Vec<Node>,
}

impl Window {
    /// The first node, depth first, that `f` accepts.
    pub fn find(&self, f: impl Fn(&Node) -> bool) -> Option<&Node> {
        self.children.iter().find_map(|c| c.find(&f))
    }

    /// The node with the keyboard focus.
    pub fn focus(&self) -> Option<&Node> {
        self.find(|n| n.focused)
    }
}
