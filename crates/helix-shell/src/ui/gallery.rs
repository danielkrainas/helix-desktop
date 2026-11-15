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

/// Every surface on the shared look (helix-desktop-spec
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
        let state = DialogState { focus: Some(Choice::Canceled), ..DialogState::new(DialogKind::End(EndDialog { kind: SHUTDOWN, seconds: 60, blockers: Vec::new(), install: Some(true), upgrade: None }), 60) };
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
            let state = DialogState::new(DialogKind::End(EndDialog { kind: SHUTDOWN, seconds: 60, blockers: Vec::new(), install: Some(true), upgrade: None }), 60);
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
        let state = DialogState { hover: Some(OverlayHit::EndInstall), pressed: Some(OverlayHit::Cancel), ..DialogState::new(DialogKind::End(EndDialog { kind: SHUTDOWN, seconds: 60, blockers: Vec::new(), install: Some(false), upgrade: None }), 60) };
        paint_question(canvas, (text.as_ref(), bold.as_ref()), size, &state);
    });
    in_looks(&dir, "press-quick", (1280, 720), &mut |canvas, size| {
        let m = quick::QuickModel { hover: Some(OverlayHit::Network(1)), pressed: Some(OverlayHit::VolumeSlider), ..quick::tests::sample(Some(3)) };
        quick::paint_quick(canvas, (text.as_ref(), bold.as_ref()), size, &m, None, &mut Default::default());
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
        ("dialog-power-off", DialogState::new(DialogKind::End(EndDialog { kind: SHUTDOWN, seconds: 60, blockers: Vec::new(), install: Some(true), upgrade: None }), 60)),
        ("dialog-power-off-blocked", DialogState::new(DialogKind::End(EndDialog { kind: SHUTDOWN, seconds: 60, blockers, install: None, upgrade: None }), 60)),
    ];
    for hc in [false, true] {
        helix_draw::set_high_contrast(hc);
        for (name, state) in &scenes {
            let name = if hc { format!("{name}-hc") } else { (*name).to_owned() };
            render(&dir, &name, DARK, (1280, 720, 1.0), |canvas| {
                paint_question(canvas, (text.as_ref(), Text::load_matching("sans-serif:bold").as_ref()), (1280, 720), state);
            });
        }
    }
    helix_draw::set_high_contrast(false);
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
    let bold = Text::load_matching("sans-serif:bold");
    let power_icon = symbolic("system-shutdown-symbolic", 16.0, scale);
    let status_items: Vec<status::StatusItem> =
        ["battery-level-80-symbolic", "audio-volume-high-symbolic", "network-wireless-signal-good-symbolic"].iter().filter_map(|n| symbolic(n, 16.0, scale).map(status::StatusItem::Icon)).collect();
    let layout = crate::panel_layout::Layout::default();
    let model = PanelModel {
        layout: &layout,
        clock: &crate::clock::top_bar(crate::clock::ClockFormat { weekday: true, ..Default::default() }, &gio::glib::DateTime::new(&gio::glib::TimeZone::local(), 2026, 11, 14, 14, 5, 0.0).unwrap()),
        status_items: &status_items,
        tray_images: &[],
        tray_w: 0,
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
            let state = DialogState::new(DialogKind::End(EndDialog { kind: SHUTDOWN, seconds: 60, blockers: Vec::new(), install: Some(true), upgrade: None }), 60);
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
            let state = DialogState::new(DialogKind::End(EndDialog { kind: SHUTDOWN, seconds: 60, blockers: Vec::new(), install: Some(true), upgrade: None }), 60);
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
            let mut p = app_grid::Paint { cache: &mut icons, text: text.as_ref(), scale: 1.0 };
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
            ("rest", OskPaint { rows: &letters, page: Page::Letters, shift: false, bar: &bar, accents: false, down: None }),
            ("held-e", OskPaint { rows: &letters, page: Page::Letters, shift: false, bar: &bar, accents: false, down: Some((1, 2)) }),
            ("shift", OskPaint { rows: &letters, page: Page::Letters, shift: true, bar: &[], accents: false, down: None }),
            ("symbols", OskPaint { rows: &symbols, page: Page::Symbols, shift: false, bar: &[], accents: false, down: None }),
        ];
        for (name, m) in &scenes {
            render(&dir, &format!("osk-{name}{look}"), DARK, (w, h, 1.0), |canvas| {
                osk::paint_keyboard(canvas, (text.as_ref(), None), &icons, m, (w as i32, h as i32));
            });
        }
    }
    helix_draw::set_high_contrast(false);
    helix_draw::accent::set(Accent::Blue);
}

/// The shell in the environment's locale (helix-desktop-spec
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
    let cal = sample_calendar(true);
    render(&dir, &name("list"), DARK, (w, h, 1.0), |canvas| {
        paint_sample(canvas, (text.as_ref(), bold.as_ref()), (w, h), &entries, &Default::default(), &cal, None);
        copy(canvas, &panel_data);
    });
    let blockers = vec![("Text Editor".to_owned(), "Unsaved document".to_owned())];
    let dialogs: Vec<(&str, DialogState)> = vec![
        ("dialog-power-off", DialogState::new(DialogKind::End(EndDialog { kind: SHUTDOWN, seconds: 45, blockers: Vec::new(), install: Some(true), upgrade: None }), 45)),
        ("dialog-power-off-blocked", DialogState::new(DialogKind::End(EndDialog { kind: SHUTDOWN, seconds: 45, blockers, install: None, upgrade: None }), 45)),
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
