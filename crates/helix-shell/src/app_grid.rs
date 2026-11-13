//! The app grid's layout (helix-desktop-spec `contracts/wayland/README.md`,
//! "App grid"): pages of apps and folders, kept in
//! `org.gnome.shell app-picker-layout`.

use gio::glib::{Variant, VariantDict};
use gio::prelude::*;

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
    let s = gio::Settings::new(SHELL);
    if !s.settings_schema().is_some_and(|x| x.has_key(KEY)) {
        return Vec::new();
    }
    parse(&s.value(KEY))
}

pub fn write(pages: &Pages) {
    if !helix_settings::has_schema(SHELL) {
        return;
    }
    let s = gio::Settings::new(SHELL);
    if s.settings_schema().is_some_and(|x| x.has_key(KEY)) {
        let _ = s.set_value(KEY, &to_variant(pages));
        gio::Settings::sync();
    }
}

/// Pages no longer than `per_page`: longer ones spill into the next.
fn spill(mut pages: Pages, per_page: usize) -> Pages {
    let per_page = per_page.max(1);
    let mut i = 0;
    while i < pages.len() {
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
pub fn arrange(saved: &Pages, items: &[(String, String)], per_page: usize) -> Pages {
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
    spill(pages, per_page)
}

/// `pages` with `id` moved to `index` of page `page` (a new last page when
/// `page` is past the end).
pub fn move_item(pages: &Pages, id: &str, page: usize, index: usize, per_page: usize) -> Pages {
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
    spill(out, per_page)
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
}
