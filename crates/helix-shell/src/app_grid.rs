//! The app grid's layout (helixdesktop/spec `contracts/wayland/README.md`,
//! "App grid"): pages of apps and folders, kept in
//! `org.gnome.shell app-picker-layout`.

use gio::glib::{Variant, VariantDict};

/// Pages of item ids: an app's desktop file id, or a folder's id.
pub type Pages = Vec<Vec<String>>;

const SHELL: &str = "org.gnome.shell";
const KEY: &str = "app-picker-layout";

/// `app-picker-layout` (`aa{sv}`: per page, item id → `{position: i}`)
/// as pages ordered by position.
pub fn parse(value: &Variant) -> Pages {
    let mut pages = Vec::new();
    for page in value.iter() {
        let mut items: Vec<(i32, String)> = Vec::new();
        for entry in page.iter() {
            let Some(id) = entry.child_value(0).str().map(str::to_owned) else { continue };
            let props = entry.child_value(1).as_variant().unwrap_or(entry.child_value(1));
            let position = VariantDict::new(Some(&props)).lookup::<i32>("position").ok().flatten().unwrap_or(i32::MAX);
            items.push((position, id));
        }
        items.sort();
        pages.push(items.into_iter().map(|(_, id)| id).collect());
    }
    pages
}

/// `pages` as `app-picker-layout`.
pub fn to_variant(pages: &Pages) -> Variant {
    let ty = gio::glib::VariantTy::new("a{sv}").expect("type");
    let pages: Vec<Variant> = pages
        .iter()
        .map(|page| {
            let dict = VariantDict::new(None);
            for (i, id) in page.iter().enumerate() {
                let props = VariantDict::new(None);
                props.insert("position", i as i32);
                dict.insert_value(id, &props.end());
            }
            dict.end()
        })
        .collect();
    Variant::array_from_iter_with_type(ty, pages)
}

pub fn read() -> Pages {
    if !helix_settings::has_schema(SHELL) {
        return Vec::new();
    }
    let s = helix_settings::Settings::new(SHELL);
    if !s.settings_schema().is_some_and(|x| x.has_key(KEY)) {
        return Vec::new();
    }
    parse(&s.value(KEY))
}

pub fn write(pages: &Pages) {
    if !helix_settings::has_schema(SHELL) {
        return;
    }
    let s = helix_settings::Settings::new(SHELL);
    if s.settings_schema().is_some_and(|x| x.has_key(KEY)) {
        let _ = s.set_value(KEY, &to_variant(pages));
        gio::Settings::sync();
    }
}

/// How many items a page holds: the first page can hold fewer (a row
/// less while the "Recent" row shows above it).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PerPage {
    pub first: usize,
    pub rest: usize,
}

impl PerPage {
    /// Every page alike.
    pub fn uniform(n: usize) -> Self {
        Self { first: n, rest: n }
    }

    /// What page `page` holds (at least one).
    pub fn of(self, page: usize) -> usize {
        if page == 0 { self.first } else { self.rest }.max(1)
    }
}

impl From<usize> for PerPage {
    fn from(n: usize) -> Self {
        Self::uniform(n)
    }
}

/// The "Recent" row: of `recent` (desktop file ids, the latest first), the
/// apps that are `shown` (installed and listed by the launcher), each once,
/// at most `cols`.
pub fn recent_row(recent: &[String], shown: impl Fn(&str) -> bool, cols: usize) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for id in recent {
        if out.len() == cols {
            break;
        }
        if shown(id) && !out.contains(id) {
            out.push(id.clone());
        }
    }
    out
}

/// Pages no longer than they hold: longer ones spill into the next.
fn spill(mut pages: Pages, per: PerPage) -> Pages {
    let mut i = 0;
    while i < pages.len() {
        let per_page = per.of(i);
        if pages[i].len() > per_page {
            let rest = pages[i].split_off(per_page);
            if i + 1 == pages.len() {
                pages.push(Vec::new());
            }
            let mut next = rest;
            next.append(&mut pages[i + 1]);
            pages[i + 1] = next;
        }
        i += 1;
    }
    pages.retain(|p| !p.is_empty());
    pages
}

/// The grid for `items` (id, name): their saved places first (items no
/// longer there dropped), then new ones by name at the end; at most
/// `per_page` a page.
pub fn arrange(saved: &Pages, items: &[(String, String)], per_page: impl Into<PerPage>) -> Pages {
    let known = |id: &String| items.iter().any(|(i, _)| i == id);
    let mut pages: Pages = saved.iter().map(|p| p.iter().filter(|id| known(id)).cloned().collect()).collect();
    let placed: Vec<&String> = saved.iter().flatten().collect();
    let mut new: Vec<&(String, String)> = items.iter().filter(|(id, _)| !placed.contains(&id)).collect();
    new.sort_by_key(|(_, name)| name.to_lowercase());
    pages.retain(|p| !p.is_empty());
    if pages.is_empty() {
        pages.push(Vec::new());
    }
    pages.last_mut().expect("one page").extend(new.into_iter().map(|(id, _)| id.clone()));
    spill(pages, per_page.into())
}

/// `pages` with `id` moved to `index` of page `page` (a new last page when
/// `page` is past the end).
pub fn move_item(pages: &Pages, id: &str, page: usize, index: usize, per_page: impl Into<PerPage>) -> Pages {
    let mut out: Pages = pages.clone();
    let mut from = None;
    for (p, items) in out.iter_mut().enumerate() {
        if let Some(i) = items.iter().position(|x| x == id) {
            items.remove(i);
            from = Some((p, i));
        }
    }
    while out.len() <= page {
        out.push(Vec::new());
    }
    // Within one page, later places shift down by one.
    let index = match from {
        Some((p, i)) if p == page && i < index => index - 1,
        _ => index,
    };
    let target = &mut out[page];
    target.insert(index.min(target.len()), id.to_owned());
    spill(out, per_page.into())
}

/// `pages` without `id`.
pub fn remove(pages: &Pages, id: &str) -> Pages {
    let mut out: Pages = pages.iter().map(|p| p.iter().filter(|x| *x != id).cloned().collect()).collect();
    out.retain(|p| !p.is_empty());
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ids(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    fn items(v: &[(&str, &str)]) -> Vec<(String, String)> {
        v.iter().map(|(a, b)| (a.to_string(), b.to_string())).collect()
    }

    #[test]
    fn round_trip() {
        let pages = vec![ids(&["b.desktop", "Utilities", "a.desktop"]), ids(&["c.desktop"])];
        assert_eq!(parse(&to_variant(&pages)), pages);
        assert_eq!(to_variant(&pages).type_().as_str(), "aa{sv}");
    }

    #[test]
    fn arranging() {
        let saved = vec![ids(&["b.desktop", "gone.desktop", "a.desktop"])];
        let all = items(&[("a.desktop", "Atlas"), ("b.desktop", "Bee"), ("z.desktop", "Zed"), ("c.desktop", "Crab")]);
        assert_eq!(arrange(&saved, &all, 3), [ids(&["b.desktop", "a.desktop", "c.desktop"]), ids(&["z.desktop"])]);
        assert_eq!(arrange(&Vec::new(), &all, 10), [ids(&["a.desktop", "b.desktop", "c.desktop", "z.desktop"])]);
    }

    #[test]
    fn moving() {
        let pages = vec![ids(&["a", "b", "c"]), ids(&["d"])];
        assert_eq!(move_item(&pages, "a", 0, 3, 3), [ids(&["b", "c", "a"]), ids(&["d"])]);
        assert_eq!(move_item(&pages, "d", 0, 0, 3), [ids(&["d", "a", "b"]), ids(&["c"])]);
        assert_eq!(move_item(&pages, "a", 2, 0, 3), [ids(&["b", "c"]), ids(&["d"]), ids(&["a"])]);
        assert_eq!(remove(&pages, "d"), [ids(&["a", "b", "c"])]);
    }

    #[test]
    fn recent_row_order_and_limit() {
        let installed = ["a.desktop", "b.desktop", "c.desktop", "d.desktop"];
        let shown = |id: &str| installed.contains(&id);
        let recent = ids(&["c.desktop", "gone.desktop", "a.desktop", "c.desktop", "b.desktop", "d.desktop"]);
        assert_eq!(recent_row(&recent, shown, 8), ids(&["c.desktop", "a.desktop", "b.desktop", "d.desktop"]), "latest first, removed apps skipped, each once");
        assert_eq!(recent_row(&recent, shown, 3), ids(&["c.desktop", "a.desktop", "b.desktop"]), "one row's worth");
        assert!(recent_row(&[], shown, 8).is_empty());
        assert!(recent_row(&ids(&["gone.desktop"]), shown, 8).is_empty(), "nothing installed: no row");
    }

    /// With the "Recent" row the first page holds a row less; every item
    /// is on exactly one page, in order.
    #[test]
    fn pages_with_the_recent_row() {
        let all: Vec<(String, String)> = (0..23).map(|i| (format!("{i:02}.desktop"), format!("App {i:02}"))).collect();
        let (cols, rows) = (4, 3);
        let with_row = PerPage { first: cols * (rows - 1), rest: cols * rows };
        let pages = arrange(&Vec::new(), &all, with_row);
        assert_eq!(pages.iter().map(Vec::len).collect::<Vec<_>>(), [8, 12, 3]);
        let flat: Vec<&String> = pages.iter().flatten().collect();
        assert_eq!(flat, all.iter().map(|(id, _)| id).collect::<Vec<_>>(), "each app once, in order");
        let without = arrange(&Vec::new(), &all, cols * rows);
        assert_eq!(without.iter().map(Vec::len).collect::<Vec<_>>(), [12, 11]);
        // A saved full first page spills its last row into the next.
        let saved = without.clone();
        let spilled = arrange(&saved, &all, with_row);
        assert_eq!(spilled[0], saved[0][..8]);
        assert_eq!(spilled[1][..4], saved[0][8..]);
        assert_eq!(spilled.iter().flatten().count(), 23);
        // Moving onto the first page keeps it to its size.
        let moved = move_item(&pages, "22.desktop", 0, 0, with_row);
        assert_eq!(moved[0].len(), 8);
        assert_eq!(moved[0][0], "22.desktop");
        assert_eq!(moved.iter().flatten().count(), 23);
        assert_eq!(PerPage { first: 0, rest: 5 }.of(0), 1, "a page holds at least one");
    }
}
