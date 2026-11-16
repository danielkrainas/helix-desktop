//! Renders of shell surfaces over real wallpapers, for looking at:
//! `HELIX_SHELL_RENDER_DIR=dir cargo test -p helix-shell --lib
//! ui::gallery -- --ignored --test-threads=1` (high contrast and the text
//! scale are global, so the scenes run one at a time).

use super::*;

/// Where the renders go; `None` (the test does nothing) without
/// `HELIX_SHELL_RENDER_DIR`.
pub(super) fn dir() -> Option<std::path::PathBuf> {
    let dir = std::path::PathBuf::from(std::env::var_os("HELIX_SHELL_RENDER_DIR")?);
    std::fs::create_dir_all(&dir).ok()?;
    Some(dir)
}

/// Write an ARGB8888 buffer as a PNG.
pub(super) fn save(dir: &std::path::Path, name: &str, data: &[u8], pw: usize, ph: usize) {
    let file = std::io::BufWriter::new(std::fs::File::create(dir.join(format!("{name}.png"))).unwrap());
    let mut encoder = png::Encoder::new(file, pw as u32, ph as u32);
    encoder.set_color(png::ColorType::Rgb);
    let mut writer = encoder.write_header().unwrap();
    let rgb: Vec<u8> = data.as_chunks::<4>().0.iter().flat_map(|p| [p[2], p[1], p[0]]).collect();
    writer.write_image_data(&rgb).unwrap();
}

/// A `w`×`h` (logical) screen at `scale` showing a wallpaper (black in
/// high contrast).
pub(super) fn wallpaper(name: &str, (w, h): (usize, usize), scale: f64) -> Vec<u8> {
    let (pw, ph) = (physical(w, scale), physical(h, scale));
    let mut px = vec![0u8; pw * ph * 4];
    if helix_draw::high_contrast() {
        helix_draw::backdrop::high_contrast(&mut px);
        return px;
    }
    let image = crate::wallpaper::decode(std::path::Path::new(&format!("/usr/share/backgrounds/{name}"))).ok().map(Arc::new);
    let bg = crate::wallpaper::Background { image, ..Default::default() };
    background::paint(&mut px, pw, ph, scale, &bg, (w as f64, h as f64), None);
    px
}

pub(super) const DARK: &str = "Resolute_Raccoon_Wallpaper_Dimmed_3840x2160.png";
pub(super) const LIGHT: &str = "Resolute_Raccoon_Wallpaper_Light_3840x2160.png";
#[allow(dead_code)]
pub(super) const PHOTO: &str = "mendhak-Bluebells_Suspended_In_Time.jpg";

/// Render one scene: `paint` draws on a `w`×`h` canvas at `scale` over
/// `wall`.
/// The surface is painted on its own (transparent) buffer, then laid over
/// the wallpaper as the compositor would.
pub(super) fn render(dir: &std::path::Path, name: &str, wall: &str, (w, h, scale): (usize, usize, f64), paint: impl FnOnce(&mut Canvas)) {
    let mut data = wallpaper(wall, (w, h), scale);
    let mut surface = vec![0u8; data.len()];
    let mut canvas = Canvas::scaled(&mut surface, w, h, scale).mirrored(helix_draw::rtl());
    paint(&mut canvas);
    // Premultiplied "over".
    for (d, s) in data.as_chunks_mut::<4>().0.iter_mut().zip(surface.as_chunks::<4>().0) {
        let a = u32::from(s[3]);
        for i in 0..3 {
            d[i] = (u32::from(s[i]) + u32::from(d[i]) * (255 - a) / 255).min(255) as u8;
        }
        d[3] = 255;
    }
    save(dir, name, &data, physical(w, scale), physical(h, scale));
}

/// One way of looking at the shell: a wallpaper, a scale, high contrast,
/// an accent, a text scale.
pub(super) struct Look {
    pub name: &'static str,
    pub wall: &'static str,
    pub scale: f64,
    pub hc: bool,
    pub accent: helix_draw::accent::Accent,
    pub text: f64,
}

/// The looks every surface's gallery shows: dark and light wallpapers,
/// scale 1.5, high contrast, a non-blue accent and text at 1.5.
pub(super) fn looks() -> Vec<Look> {
    use helix_draw::accent::Accent;
    let look = |name, wall, scale, hc, accent, text| Look { name, wall, scale, hc, accent, text };
    vec![
        look("dark", DARK, 1.0, false, Accent::Blue, 1.0),
        look("light", LIGHT, 1.0, false, Accent::Blue, 1.0),
        look("scale1.5", DARK, 1.5, false, Accent::Blue, 1.0),
        look("hc", DARK, 1.0, true, Accent::Blue, 1.0),
        look("orange", LIGHT, 1.0, false, Accent::Orange, 1.0),
        look("text1.5", DARK, 1.0, false, Accent::Blue, 1.5),
    ]
}

/// Render `paint` on a `w`×`h` screen in every [`looks`]:
/// `{name}-{look}.png`.
pub(super) fn in_looks(dir: &std::path::Path, name: &str, (w, h): (usize, usize), paint: &mut dyn FnMut(&mut Canvas, (usize, usize))) {
    for l in looks() {
        helix_draw::accent::set(l.accent);
        helix_draw::set_high_contrast(l.hc);
        helix_draw::theme::set_text_scale(l.text);
        render(dir, &format!("{name}-{}", l.name), l.wall, (w, h, l.scale), |canvas| paint(canvas, (w, h)));
    }
    helix_draw::theme::set_text_scale(1.0);
    helix_draw::set_high_contrast(false);
    helix_draw::accent::set(helix_draw::accent::Accent::Blue);
}

/// The panel (`panel_with`) drawn over the top of `canvas`, as its own
/// surface.
fn panel_over(canvas: &mut Canvas, text: Option<&Text>, w: usize, hover: Option<crate::panel_layout::Slot>, power_open: bool) {
    let ph = panel_height() as usize;
    let scale = canvas.scale;
    let mut data = vec![0u8; physical(w, scale) * physical(ph, scale) * 4];
    let mut bar = Canvas::scaled(&mut data, w, ph, scale);
    panel_with(&mut bar, text, (w, ph), scale, None, power_open, hover, None);
    let n = physical(w, scale) * physical(ph, scale) * 4;
    canvas.data[..n].copy_from_slice(&data);
}

/// Every surface on the shared look (helixdesktop/spec
/// `components/README.md`, "Shell look") in every [`looks`]: `look-…`.
#[test]
#[ignore]
fn shared_look() {
    use notify::tests::{paint_sample, sample, sample_calendar};
    let Some(dir) = dir() else { return };
    let (text, bold) = (Text::load(), Text::load_matching("sans-serif:bold"));
    let entries = sample();
    let open: std::collections::HashSet<String> = ["app:org.example.Chat".to_owned()].into();
    let cal = sample_calendar(true);
    in_looks(&dir, "look-quick-menu", (1280, 720), &mut |canvas, (w, h)| {
        quick::paint_quick(canvas, (text.as_ref(), Text::load_matching("sans-serif:bold").as_ref()), (w, h), &quick::tests::sample(Some(3)), None, &mut Default::default());
        let ph = panel_height() as i32;
        window_menu::paint_menu(canvas, text.as_ref(), (w as i32, h as i32), (40, ph + 4), &menu_items(), Some(1), false, false);
        panel_over(canvas, text.as_ref(), w, Some(crate::panel_layout::Slot::Item(crate::panel_layout::Item::Clock)), true);
    });
    in_looks(&dir, "look-list", (1280, 720), &mut |canvas, (w, h)| {
        paint_sample(canvas, (text.as_ref(), bold.as_ref()), (w, h), &entries, &open, &cal, Some(&notify::ListHit::Entry(4)));
        panel_over(canvas, text.as_ref(), w, None, false);
    });
    in_looks(&dir, "look-dialog", (1280, 720), &mut |canvas, (w, h)| {
        let state = DialogState { focus: Some(Choice::Canceled), ..DialogState::new(DialogKind::End(EndDialog { kind: SHUTDOWN, seconds: 60, blockers: Vec::new(), install: Some(true), upgrade: None, blocker_apps: Vec::new() }), 60) };
        paint_question(canvas, (text.as_ref(), Text::load_matching("sans-serif:bold").as_ref()), (w, h), &state);
    });
    in_looks(&dir, "look-dialog-hover", (1280, 720), &mut |canvas, (w, h)| {
        let state = DialogState { hover: Some(OverlayHit::Confirm), pressed: None, ..DialogState::new(DialogKind::NotResponding { id: 1, title: "Text Editor".into() }, 0) };
        paint_question(canvas, (text.as_ref(), Text::load_matching("sans-serif:bold").as_ref()), (w, h), &state);
    });
}

/// The type scale in the interface font (`font-name`): the system font,
/// then two others, on the list, quick settings and a dialog
/// (`font-…`).
#[test]
#[ignore]
fn interface_font() {
    use notify::tests::{paint_sample, sample, sample_calendar};
    let Some(dir) = dir() else { return };
    let entries = sample();
    let cal = sample_calendar(true);
    for family in [None, Some("DejaVu Serif"), Some("Century Gothic")] {
        let (text, bold) = (Text::load_family(family, None), Text::load_family(family, Some("bold")));
        let tag = family.unwrap_or("system").replace(' ', "-");
        render(&dir, &format!("font-{tag}-list"), DARK, (1280, 720, 1.0), |canvas| {
            paint_sample(canvas, (text.as_ref(), bold.as_ref()), (1280, 720), &entries, &Default::default(), &cal, None);
        });
        render(&dir, &format!("font-{tag}-dialog"), DARK, (1280, 720, 1.0), |canvas| {
            let state = DialogState::new(DialogKind::End(EndDialog { kind: SHUTDOWN, seconds: 60, blockers: Vec::new(), install: Some(true), upgrade: None, blocker_apps: Vec::new() }), 60);
            paint_question(canvas, (text.as_ref(), bold.as_ref()), (1280, 720), &state);
            let ph = panel_height() as usize;
            let mut data = vec![0u8; 1280 * ph * 4];
            let mut bar = Canvas::scaled(&mut data, 1280, ph, 1.0);
            let layout = crate::panel_layout::Layout::default();
            let clock = crate::clock::top_bar(crate::clock::ClockFormat { weekday: true, ..Default::default() }, &gio::glib::DateTime::new(&gio::glib::TimeZone::local(), 2026, 11, 14, 14, 5, 0.0).unwrap());
            let model = PanelModel { layout: &layout, clock: &clock, status_items: &[], tray_images: &[], tray_w: 0, ext_items: &[], ext_icons: &[], ext_widths: &[], locked: false, launcher_open: false, power_open: false, list_open: false, menu_open: None, unseen_dot: false, workspaces: (4, 1), a11y_shown: false, source_label: Some("en"), sharing: false, recording: false, drag_target: None, focus: None, hover: None, pressed: None, power_icon: None, bold: bold.as_ref() };
            paint_panel(&mut bar, text.as_ref(), (1280, ph), &model);
            blit(canvas, &data, 1280, ph);
        });
    }
}

/// Quick settings (`qs-…`): at rest with the Wi-Fi list, the power
/// actions open, the Power Mode list open with a toggle hovered, in every
/// look; and at 1920×1080 at scales 1 and 1.5 on both wallpapers.
#[test]
#[ignore]
fn quick_settings_look() {
    let Some(dir) = dir() else { return };
    let (text, bold) = (Text::load(), Text::load_matching("sans-serif:bold"));
    let fonts = (text.as_ref(), bold.as_ref());
    let paint = |canvas: &mut Canvas, (w, h): (usize, usize), m: &quick::QuickModel, focus| {
        quick::paint_quick(canvas, fonts, (w, h), m, focus, &mut Default::default());
        panel_over(canvas, text.as_ref(), w, None, true);
    };
    in_looks(&dir, "qs-wifi", (1280, 720), &mut |canvas, size| paint(canvas, size, &quick::tests::sample(Some(3)), None));
    in_looks(&dir, "qs-power", (1280, 720), &mut |canvas, size| paint(canvas, size, &quick::tests::sample_power(), Some(OverlayHit::Power(PowerAction::Restart))));
    in_looks(&dir, "qs-modes", (1280, 720), &mut |canvas, size| {
        let m = quick::QuickModel { hover: Some(OverlayHit::Switch(quick::Switch::Bluetooth)), pressed: Some(OverlayHit::Mute), ..quick::tests::sample_modes() };
        paint(canvas, size, &m, Some(OverlayHit::Switch(quick::Switch::NightLight)));
    });
    for (wall, tag) in [(DARK, "dark"), (LIGHT, "light")] {
        for (w, h, scale) in [(1920, 1080, 1.0), (1280, 720, 1.5)] {
            render(&dir, &format!("qs-1920x1080-{tag}@{scale}"), wall, (w, h, scale), |canvas| paint(canvas, (w, h), &quick::tests::sample(None), None));
        }
    }
}

/// Hover and press (`press-…`): a notification under the pointer with
/// Clear pressed, a calendar day and month arrow, a dialog's check box
/// hovered with Cancel pressed, quick settings with a slider held and a
/// list row lit.
#[test]
#[ignore]
fn hover_and_press() {
    use notify::tests::{paint_sample_pointer, sample, sample_calendar};
    use notify::ListHit;
    let Some(dir) = dir() else { return };
    let (text, bold) = (Text::load(), Text::load_matching("sans-serif:bold"));
    let entries = sample();
    let cal = sample_calendar(true);
    let (y, m, d) = cal.selected;
    in_looks(&dir, "press-list", (1280, 720), &mut |canvas, size| {
        paint_sample_pointer(canvas, (text.as_ref(), bold.as_ref()), size, &entries, &Default::default(), &cal, None, (Some(&ListHit::Entry(5)), Some(&ListHit::Clear)));
    });
    in_looks(&dir, "press-calendar", (1280, 720), &mut |canvas, size| {
        paint_sample_pointer(canvas, (text.as_ref(), bold.as_ref()), size, &entries, &Default::default(), &cal, None, (Some(&ListHit::Day(y, m, d + 2)), Some(&ListHit::Month(1))));
    });
    in_looks(&dir, "press-dialog", (1280, 720), &mut |canvas, size| {
        let state = DialogState { hover: Some(OverlayHit::EndInstall), pressed: Some(OverlayHit::Cancel), ..DialogState::new(DialogKind::End(EndDialog { kind: SHUTDOWN, seconds: 60, blockers: Vec::new(), install: Some(false), upgrade: None, blocker_apps: Vec::new() }), 60) };
        paint_question(canvas, (text.as_ref(), bold.as_ref()), size, &state);
    });
    in_looks(&dir, "press-quick", (1280, 720), &mut |canvas, size| {
        let m = quick::QuickModel { hover: Some(OverlayHit::Network(1)), pressed: Some(OverlayHit::VolumeSlider), ..quick::tests::sample(Some(3)) };
        quick::paint_quick(canvas, (text.as_ref(), bold.as_ref()), size, &m, None, &mut Default::default());
    });
}

/// The application switcher (`switcher-…`): five apps (two with several
/// windows) with one under the pointer, and fourteen on a 1280×720
/// screen; in every look.
#[test]
#[ignore]
fn app_switcher() {
    let Some(dir) = dir() else { return };
    let (text, bold) = (Text::load(), Text::load_matching("sans-serif:bold"));
    let ids = [
        "org.gnome.Nautilus", "firefox", "org.gnome.Terminal", "org.gnome.TextEditor", "org.gnome.Calculator", "org.gnome.Calendar", "org.gnome.Settings", "org.gnome.Software", "org.gnome.clocks", "org.gnome.Weather", "org.gnome.Maps",
        "org.gnome.Characters", "org.gnome.Logs", "org.gnome.Contacts",
    ];
    let names = ["Files", "Firefox", "Terminal", "Text Editor", "Calculator", "Calendar", "Settings", "Software", "Clocks", "Weather", "Maps", "Characters", "Logs", "Contacts"];
    for (tag, n) in [("5", 5usize), ("14", 14)] {
        let entries: Vec<SwitchEntry> = (0..n).map(|i| SwitchEntry { id: i as u32 + 1, label: names[i].into(), detail: String::new(), count: if i % 3 == 0 { 2 + i % 2 } else { 1 } }).collect();
        let switcher = Switcher { entries, selected: 1 };
        in_looks(&dir, &format!("switcher-{tag}"), (1280, 720), &mut |canvas, (w, h)| {
            let Some(icon) = app_row_icon(w as i32, n) else { return };
            let px = (f64::from(icon) * canvas.scale).ceil() as u32;
            let icons: Vec<Option<helix_draw::Image>> = ids.iter().take(n).map(|id| crate::icons::app_icon(id, px)).collect();
            paint_app_row(canvas, (text.as_ref(), bold.as_ref()), (w as i32, h as i32), &switcher, &icons, icon, (Some(OverlayHit::Window(3)), None));
        });
    }
}

/// The switcher as a list (`switcher-list-…`): four windows with icons,
/// the second chosen from the keyboard, the third under the pointer; in
/// every look.
#[test]
#[ignore]
fn switcher_list() {
    let Some(dir) = dir() else { return };
    let text = Text::load();
    let apps = [("org.gnome.Nautilus", "Files — Home"), ("firefox", "Mozilla Firefox"), ("org.gnome.TextEditor", "notes.txt — Text Editor"), ("org.gnome.Terminal", "Terminal")];
    let entries: Vec<SwitchEntry> = apps.iter().enumerate().map(|(i, (_, title))| SwitchEntry { id: i as u32 + 1, label: (*title).into(), detail: if i == 3 { "Workspace 2".into() } else { String::new() }, count: 1 }).collect();
    let switcher = Switcher { entries, selected: 1 };
    in_looks(&dir, "switcher-list", (1280, 720), &mut |canvas, (w, h)| {
        let px = (f64::from(ICON_PX) * canvas.scale).ceil() as u32;
        let icons: Vec<Option<helix_draw::Image>> = apps.iter().map(|(id, _)| crate::icons::app_icon(id, px)).collect();
        paint_switcher_list(canvas, text.as_ref(), (w as i32, h as i32), &switcher, &icons, (Some(OverlayHit::Window(3)), None));
    });
}

/// The overview's look (`overview-…`): the panel above it, the search
/// field, three windows (rounded pictures with their app icons, one under
/// the pointer, one chosen from the keyboard with its close button).
#[test]
#[ignore]
fn overview_look() {
    let Some(dir) = dir() else { return };
    let text = Text::load();
    let picture = |name: &str| crate::wallpaper::decode(std::path::Path::new(&format!("/usr/share/backgrounds/{name}"))).ok().map(|i| crate::wallpaper::thumbnail(&i, 640, 400));
    let pictures = [picture(PHOTO), picture("mendhak-Red_Acer.jpg"), picture(LIGHT)];
    let apps = [("org.gnome.Nautilus", "Files — Home"), ("firefox", "Mozilla Firefox"), ("org.gnome.TextEditor", "notes.txt — Text Editor")];
    in_looks(&dir, "overview", (1280, 720), &mut |canvas, (w, h)| {
        let ph = panel_height() as i32;
        canvas.rect(0, ph, w as i32, h as i32 - ph, SCRIM);
        let fw = helix_draw::theme::grow(SEARCH_W).min(w as i32 - 32);
        let field = Rect { x: (w as i32 - fw) / 2, y: ph + 24, w: fw, h: helix_draw::theme::grow(SEARCH_H) };
        search_field(canvas, text.as_ref(), field, "", helix_i18n::tr("Type to search…"), false);
        let area = Rect { x: 32, y: field.y + field.h + 40, w: w as i32 - 64, h: h as i32 - field.y - field.h - 80 };
        let sizes: Vec<(i32, i32)> = pictures.iter().map(|p| p.as_ref().map_or((640, 400), |p| (p.width as i32, p.height as i32))).collect();
        let px = (48.0 * canvas.scale).ceil() as u32;
        for (i, rect) in overview::arrange(&sizes, area.w, area.h).into_iter().enumerate() {
            let r = Rect { x: area.x + rect.x, y: area.y + rect.y, ..rect };
            let icon = crate::icons::app_icon(apps[i].0, px);
            let tile = overview::WindowTile { picture: pictures[i].as_ref(), icon: icon.as_ref(), title: apps[i].1, hovered: i == 0, focused: i == 2, close: i != 1 };
            overview::paint_window_tile(canvas, text.as_ref(), r, &tile);
        }
        panel_over(canvas, text.as_ref(), w, None, false);
    });
}

/// The launcher searching "note" (`launcher-…`): an open window, apps,
/// and two search providers' results under their headings, with icons on
/// every row; the second app under the pointer; in every look.
#[test]
#[ignore]
fn launcher_results() {
    use crate::launcher::{App, Hit, Launcher, SearchHit};
    let Some(dir) = dir() else { return };
    let (text, bold) = (Text::load(), Text::load_matching("sans-serif:bold"));
    let app = |id: &str, name: &str| App { id: format!("{id}.desktop"), name: name.into(), search: "note notes".into(), categories: Vec::new() };
    let apps = vec![app("org.gnome.TextEditor", "Text Editor"), app("org.gnome.Notes", "Notes"), app("org.gnome.Nautilus", "Files"), app("org.gnome.Characters", "Characters")];
    let window = crate::windows::WindowEntry { id: 7, app_id: "org.gnome.TextEditor".into(), title: "notes.txt — ~/Documents".into(), workspace: 0 };
    let mut model = Launcher::with_windows(apps.clone(), vec![window]);
    model.type_text("note");
    let hit = |provider, app: &str, name: &str, description: &str| SearchHit { provider, app: app.into(), id: name.into(), name: name.into(), description: description.into(), icon: None, clipboard: None };
    model.set_search(0, vec![hit(0, "Files", "notes.txt", "~/Documents"), hit(0, "Files", "Notebook", "~/Pictures")]);
    model.set_search(1, vec![hit(1, "Characters", "♪", "Eighth Note")]);
    in_looks(&dir, "launcher", (1280, 720), &mut |canvas, (w, h)| {
        canvas.fill(SCRIM);
        let px = (32.0 * canvas.scale).ceil() as u32;
        let icon_of = |id: &str| crate::icons::app_icon(id, px);
        let rows: HashMap<usize, Option<helix_draw::Image>> = model
            .results
            .iter()
            .enumerate()
            .map(|(i, hit)| {
                let icon = match *hit {
                    Hit::Window(_) => icon_of("org.gnome.TextEditor"),
                    Hit::App(a) => icon_of(model.apps[a].id.trim_end_matches(".desktop")),
                    Hit::Search(e) => apps.iter().find(|a| a.name == model.external[e].app).and_then(|a| icon_of(a.id.trim_end_matches(".desktop"))),
                    _ => None,
                };
                (i, icon)
            })
            .collect();
        let small = (16.0 * canvas.scale).ceil() as u32;
        let headings: HashMap<usize, Option<helix_draw::Image>> = [(0, crate::icons::app_icon("org.gnome.Nautilus", small)), (2, crate::icons::app_icon("org.gnome.Characters", small))].into();
        let bw = helix_draw::theme::grow(SEARCH_W).min(w as i32 - 48);
        let bx = (w as i32 - bw) / 2;
        let by = (h as i32 / 6).max(panel_height() as i32 + 24);
        let field = Rect { x: bx, y: by, w: bw, h: helix_draw::theme::grow(SEARCH_H) };
        search_field(canvas, text.as_ref(), field, &model.query, "", true);
        paint_results(canvas, (text.as_ref(), bold.as_ref()), &model, (&rows, &headings), (bx, by + field.h + 8, bw, h as i32 - 24), (Some(3), None), &mut Default::default());
    });
}

/// The launcher with more results than fit (helixdesktop/desktop#104,
/// `launcher-more-…`): at the top (a fade and "N more results" below),
/// scrolled into the middle (fades at both edges, the footer under the
/// pointer) and at the end ("N results above"); in every look, and right
/// to left (`launcher-more-rtl-…`).
#[test]
#[ignore]
fn launcher_more_results() {
    use crate::launcher::{App, Launcher, ListScroll};
    let Some(dir) = dir() else { return };
    let (text, bold) = (Text::load(), Text::load_matching("sans-serif:bold"));
    let names = ["Notes", "Notebook", "Note Taker", "Notation", "Noteworthy", "Notepad", "Notify", "Nota", "Notecase", "Noted"];
    let apps: Vec<App> = (0..24).map(|i| App { id: format!("org.example.N{i}.desktop"), name: format!("{} {}", names[i % names.len()], i / names.len() + 1), search: "note".into(), categories: Vec::new() }).collect();
    let mut model = Launcher::new(apps);
    model.type_text("not");
    let icons: HashMap<usize, Option<helix_draw::Image>> = HashMap::new();
    let scene = |canvas: &mut Canvas, (w, h): (usize, usize), model: &Launcher, scrolled: isize, hover: Option<usize>| {
        canvas.fill(SCRIM);
        let bw = helix_draw::theme::grow(SEARCH_W).min(w as i32 - 48);
        let bx = (w as i32 - bw) / 2;
        let by = (h as i32 / 6).max(panel_height() as i32 + 24);
        let field = Rect { x: bx, y: by, w: bw, h: helix_draw::theme::grow(SEARCH_H) };
        search_field(canvas, text.as_ref(), field, &model.query, "", true);
        let mut scroll = ListScroll::default();
        let area = (bx, by + field.h + 8, bw, h as i32 - 24);
        let paint = |canvas: &mut Canvas, scroll: &mut ListScroll| paint_results(canvas, (text.as_ref(), bold.as_ref()), model, (&icons, &icons), area, (hover, None), scroll);
        if scrolled != 0 {
            // Placed once, then scrolled (as the wheel does).
            let mut scratch = vec![0u8; w * h * 4];
            let mut off = Canvas::scaled(&mut scratch, w, h, 1.0);
            paint(&mut off, &mut scroll);
            scroll.scroll_by(scrolled);
        }
        paint(canvas, &mut scroll);
    };
    for (what, scrolled, hover) in [("top", 0, Some(2)), ("middle", 6, Some(FOOTER_HIT)), ("end", 100, None)] {
        in_looks(&dir, &format!("launcher-more-{what}"), (1280, 720), &mut |canvas, size| scene(canvas, size, &model, scrolled, hover));
    }
    helix_draw::set_rtl(true);
    helix_draw::theme::set_text_scale(1.5);
    render(&dir, "launcher-more-rtl-text1.5", DARK, (1280, 720, 1.0), |canvas| scene(canvas, (1280, 720), &model, 6, Some(FOOTER_HIT)));
    helix_draw::theme::set_text_scale(1.0);
    render(&dir, "launcher-more-rtl", DARK, (1280, 720, 1.0), |canvas| scene(canvas, (1280, 720), &model, 0, None));
    helix_draw::set_rtl(false);
}

/// Status icons in the panel (helixdesktop/desktop#106, `panel-tray-…`), at
/// scale 1 and 2, as a Blueman item naming `blueman-disabled` gets them:
/// 1. through Yaru (which has fixed-size PNGs only, and hicolor a
///    `-symbolic` form): the symbolic form in the panel's color;
/// 2. as it was drawn before: Yaru's 16 px PNG 1:1 (stretched at scale 2);
/// 3. from an item theme directory holding only Yaru's 16, 32 and 48 px
///    PNGs: the 32 shrunk to 16 at scale 1, the 48 to 32 at scale 2;
/// 4. a test theme's icon with only 16 (red), 32 (green) and 48 (blue) px
///    PNGs: green at scale 1, blue at scale 2;
/// 5. `bluetooth-active` through Yaru: its symbolic form.
#[test]
#[ignore]
fn panel_tray_icons() {
    let Some(dir) = dir() else { return };
    let text = Text::load();
    let theme = std::env::temp_dir().join(format!("helix-gallery-tray-{}", std::process::id()));
    for (size, color) in [(16, [200, 40, 40, 255]), (32, [40, 160, 60, 255]), (48, [40, 90, 210, 255])] {
        crate::icons::tests::write_png(&theme.join(format!("test/{size}x{size}/status/test-tray.png")), size, color);
        let yaru = std::path::PathBuf::from(format!("/usr/share/icons/Yaru/{size}x{size}/status/blueman-disabled.png"));
        let copy = theme.join(format!("yaru/{size}x{size}/status/test-yaru-tray.png"));
        std::fs::create_dir_all(copy.parent().unwrap()).unwrap();
        let _ = std::fs::copy(&yaru, &copy);
    }
    let rgb = [(FG >> 16) as u8, (FG >> 8) as u8, FG as u8];
    for scale in [1.0, 2.0] {
        let px = (f64::from(status::ICON) * scale).round() as u32;
        let yaru = |name: &str| crate::icons::status_icon_in(name, None, px, rgb, "Yaru");
        let old = crate::icons::find_icon("blueman-disabled", 16, "Yaru").and_then(|p| crate::icons::decode_file(&p, 16));
        // (Renamed, so that no theme has a symbolic form of it.)
        let copies = crate::icons::status_icon_in("test-yaru-tray", Some(&theme.join("yaru")), px, rgb, "none");
        let test = crate::icons::status_icon_in("test-tray", Some(&theme.join("test")), px, rgb, "none");
        let tray = [(yaru("blueman-disabled"), None, false), (old, None, false), (copies, None, false), (test, None, false), (yaru("bluetooth-active"), None, false)];
        let (w, ph) = (1280, panel_height() as usize);
        render(&dir, &format!("panel-tray-scale{scale}"), DARK, (w, ph, scale), |canvas| {
            panel_tray(canvas, text.as_ref(), (w, ph), scale, (None, false, None, None), &tray);
        });
    }
    let _ = std::fs::remove_dir_all(&theme);
}

/// The screenshot UI (`shot-…`): an area with handles and its size, the
/// icon toolbar with a tooltip, recording with the capture button under
/// the pointer; in every look.
#[test]
#[ignore]
fn screenshot_ui() {
    let Some(dir) = dir() else { return };
    let text = Text::load();
    in_looks(&dir, "shot-area", (1280, 720), &mut |canvas, (w, h)| capture::gallery_scene(canvas, text.as_ref(), (w as i32, h as i32), false, 1, true));
    in_looks(&dir, "shot-record", (1280, 720), &mut |canvas, (w, h)| capture::gallery_scene(canvas, text.as_ref(), (w as i32, h as i32), true, 2, false));
}

/// The rest of the shared look in every look (`look-…`): a banner with
/// actions (one hovered), an authentication prompt and a run dialog with
/// a list, and the on-screen keyboard.
#[test]
#[ignore]
fn shared_look_more() {
    use crate::prompt::Prompt;
    let Some(dir) = dir() else { return };
    let (text, bold) = (Text::load(), Text::load_matching("sans-serif:bold"));
    in_looks(&dir, "look-banner", (1280, 360), &mut |canvas, (w, _)| {
        let banner = crate::notifications::Banner {
            summary: "Meeting starts now".into(),
            body: "Weekly planning in room 4 — the agenda is attached.".into(),
            buttons: vec![("snooze".into(), "Snooze".into()), ("join".into(), "Join".into())],
            ..notify::tests::note(7, "Calendar", "org.gnome.Calendar")
        };
        let bw = banner_w() as i32;
        let x = (w as i32 - bw) / 2;
        let mut scratch = vec![0u8; canvas.data.len()];
        let mut dry = Canvas::scaled(&mut scratch, w, 360, canvas.scale);
        let (_, bottom) = notify::paint_notification(&mut dry, (text.as_ref(), bold.as_ref()), None, &banner, (x + 16, 24), bw - 32 - 24, 2);
        let bh = bottom - 8 + helix_draw::theme::grow(52);
        theme::popover(canvas, x, 8, bw, bh);
        let (_, bottom) = notify::paint_notification(canvas, (text.as_ref(), bold.as_ref()), None, &banner, (x + 16, 24), bw - 32 - 24, 2);
        let bt = helix_draw::theme::grow(36);
        for (i, (_, l)) in banner.buttons.iter().enumerate() {
            let bx = x + 16 + i as i32 * ((bw - 40) / 2 + 8);
            let look = theme::Look { hovered: i == 1, ..Default::default() };
            theme::button(canvas, text.as_ref(), (bx, bottom + 8, (bw - 40) / 2, bt), l, theme::Kind::Normal, look, false);
        }
    });
    let auth = Prompt { title: "Authentication Required".into(), message: "Authentication is needed to change the system time.".into(), password: Some("Password".into()), choice: Some(("Remember for this session".into(), true)), continue_label: "Authenticate".into(), ..Default::default() };
    in_looks(&dir, "look-prompt", (1280, 720), &mut |canvas, size| {
        let view = prompt::sample_view(auth.clone());
        prompt::paint_prompt_view(canvas, (text.as_ref(), bold.as_ref()), size, &view, (Some(OverlayHit::Toggle), Some(OverlayHit::Cancel)));
    });
    let list = Prompt { title: "Open With".into(), message: "Choose an app for notes.txt".into(), list: vec!["Text Editor".into(), "Notes".into(), "Terminal".into()], selected: 1, ..Default::default() };
    in_looks(&dir, "look-prompt-list", (1280, 720), &mut |canvas, size| {
        let view = prompt::sample_view(list.clone());
        prompt::paint_prompt_view(canvas, (text.as_ref(), bold.as_ref()), size, &view, (Some(OverlayHit::Row(2)), None));
    });
}

#[test]
#[ignore]
fn dialogs() {
    let Some(dir) = dir() else { return };
    let text = Text::load();
    let blockers = vec![("Text Editor".to_owned(), "Unsaved document".to_owned()), ("Terminal".to_owned(), String::new())];
    let scenes: Vec<(&str, DialogState)> = vec![
        ("dialog-not-responding", DialogState::new(DialogKind::NotResponding { id: 1, title: "Text Editor".into() }, 0)),
        ("dialog-not-responding-tab", DialogState { focus: Some(Choice::Confirmed), ..DialogState::new(DialogKind::NotResponding { id: 1, title: "Text Editor".into() }, 0) }),
        ("dialog-remote-control", DialogState::new(DialogKind::RemoteControl { app: "Remote Desktop".into(), devices: vec!["keyboard".into(), "pointer".into()], reply: None }, 0)),
        ("dialog-power-off", DialogState::new(DialogKind::End(EndDialog { kind: SHUTDOWN, seconds: 60, blockers: Vec::new(), install: Some(true), upgrade: None, blocker_apps: Vec::new() }), 60)),
        ("dialog-power-off-install-focus", DialogState { install_focused: true, ..DialogState::new(DialogKind::End(EndDialog { kind: SHUTDOWN, seconds: 60, blockers: Vec::new(), install: Some(true), upgrade: None, blocker_apps: Vec::new() }), 60) }),
        ("dialog-power-off-blocked", DialogState::new(DialogKind::End(EndDialog { kind: SHUTDOWN, seconds: 60, blockers, install: None, upgrade: None, blocker_apps: vec!["org.gnome.TextEditor".into(), "org.gnome.Terminal".into()] }), 60)),
    ];
    let icons: Vec<Option<helix_draw::Image>> = ["org.gnome.TextEditor", "org.gnome.Terminal"].iter().map(|id| crate::icons::app_icon(id, BLOCKER_ICON as u32)).collect();
    for hc in [false, true] {
        helix_draw::set_high_contrast(hc);
        for (name, state) in &scenes {
            let name = if hc { format!("{name}-hc") } else { (*name).to_owned() };
            render(&dir, &name, DARK, (1280, 720, 1.0), |canvas| {
                paint_question_with(canvas, (text.as_ref(), Text::load_matching("sans-serif:bold").as_ref()), (1280, 720), state, &icons);
            });
        }
    }
    helix_draw::set_high_contrast(false);
}

/// The polkit prompt with who authenticates (`polkit-…`): the initials
/// (a name without a picture) and a picture; in every look.
#[test]
#[ignore]
fn polkit_identity() {
    use crate::prompt::Prompt;
    let Some(dir) = dir() else { return };
    let (text, bold) = (Text::load(), Text::load_matching("sans-serif:bold"));
    let picture = crate::user_identity::load_picture(std::path::Path::new("/usr/share/backgrounds/mendhak-Red_Acer.jpg")).map(Arc::new);
    for (tag, picture) in [("initials", None), ("picture", picture)] {
        let auth = Prompt {
            title: "Authentication Required".into(),
            message: "Authentication is needed to change the system time.".into(),
            password: Some("Password".into()),
            identity: Some(("Ann Smith".into(), picture)),
            continue_label: "Authenticate".into(),
            cancel_label: "Cancel".into(),
            ..Default::default()
        };
        in_looks(&dir, &format!("polkit-{tag}"), (1280, 720), &mut |canvas, size| {
            let view = prompt::sample_view(auth.clone());
            prompt::paint_prompt_view(canvas, (text.as_ref(), bold.as_ref()), size, &view, (None, None));
        });
    }
}

/// Quick settings and the notification list on short screens (logical
/// sizes of 1280×720 at 100 % and 150 %, 1366×768 at 125 %, 1280×800 at
/// 150 %).
#[test]
#[ignore]
fn short_screens() {
    use notify::tests::{paint_sample, sample as notes, sample_calendar};
    let Some(dir) = dir() else { return };
    let (text, bold) = (Text::load(), Text::load_matching("sans-serif:bold"));
    let entries = notes();
    let cal = sample_calendar(true);
    for (name, (w, h, scale)) in [("1280x720", (1280, 720, 1.0)), ("1280x720@1.5", (853, 480, 1.5)), ("1366x768@1.25", (1093, 614, 1.25)), ("1280x800@1.5", (853, 533, 1.5))] {
        render(&dir, &format!("short-quick-wifi-{name}"), DARK, (w, h, scale), |canvas| {
            quick::paint_quick(canvas, (text.as_ref(), Text::load_matching("sans-serif:bold").as_ref()), (w, h), &quick::tests::sample(Some(10)), None, &mut Default::default());
        });
        render(&dir, &format!("short-quick-power-off-{name}"), DARK, (w, h, scale), |canvas| {
            let focus = Some(OverlayHit::Network(9));
            quick::paint_quick(canvas, (text.as_ref(), Text::load_matching("sans-serif:bold").as_ref()), (w, h), &quick::tests::sample(Some(10)), focus, &mut Default::default());
        });
        render(&dir, &format!("short-list-{name}"), DARK, (w, h, scale), |canvas| {
            paint_sample(canvas, (text.as_ref(), bold.as_ref()), (w, h), &entries, &Default::default(), &cal, None);
        });
        render(&dir, &format!("short-list-media-{name}"), DARK, (w, h, scale), |canvas| {
            paint_sample(canvas, (text.as_ref(), bold.as_ref()), (w, h), &entries, &Default::default(), &cal, Some(&notify::ListHit::Media(crate::shell_keys::Playback::PlayPause)));
        });
    }
}

#[test]
#[ignore]
fn quick_settings() {
    use quick::tests::sample;
    let Some(dir) = dir() else { return };
    let text = Text::load();
    type Scene<'a> = (&'a str, (usize, usize, f64), quick::QuickModel, Option<OverlayHit>);
    let scenes: Vec<Scene> = vec![
        ("quick", (1280, 800, 1.0), sample(None), None),
        ("quick-focus-switch", (1280, 800, 1.0), sample(None), Some(OverlayHit::Switch(quick::Switch::Bluetooth))),
        ("quick-focus-slider", (1280, 800, 1.0), sample(None), Some(OverlayHit::VolumeSlider)),
        ("quick-wifi-focus-row", (1280, 800, 1.0), sample(Some(3)), Some(OverlayHit::Network(1))),
    ];
    for hc in [false, true] {
        helix_draw::set_high_contrast(hc);
        for (name, (w, h, scale), m, focus) in &scenes {
            let name = if hc { format!("{name}-hc") } else { (*name).to_owned() };
            render(&dir, &name, DARK, (*w, *h, *scale), |canvas| {
                quick::paint_quick(canvas, (text.as_ref(), Text::load_matching("sans-serif:bold").as_ref()), (*w, *h), m, *focus, &mut Default::default());
            });
        }
    }
    helix_draw::set_high_contrast(false);
}

#[test]
#[ignore]
fn notification_list() {
    use notify::tests::{paint_sample, sample, sample_calendar};
    use notify::ListHit;
    let Some(dir) = dir() else { return };
    let (text, bold) = (Text::load(), Text::load_matching("sans-serif:bold"));
    let entries = sample();
    let open: std::collections::HashSet<String> = ["app:org.example.Chat".to_owned()].into();
    let closed = std::collections::HashSet::new();
    let cal = sample_calendar(true);
    let (y, m, d) = cal.selected;
    let scenes: Vec<(&str, &std::collections::HashSet<String>, Option<ListHit>)> = vec![
        ("list", &closed, None),
        ("list-focus-row", &open, Some(ListHit::Entry(4))),
        ("list-focus-header", &closed, Some(ListHit::GroupToggle("app:org.example.Chat".into()))),
        ("list-focus-day", &closed, Some(ListHit::Day(y, m, d))),
        ("list-focus-media", &closed, Some(ListHit::Media(crate::shell_keys::Playback::PlayPause))),
    ];
    for hc in [false, true] {
        helix_draw::set_high_contrast(hc);
        for (name, expanded, focus) in &scenes {
            let name = if hc { format!("{name}-hc") } else { (*name).to_owned() };
            render(&dir, &name, DARK, (1280, 800, 1.0), |canvas| {
                paint_sample(canvas, (text.as_ref(), bold.as_ref()), (1280, 800), &entries, expanded, &cal, focus.as_ref());
            });
        }
    }
    helix_draw::set_high_contrast(false);
}

/// A symbolic icon in the panel's color.
fn symbolic(name: &str, logical: f64, scale: f64) -> Option<helix_draw::Image> {
    crate::icons::symbolic(name, (logical * scale).round() as u32, [0xED, 0xED, 0xEF])
}

/// The panel with everything on: workspaces, the a11y indicator, an
/// input source, sharing and recording, the list open.
fn panel(canvas: &mut Canvas, text: Option<&Text>, (w, h): (usize, usize), scale: f64, focus: Option<usize>, power_open: bool) {
    panel_with(canvas, text, (w, h), scale, focus, power_open, None, None);
}

/// [`panel`], with `hover` under the pointer and `menu_open`'s menu open.
#[allow(clippy::too_many_arguments)]
fn panel_with(canvas: &mut Canvas, text: Option<&Text>, (w, h): (usize, usize), scale: f64, focus: Option<usize>, power_open: bool, hover: Option<crate::panel_layout::Slot>, menu_open: Option<crate::panel_layout::Item>) {
    panel_tray(canvas, text, (w, h), scale, (focus, power_open, hover, menu_open), &[]);
}

/// Tray icons: (image, overlay, needs attention).
type TrayImages = [(Option<helix_draw::Image>, Option<helix_draw::Image>, bool)];

/// [`panel_with`], with status icons in the tray.
fn panel_tray(
    canvas: &mut Canvas,
    text: Option<&Text>,
    (w, h): (usize, usize),
    scale: f64,
    (focus, power_open, hover, menu_open): (Option<usize>, bool, Option<crate::panel_layout::Slot>, Option<crate::panel_layout::Item>),
    tray: &TrayImages,
) {
    let tray_w = match tray.len() as i32 {
        0 => 0,
        n => n * status::ICON + (n - 1) * tray::TRAY_GAP,
    };
    let bold = Text::load_matching("sans-serif:bold");
    let power_icon = symbolic("system-shutdown-symbolic", 16.0, scale);
    let status_items: Vec<status::StatusItem> =
        ["battery-level-80-symbolic", "audio-volume-high-symbolic", "network-wireless-signal-good-symbolic"].iter().filter_map(|n| symbolic(n, 16.0, scale).map(status::StatusItem::Icon)).collect();
    let layout = crate::panel_layout::Layout::default();
    let model = PanelModel {
        layout: &layout,
        clock: &crate::clock::top_bar(crate::clock::ClockFormat { weekday: true, ..Default::default() }, &gio::glib::DateTime::new(&gio::glib::TimeZone::local(), 2026, 11, 14, 14, 5, 0.0).unwrap()),
        status_items: &status_items,
        tray_images: tray,
        tray_w,
        ext_items: &[],
        ext_icons: &[],
        ext_widths: &[],
        locked: false,
        launcher_open: false,
        power_open,
        list_open: false,
        menu_open,
        unseen_dot: true,
        workspaces: (4, 1),
        a11y_shown: true,
        source_label: Some("en"),
        sharing: true,
        recording: true,
        drag_target: None,
        focus,
        hover,
        pressed: None,
        power_icon: power_icon.as_ref(),
        bold: bold.as_ref(),
    };
    paint_panel(canvas, text, (w, h), &model);
}

/// A menu like the window menu, with an entry selected.
fn menu_items() -> Vec<Option<window_menu::Item>> {
    use window_menu::{Choice, Item};
    let item = |label: &'static str, choice, checked| Some(Item { label: helix_i18n::tr(label).into(), choice, checked, active: false, sub: Vec::new() });
    vec![
        item("Minimize", Choice::Action(crate::windows::WindowAction::Minimize), None),
        item("Maximize", Choice::Action(crate::windows::WindowAction::Maximize), None),
        item("Move", Choice::Disabled, None),
        None,
        item("Always on Top", Choice::Action(crate::windows::WindowAction::Minimize), Some(true)),
        item("Always on Visible Workspace", Choice::OnAllWorkspaces(true), Some(false)),
        item("Move to Monitor Right", Choice::Submenu, None),
        None,
        item("Close", Choice::Action(crate::windows::WindowAction::Close), None),
    ]
}

/// Every surface at once on a 1280×720 screen: the panel, a menu, quick
/// settings, the notification list or a dialog, at a text scale.
#[test]
#[ignore]
fn text_scale() {
    use notify::tests::{paint_sample, sample, sample_calendar};
    let Some(dir) = dir() else { return };
    let (text, bold) = (Text::load(), Text::load_matching("sans-serif:bold"));
    let entries = sample();
    let open: std::collections::HashSet<String> = ["app:org.example.Chat".to_owned()].into();
    let cal = sample_calendar(true);
    for factor in [1.0, 1.25, 1.5] {
        helix_draw::theme::set_text_scale(factor);
        let (w, h) = (1280, 720);
        let ph = panel_height() as usize;
        let shot = |name: &str, paint: &mut dyn FnMut(&mut Canvas)| {
            render(&dir, &format!("{name}-text{factor}"), DARK, (w, h, 1.0), |canvas| {
                paint(canvas);
            });
        };
        shot("scaled-quick-menu", &mut |canvas| {
            quick::paint_quick(canvas, (text.as_ref(), Text::load_matching("sans-serif:bold").as_ref()), (w, h), &quick::tests::sample(None), None, &mut Default::default());
            window_menu::paint_menu(canvas, text.as_ref(), (w as i32, h as i32), (40, ph as i32 + 4), &menu_items(), Some(1), true, false);
            let mut data = vec![0u8; w * ph * 4];
            let mut bar = Canvas::scaled(&mut data, w, ph, 1.0);
            panel(&mut bar, text.as_ref(), (w, ph), 1.0, Some(6), true);
            blit(canvas, &data, w, ph);
        });
        shot("scaled-list", &mut |canvas| {
            paint_sample(canvas, (text.as_ref(), bold.as_ref()), (w, h), &entries, &open, &cal, None);
        });
        shot("scaled-banner", &mut |canvas| {
            // A banner as the shell draws it: measured, then its box and
            // contents, centered at the top.
            let banner = crate::notifications::Banner {
                summary: "Meeting starts now".into(),
                body: "Weekly planning in room 4 — the agenda is attached, please read it before we start.".into(),
                buttons: vec![("snooze".into(), "Snooze".into()), ("join".into(), "Join".into())],
                ..notify::tests::note(7, "Calendar", "org.gnome.Calendar")
            };
            let bw = banner_w() as i32;
            let x = (w as i32 - bw) / 2;
            let mut scratch = vec![0u8; w * h * 4];
            let mut dry = Canvas::scaled(&mut scratch, w, h, 1.0);
            let (_, bottom) = notify::paint_notification(&mut dry, (text.as_ref(), bold.as_ref()), None, &banner, (x + 16, 24), bw - 32 - 24, 2);
            let bh = bottom - 8 + helix_draw::theme::grow(52);
            theme::popover(canvas, x, 8, bw, bh);
            let (_, bottom) = notify::paint_notification(canvas, (text.as_ref(), bold.as_ref()), None, &banner, (x + 16, 24), bw - 32 - 24, 2);
            let bt = helix_draw::theme::grow(36);
            for (i, (_, l)) in banner.buttons.iter().enumerate() {
                let bx = x + 16 + i as i32 * ((bw - 40) / 2 + 8);
                theme::control(canvas, bx, bottom + 8, (bw - 40) / 2, bt, theme::Look::default());
                label(text.as_ref(), canvas, l, (bx + 12) as f32, baseline(text.as_ref(), bottom + 8, bt, 14.0), 14.0, FG, 100.0);
            }
        });
        shot("scaled-dialog", &mut |canvas| {
            let state = DialogState::new(DialogKind::End(EndDialog { kind: SHUTDOWN, seconds: 60, blockers: Vec::new(), install: Some(true), upgrade: None, blocker_apps: Vec::new() }), 60);
            paint_question(canvas, (text.as_ref(), Text::load_matching("sans-serif:bold").as_ref()), (w, h), &state);
        });
    }
    helix_draw::theme::set_text_scale(1.0);
}

/// Copy a `w`×`h` ARGB buffer (scale 1) over the top of `canvas`.
fn blit(canvas: &mut Canvas, data: &[u8], w: usize, h: usize) {
    for y in 0..h {
        for x in 0..w {
            let p = &data[(y * w + x) * 4..][..4];
            let color = u32::from_le_bytes([p[0], p[1], p[2], p[3]]);
            canvas.blend_px(x as i32, y as i32, color | 0xFF00_0000, 1.0);
        }
    }
}

#[test]
#[ignore]
fn panels() {
    let Some(dir) = dir() else { return };
    let text = Text::load();
    for hc in [false, true] {
        helix_draw::set_high_contrast(hc);
        use crate::panel_layout::{Item, Slot};
        let scenes = [
            ("panel", None, false, None, None),
            ("panel-focus-open", Some(6), true, None, None),
            ("panel-hover-clock", Some(0), false, Some(Slot::Item(Item::Clock)), None),
            // The input source and accessibility menus open: their pills lit.
            ("panel-sources-open", None, false, None, Some(Item::Keyboard)),
            ("panel-a11y-open", None, false, None, Some(Item::Accessibility)),
        ];
        for (name, focus, power, hover, menu) in scenes {
            let name = if hc { format!("{name}-hc") } else { name.to_owned() };
            let ph = panel_height() as usize;
            render(&dir, &name, DARK, (1280, ph, 1.0), |canvas| panel_with(canvas, text.as_ref(), (1280, ph), 1.0, focus, power, hover, menu));
        }
    }
    helix_draw::set_high_contrast(false);
}

#[test]
#[ignore]
fn menus() {
    let Some(dir) = dir() else { return };
    let text = Text::load();
    for hc in [false, true] {
        helix_draw::set_high_contrast(hc);
        let name = if hc { "menu-hc" } else { "menu" };
        render(&dir, name, DARK, (640, 400, 1.0), |canvas| {
            window_menu::paint_menu(canvas, text.as_ref(), (640, 400), (40, 40), &menu_items(), Some(1), false, false);
        });
        render(&dir, &format!("{name}-keyboard"), DARK, (640, 400, 1.0), |canvas| {
            window_menu::paint_menu(canvas, text.as_ref(), (640, 400), (40, 40), &menu_items(), Some(1), true, false);
        });
        // The desktop background's menu.
        render(&dir, &format!("{name}-background"), DARK, (640, 400, 1.0), |canvas| {
            window_menu::paint_menu(canvas, text.as_ref(), (640, 400), (200, 120), &background_menu::items(), Some(2), false, false);
        });
    }
    helix_draw::set_high_contrast(false);
}

/// The surfaces that show the accent, in several accents and in high
/// contrast (which keeps its own colors whatever the accent):
/// `accent-{name}-…`.
#[test]
#[ignore]
fn accents() {
    use helix_draw::accent::Accent;
    use notify::tests::{paint_sample, sample, sample_calendar};
    let Some(dir) = dir() else { return };
    let (text, bold) = (Text::load(), Text::load_matching("sans-serif:bold"));
    let entries = sample();
    let open: std::collections::HashSet<String> = ["app:org.example.Chat".to_owned()].into();
    let cal = sample_calendar(true);
    let (w, h) = (1280, 720);
    let looks: Vec<(String, Accent, bool)> = [Accent::Blue, Accent::Orange, Accent::Yellow, Accent::Slate, Accent::Purple, Accent::Teal]
        .into_iter()
        .map(|a| (a.name().to_owned(), a, false))
        .chain([("yellow-hc".to_owned(), Accent::Yellow, true)])
        .collect();
    for (name, accent, hc) in &looks {
        helix_draw::accent::set(*accent);
        helix_draw::set_high_contrast(*hc);
        let ph = panel_height() as usize;
        render(&dir, &format!("accent-{name}-quick-menu"), DARK, (w, h, 1.0), |canvas| {
            let focus = Some(OverlayHit::Switch(quick::Switch::Bluetooth));
            quick::paint_quick(canvas, (text.as_ref(), Text::load_matching("sans-serif:bold").as_ref()), (w, h), &quick::tests::sample(None), focus, &mut Default::default());
            window_menu::paint_menu(canvas, text.as_ref(), (w as i32, h as i32), (40, ph as i32 + 4), &menu_items(), Some(1), true, false);
            let mut data = vec![0u8; w * ph * 4];
            let mut bar = Canvas::scaled(&mut data, w, ph, 1.0);
            panel(&mut bar, text.as_ref(), (w, ph), 1.0, Some(6), true);
            blit(canvas, &data, w, ph);
        });
        render(&dir, &format!("accent-{name}-wifi"), DARK, (w, h, 1.0), |canvas| {
            quick::paint_quick(canvas, (text.as_ref(), Text::load_matching("sans-serif:bold").as_ref()), (w, h), &quick::tests::sample(Some(3)), Some(OverlayHit::Network(1)), &mut Default::default());
        });
        render(&dir, &format!("accent-{name}-list"), DARK, (w, h, 1.0), |canvas| {
            paint_sample(canvas, (text.as_ref(), bold.as_ref()), (w, h), &entries, &open, &cal, Some(&notify::ListHit::Entry(4)));
        });
        render(&dir, &format!("accent-{name}-dialog"), DARK, (w, h, 1.0), |canvas| {
            let state = DialogState::new(DialogKind::End(EndDialog { kind: SHUTDOWN, seconds: 60, blockers: Vec::new(), install: Some(true), upgrade: None, blocker_apps: Vec::new() }), 60);
            paint_question(canvas, (text.as_ref(), Text::load_matching("sans-serif:bold").as_ref()), (w, h), &state);
        });
        render(&dir, &format!("accent-{name}-dialog-focus"), DARK, (w, h, 1.0), |canvas| {
            let state = DialogState { focus: Some(Choice::Canceled), ..DialogState::new(DialogKind::NotResponding { id: 1, title: "Text Editor".into() }, 0) };
            paint_question(canvas, (text.as_ref(), Text::load_matching("sans-serif:bold").as_ref()), (w, h), &state);
        });
    }
    helix_draw::set_high_contrast(false);
    helix_draw::accent::set(Accent::Blue);
}

/// A dock on a bottom edge with every state shown: the focused window's
/// app (Files), one under the pointer (Calculator, with its tooltip), one
/// wanting attention with a count (Calendar), one downloading (Chrome),
/// the keyboard on Terminal, and an app without an icon (the generic one).
pub(super) fn sample_dock(icons: &mut crate::icon_cache::IconCache, (w, h): (i32, i32), scale: f64) -> dock::DockPaint {
    use crate::dock::{Edge, Indicator, Item};
    let ids = ["org.gnome.Nautilus", "org.gnome.Calculator", "org.gnome.Calendar", "com.google.Chrome", "org.gnome.Terminal", "org.example.NoIcon"];
    let mut items: Vec<Item> = ids.iter().map(|id| Item::App { id: (*id).into(), favorite: true }).collect();
    items.push(Item::Trash);
    items.push(Item::ShowApps);
    let icon = 48;
    let px = (f64::from(icon) * scale).round() as u32;
    let icons_of: Vec<Option<helix_draw::Image>> = items
        .iter()
        .map(|i| match i {
            Item::App { id, .. } => {
                let id = id.clone();
                icons.get(format!("app:{id}@{px}"), move || crate::icons::app_icon(&id, px))
            }
            Item::Trash => icons.get(format!("named:user-trash@{px}"), move || crate::icons::load(&crate::icons::IconSource::Names(vec!["user-trash".into()]), px)),
            _ => None,
        })
        .collect();
    let n = items.len();
    let look = |highlight: f64| crate::item_feedback::Look { highlight, pressed: false, scale: if highlight > 0.0 { crate::item_feedback::HOVER_SCALE } else { 1.0 } };
    let mut looks = vec![look(0.0); n];
    looks[1] = look(1.0);
    let mut progress = vec![None; n];
    progress[3] = Some(0.45);
    let mut urgent = vec![false; n];
    urgent[2] = true;
    let mut counts = vec![(0, 0); n];
    counts[0] = (2, 0);
    counts[1] = (1, 0);
    counts[2] = (1, 4);
    counts[3] = (3, 0);
    counts[4] = (1, 0);
    dock::DockPaint {
        size: (w, h),
        edge: Edge::Bottom,
        extend: false,
        always_center: true,
        indicator: Indicator::Dots,
        icon,
        thickness: icon + icon / 4 + 8,
        background: 0xCC,
        items,
        icons: icons_of,
        counts,
        progress,
        urgent,
        wiggles: vec![0; n],
        numbers: vec![None; n],
        focused_item: Some(0),
        looks,
        ghosts: Vec::new(),
        drag: None,
        tooltip: Some((1, vec!["Calculator".into(), "Calculator — 12 × 7".into()])),
        keyboard: Some(4),
    }
}

/// The dock in its states, in several accents, in high contrast and at
/// text scale 1.5 (`dock-…`), and app grid cells (`grid-…`): one under
/// the pointer, one with the keyboard focus, one pressed, an app without
/// an icon.
#[test]
#[ignore]
fn dock_and_grid() {
    use helix_draw::accent::Accent;
    let Some(dir) = dir() else { return };
    let text = Text::load();
    let mut icons = crate::icon_cache::IconCache::default();
    let (w, h) = (760, 200);
    let looks: Vec<(String, Accent, bool, f64)> = [Accent::Blue, Accent::Yellow, Accent::Orange, Accent::Slate, Accent::Green]
        .into_iter()
        .map(|a| (a.name().to_owned(), a, false, 1.0))
        .chain([("hc".to_owned(), Accent::Blue, true, 1.0), ("text1.5".to_owned(), Accent::Blue, false, 1.5)])
        .collect();
    let apps = ["org.gnome.Nautilus", "org.gnome.Calculator", "org.gnome.Calendar", "org.gnome.Terminal", "org.example.NoIcon"]
        .iter()
        .map(|id| crate::launcher::App { id: format!("{id}.desktop"), name: id.rsplit('.').next().unwrap().to_owned(), search: String::new(), categories: Vec::new() })
        .collect();
    let model = crate::launcher::Launcher::new(apps);
    for (name, accent, hc, factor) in &looks {
        helix_draw::accent::set(*accent);
        helix_draw::set_high_contrast(*hc);
        helix_draw::theme::set_text_scale(*factor);
        render(&dir, &format!("dock-{name}"), DARK, (w, h, 1.0), |canvas| {
            let m = sample_dock(&mut icons, (w as i32, h as i32), 1.0);
            dock::paint_dock(canvas, text.as_ref(), &m);
        });
        render(&dir, &format!("grid-{name}"), DARK, (w, 200, 1.0), |canvas| {
            canvas.fill(SCRIM);
            let mut p = app_grid::Paint { cache: &mut icons, text: text.as_ref(), bold: None, scale: 1.0 };
            let lit = crate::item_feedback::Look { highlight: 1.0, pressed: false, scale: crate::item_feedback::HOVER_SCALE };
            let rest = crate::item_feedback::Look { highlight: 0.0, pressed: false, scale: 1.0 };
            let pressed = crate::item_feedback::Look { highlight: 1.0, pressed: true, scale: 1.0 };
            for (i, app) in model.apps.iter().enumerate() {
                let cell = Rect { x: 20 + i as i32 * 140, y: 30, w: 120, h: 118 };
                let (look, focused) = match i {
                    1 => (lit, false),
                    2 => (lit, true),
                    3 => (pressed, false),
                    _ => (rest, false),
                };
                app_grid::paint_cell(&mut p, &model, canvas, cell, &app.id, look, focused);
            }
        });
    }
    helix_draw::theme::set_text_scale(1.0);
    helix_draw::set_high_contrast(false);
    helix_draw::accent::set(Accent::Blue);
}

/// On-screen displays: volume, brightness with a label, above 100 %,
/// words only; in an accent, high contrast and text scale 1.5 (`osd-…`).
#[test]
#[ignore]
fn osds() {
    use helix_draw::accent::Accent;
    let Some(dir) = dir() else { return };
    let text = Text::load();
    let osd = |icon: &str, label: Option<&str>, level: Option<f64>, max: f64| Osd { icon: Some(icon.into()), label: label.map(Into::into), level, max_level: max, connector: None };
    let scenes = [
        ("volume", osd("audio-volume-medium-symbolic", None, Some(0.45), 1.0)),
        ("brightness-label", osd("display-brightness-symbolic", Some("Built-in display"), Some(0.8), 1.0)),
        ("overdrive", osd("audio-volume-overamplified-symbolic", None, Some(1.2), 1.5)),
        ("mic-off", osd("microphone-disabled-symbolic", Some("Microphone off"), None, 1.0)),
    ];
    for (look, accent, hc, factor) in [("", Accent::Blue, false, 1.0), ("-orange", Accent::Orange, false, 1.0), ("-hc", Accent::Blue, true, 1.0), ("-text1.5", Accent::Blue, false, 1.5)] {
        helix_draw::accent::set(accent);
        helix_draw::set_high_contrast(hc);
        helix_draw::theme::set_text_scale(factor);
        for (name, o) in &scenes {
            let icon = o.icon.as_deref().map(osd::osd_icon_names).and_then(|names| names.iter().find_map(|n| symbolic(n, 24.0, 1.0)));
            let (w, h) = (helix_draw::theme::grow(300), helix_draw::theme::grow(if o.level.is_some() && o.label.is_some() { 72 } else { 52 }));
            render(&dir, &format!("osd-{name}{look}"), DARK, (w as usize + 40, h as usize + 40, 1.0), |canvas| {
                let mut data = vec![0u8; (w * h * 4) as usize];
                let mut pill = Canvas::scaled(&mut data, w as usize, h as usize, 1.0);
                osd::paint_osd(&mut pill, text.as_ref(), icon.as_ref(), o, (w, h));
                for y in 0..h {
                    for x in 0..w {
                        let p = &data[((y * w + x) * 4) as usize..][..4];
                        let a = p[3];
                        if a > 0 {
                            // Premultiplied: back to a straight color for blending.
                            let un = |c: u8| (u32::from(c) * 255 / u32::from(a)).min(255);
                            let color = (u32::from(a) << 24) | (un(p[2]) << 16) | (un(p[1]) << 8) | un(p[0]);
                            canvas.blend_px(x + 20, y + 20, color, 1.0);
                        }
                    }
                }
            });
        }
    }
    helix_draw::theme::set_text_scale(1.0);
    helix_draw::set_high_contrast(false);
    helix_draw::accent::set(Accent::Blue);
}

/// The on-screen keyboard: at rest, a character held (pressed, preview),
/// Shift latched, the symbols page; high contrast and an accent too
/// (`osk-…`).
#[test]
#[ignore]
fn on_screen_keyboard() {
    use helix_draw::accent::Accent;
    use osk::{OskPaint, Page, Special};
    let Some(dir) = dir() else { return };
    let text = Text::load();
    let icons: Vec<(Special, Option<helix_draw::Image>)> = [
        (Special::Backspace, "edit-clear-symbolic"),
        (Special::Left, "go-previous-symbolic"),
        (Special::Right, "go-next-symbolic"),
        (Special::Hide, "input-keyboard-symbolic"),
        (Special::Page(Page::Emoji), "face-smile-symbolic"),
    ]
    .into_iter()
    .map(|(s, n)| (s, symbolic(n, 18.0, 1.0)))
    .collect();
    let letters = osk::drawable_rows(osk::page_rows(Page::Letters, "us", ""), Page::Letters, text.as_ref());
    let symbols = osk::drawable_rows(osk::page_rows(Page::Symbols, "us", ""), Page::Symbols, text.as_ref());
    let bar = vec!["hello".to_owned(), "help".to_owned(), "helmet".to_owned()];
    let (w, h) = (1280usize, (40 + 8 + letters.len() as i32 * 52 + 8) as usize);
    for (look, accent, hc) in [("", Accent::Blue, false), ("-orange", Accent::Orange, false), ("-hc", Accent::Blue, true)] {
        helix_draw::accent::set(accent);
        helix_draw::set_high_contrast(hc);
        let scenes: Vec<(&str, OskPaint)> = vec![
            ("rest", OskPaint { rows: &letters, page: Page::Letters, shift: false, bar: &bar, accents: false, down: None, caps: false, released: None }),
            ("held-e", OskPaint { rows: &letters, page: Page::Letters, shift: false, bar: &bar, accents: false, down: Some((1, 2)), caps: false, released: None }),
            ("shift", OskPaint { rows: &letters, page: Page::Letters, shift: true, bar: &[], accents: false, down: None, caps: false, released: None }),
            ("symbols", OskPaint { rows: &symbols, page: Page::Symbols, shift: false, bar: &[], accents: false, down: None, caps: false, released: None }),
            ("caps", OskPaint { rows: &letters, page: Page::Letters, shift: false, bar: &[], accents: false, down: None, caps: true, released: None }),
            ("released-e", OskPaint { rows: &letters, page: Page::Letters, shift: false, bar: &bar, accents: false, down: None, caps: false, released: Some(((1, 2), 0.5)) }),
        ];
        for (name, m) in &scenes {
            render(&dir, &format!("osk-{name}{look}"), DARK, (w, h, 1.0), |canvas| {
                osk::paint_keyboard(canvas, (text.as_ref(), None), &icons, m, (w as i32, h as i32));
            });
        }
    }
    // A key held, in every look.
    let held = OskPaint { rows: &letters, page: Page::Letters, shift: false, bar: &bar, accents: false, down: Some((1, 2)), caps: false, released: None };
    in_looks(&dir, "look-osk", (w, h), &mut |canvas, (w, h)| {
        osk::paint_keyboard(canvas, (text.as_ref(), None), &icons, &held, (w as i32, h as i32));
    });
    helix_draw::set_high_contrast(false);
    helix_draw::accent::set(Accent::Blue);
}

/// The shell in the environment's locale (helixdesktop/spec
/// `components/i18n.md`): `HELIX_LOCALEDIR`, `LANGUAGE` and `LANG` (and
/// `LOCPATH`) pick the catalogue, names and formats, and a right-to-left
/// translation mirrors the layouts. `HELIX_GALLERY_TAG=he` names the
/// files `locale-he-…`. See TRANSLATING.md, "Testing layouts".
#[test]
#[ignore]
fn locale() {
    use notify::tests::{paint_sample, sample, sample_calendar};
    let Some(dir) = dir() else { return };
    helix_i18n::init_for_tests();
    helix_draw::set_rtl(helix_i18n::is_rtl());
    let tag = std::env::var("HELIX_GALLERY_TAG").unwrap_or_else(|_| "locale".into());
    let (text, bold) = (Text::load(), Text::load_matching("sans-serif:bold"));
    let (w, h) = (1280, 720);
    let ph = panel_height() as usize;
    let rtl = helix_draw::rtl();
    let name = |what: &str| format!("locale-{tag}-{what}");
    // The panel is its own surface: drawn (mirrored or not) on its own
    // buffer, then copied over the screen as it is.
    let bar = || {
        let mut data = vec![0u8; w * ph * 4];
        let mut bar = Canvas::scaled(&mut data, w, ph, 1.0).mirrored(rtl);
        panel(&mut bar, text.as_ref(), (w, ph), 1.0, None, true);
        data
    };
    let copy = |canvas: &mut Canvas, data: &[u8]| {
        for (i, p) in data.as_chunks::<4>().0.iter().enumerate() {
            let a = u32::from(p[3]);
            let d = &mut canvas.data[i * 4..i * 4 + 4];
            for k in 0..4 {
                d[k] = (u32::from(p[k]) + u32::from(d[k]) * (255 - a) / 255).min(255) as u8;
            }
        }
    };
    let panel_data = bar();
    render(&dir, &name("panel-quick-menu"), DARK, (w, h, 1.0), |canvas| {
        quick::paint_quick(canvas, (text.as_ref(), Text::load_matching("sans-serif:bold").as_ref()), (w, h), &quick::tests::sample(Some(3)), None, &mut Default::default());
        window_menu::paint_menu(canvas, text.as_ref(), (w as i32, h as i32), (40, ph as i32 + 4), &menu_items(), Some(1), true, false);
        copy(canvas, &panel_data);
    });
    let entries = sample();
    // The week starts on the locale's first weekday, as in the shell.
    let cal = sample_calendar(true).with_locale_week_start();
    render(&dir, &name("list"), DARK, (w, h, 1.0), |canvas| {
        paint_sample(canvas, (text.as_ref(), bold.as_ref()), (w, h), &entries, &Default::default(), &cal, None);
        copy(canvas, &panel_data);
    });
    let blockers = vec![("Text Editor".to_owned(), "Unsaved document".to_owned())];
    let dialogs: Vec<(&str, DialogState)> = vec![
        ("dialog-power-off", DialogState::new(DialogKind::End(EndDialog { kind: SHUTDOWN, seconds: 45, blockers: Vec::new(), install: Some(true), upgrade: None, blocker_apps: Vec::new() }), 45)),
        ("dialog-power-off-blocked", DialogState::new(DialogKind::End(EndDialog { kind: SHUTDOWN, seconds: 45, blockers, install: None, upgrade: None, blocker_apps: Vec::new() }), 45)),
        ("dialog-not-responding", DialogState::new(DialogKind::NotResponding { id: 1, title: "Text Editor".into() }, 0)),
    ];
    for (what, state) in &dialogs {
        render(&dir, &name(what), DARK, (w, h, 1.0), |canvas| {
            paint_question(canvas, (text.as_ref(), Text::load_matching("sans-serif:bold").as_ref()), (w, h), state);
        });
    }
    let osd = Osd { icon: Some("audio-volume-medium-symbolic".into()), label: Some(helix_i18n::tr("Volume").into()), level: Some(0.45), max_level: 1.0, connector: None };
    let icon = symbolic("audio-volume-medium-symbolic", 24.0, 1.0);
    render(&dir, &name("osd"), DARK, (340, 112, 1.0), |canvas| {
        osd::paint_osd(canvas, text.as_ref(), icon.as_ref(), &osd, (340, 112));
    });
    helix_draw::set_rtl(false);
}

/// The file chooser (`files-…`, helixdesktop/desktop#113, #115, #116): Open in a
/// long path with 200 entries, ten places, five bookmarks and two
/// drives; Save; an empty folder and one loading; at 1280×720 and
/// 1920×1080 in every look, right to left, text 1.5 on 1280×800 and
/// scale 1.5 on a 1280×720 screen.
#[test]
#[ignore]
fn file_chooser() {
    use crate::file_chooser::{Mode, Options};
    use file_chooser::{paint_files, sample_view, Paint, Shown};
    use helix_settings::files::{Entry, Place, PlaceKind};
    let Some(dir) = dir() else { return };
    let (text, bold) = (Text::load(), Text::load_matching("sans-serif:bold"));
    let home = std::path::PathBuf::from("/home/ada");
    let folder = home.join("Documents/Client — Contoso Ltd/2026");
    let now = 1_763_300_000i64;
    let mut entries = Vec::new();
    let mut add = |name: String, dir: bool, size: u64, age: i64| {
        let content_type = if dir { "inode/directory".to_owned() } else { helix_settings::files::fast_content_type(&name) };
        entries.push(Entry { path: folder.join(&name), name, dir, size, modified: now - age, content_type });
    };
    for (i, f) in ["Archive", "Contracts", "Invoices", "Meeting notes", "Photos", "Receipts", "Scans", "Tax 2025", "Tax 2026", "Templates"].iter().enumerate() {
        add((*f).into(), true, 0, 86_400 * (i as i64 * 9 + 1));
    }
    add("Quarterly financial report for the board of directors — final revised version after legal review (3) copy.pdf".into(), false, 2_400_000, 600);
    add("Quarterly financial report for the board of directors — final revised version after legal review (4).pdf".into(), false, 2_500_000, 3_600);
    add("Résumé – Zoë Ångström (español, 日本語).odt".into(), false, 48_000, 86_400 + 600);
    for i in 1..=60 {
        add(format!("Invoice-{:05}.pdf", 20_240 + i), false, i * 3_000, i as i64 * 40_000);
    }
    for i in 1..=40 {
        add(format!("IMG_2026{:04}.jpg", 1_100 + i), false, 800_000 + i * 1_000, i as i64 * 70_000);
    }
    for i in 1..=40 {
        add(format!("notes-{i:02}.md"), false, 120 + i, i as i64 * 90_000);
    }
    for i in 1..=27 {
        add(format!("slides-{i}.odp"), false, 1_000_000 + i * 10_000, i as i64 * 400_000);
    }
    for i in 1..=20 {
        add(format!("Budget {}.xlsx", 2005 + i), false, 30_000 + i * 100, 31_000_000 + i as i64 * 1_000_000);
    }
    assert_eq!(entries.len(), 200);
    let place = |kind, label: &str, path: &str| Place { kind, label: label.into(), path: (!path.is_empty()).then(|| path.into()) };
    let mut places = vec![place(PlaceKind::Recent, "", ""), place(PlaceKind::Home, "", "/home/ada")];
    for d in ["Desktop", "Documents", "Downloads", "Music", "Pictures", "Videos"] {
        places.push(place(PlaceKind::Standard, d, &format!("/home/ada/{d}")));
    }
    for b in ["Northwind", "Contoso", "Thesis", "Photos 2026", "Invoices"] {
        places.push(place(PlaceKind::Bookmark, b, &format!("/home/ada/Documents/{b}")));
    }
    places.push(place(PlaceKind::Drive, "SANDISK", "/media/ada/SANDISK"));
    places.push(place(PlaceKind::Drive, "Backup Disk", "/media/ada/Backup Disk"));
    places.push(place(PlaceKind::Computer, "", "/"));
    let filters = vec![("Images".into(), vec![(1, "image/png".into()), (1, "image/jpeg".into())]), ("PDF documents".into(), vec![(0, "*.pdf".into())]), ("All Files".into(), vec![(0, "*".into())])];
    let open = Options { title: "Open Document".into(), accept: "Open".into(), mode: Mode::Open, multiple: false, folder: Some(folder.clone()), name: String::new(), filters: filters.clone(), filter: Some(2), check: None, places: places.clone() };
    let save = Options { title: "Save Document".into(), accept: "Save".into(), mode: Mode::Save, name: "Untitled document.odt".into(), check: Some(("Compress".into(), false)), ..open.clone() };
    let mut cache: HashMap<String, Option<helix_draw::Image>> = HashMap::new();
    let mut scene = |name: &str, (w, h): (usize, usize), options: &Options, entries: Vec<Entry>, tune: &dyn Fn(&mut file_chooser::ChooserView)| {
        let mut paint_one = |canvas: &mut Canvas, (w, h): (usize, usize)| {
            let mut view = sample_view(options.clone(), Shown::Folder(folder.clone()), entries.clone(), &home);
            tune(&mut view);
            let scale = canvas.scale;
            let mut icon = |n: &str, color: u32| {
                let px = (16.0 * scale).round() as u32;
                cache.entry(format!("{n}@{px}#{color:x}")).or_insert_with(|| crate::icons::symbolic(n, px, [(color >> 16) as u8, (color >> 8) as u8, color as u8])).clone()
            };
            let mut p = Paint { text: text.as_ref(), bold: bold.as_ref(), icon: &mut icon, now: std::time::Instant::now(), now_unix: now };
            paint_files(canvas, &mut p, (w, h), &view, (None, None));
        };
        in_looks(&dir, name, (w, h), &mut paint_one);
        helix_draw::set_rtl(true);
        render(&dir, &format!("{name}-rtl"), DARK, (w, h, 1.0), |canvas| paint_one(canvas, (w, h)));
        helix_draw::set_rtl(false);
    };
    let pick_invoice = |v: &mut file_chooser::ChooserView| {
        let at = v.rows.iter().position(|&i| v.entries[i].name == "Invoice-20245.pdf");
        if let Some(r) = at {
            v.select(r, false, false);
        }
    };
    scene("files-open-1280x720", (1280, 720), &open, entries.clone(), &pick_invoice);
    scene("files-open-1920x1080", (1920, 1080), &open, entries.clone(), &pick_invoice);
    scene("files-save-1280x720", (1280, 720), &save, entries.clone(), &|_| {});
    scene("files-empty-1280x720", (1280, 720), &open, Vec::new(), &|_| {});
    scene("files-loading-1280x720", (1280, 720), &open, Vec::new(), &|v| v.loading = Some((1, std::time::Instant::now() - std::time::Duration::from_secs(1))));
    scene("files-menu-1280x720", (1280, 720), &open, entries.clone(), &|v| v.dropdown = true);
    scene("files-replace-1280x720", (1280, 720), &save, entries.clone(), &|v| v.replace = Some("/home/ada/Documents/Client — Contoso Ltd/2026/notes-01.md".into()));
    // Large text and scaling on small screens (helixdesktop/desktop#116).
    for (tag, (w, h, s)) in [("1280x800", (1280, 800, 1.0)), ("1280x720", (1280, 720, 1.0)), ("1280x720@1.5", (853, 480, 1.5))] {
        for (mode, options) in [("open", &open), ("save", &save)] {
            helix_draw::theme::set_text_scale(1.5);
            render(&dir, &format!("files-{mode}-{tag}-text1.5"), DARK, (w, h, s), |canvas| {
                let view = sample_view(options.clone(), Shown::Folder(folder.clone()), entries.clone(), &home);
                let mut icon = |n: &str, color: u32| crate::icons::symbolic(n, (16.0 * s).round() as u32, [(color >> 16) as u8, (color >> 8) as u8, color as u8]);
                let mut p = Paint { text: text.as_ref(), bold: bold.as_ref(), icon: &mut icon, now: std::time::Instant::now(), now_unix: now };
                paint_files(canvas, &mut p, (w, h), &view, (None, None));
            });
            helix_draw::theme::set_text_scale(1.0);
        }
    }
}
