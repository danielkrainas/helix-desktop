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
#[allow(dead_code)]
pub(super) const PHOTO: &str = "mendhak-Bluebells_Suspended_In_Time.jpg";

/// Render one scene: `paint` draws on a `w`×`h` canvas at `scale` over
/// `wall`.
/// The surface is painted on its own (transparent) buffer, then laid over
/// the wallpaper as the compositor would.
pub(super) fn render(dir: &std::path::Path, name: &str, wall: &str, (w, h, scale): (usize, usize, f64), paint: impl FnOnce(&mut Canvas)) {
    let mut data = wallpaper(wall, (w, h), scale);
    let mut surface = vec![0u8; data.len()];
    let mut canvas = Canvas::scaled(&mut surface, w, h, scale);
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
                paint_question(canvas, text.as_ref(), (1280, 720), state);
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
            quick::paint_quick(canvas, text.as_ref(), (w, h), &quick::tests::sample(Some(10)), None, &mut Default::default());
        });
        render(&dir, &format!("short-quick-power-off-{name}"), DARK, (w, h, scale), |canvas| {
            let focus = Some(OverlayHit::Network(9));
            quick::paint_quick(canvas, text.as_ref(), (w, h), &quick::tests::sample(Some(10)), focus, &mut Default::default());
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
                quick::paint_quick(canvas, text.as_ref(), (*w, *h), m, *focus, &mut Default::default());
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
    let status_items: Vec<status::StatusItem> =
        ["battery-level-80-symbolic", "audio-volume-high-symbolic", "network-wireless-signal-good-symbolic"].iter().filter_map(|n| symbolic(n, 16.0, scale).map(status::StatusItem::Icon)).collect();
    let layout = crate::panel_layout::Layout::default();
    let model = PanelModel {
        layout: &layout,
        clock: "Sat Nov 14  14:05",
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
        unseen_dot: true,
        workspaces: (4, 1),
        a11y_shown: true,
        source_label: Some("en"),
        sharing: true,
        recording: true,
        osd: None,
        drag_target: None,
        focus,
    };
    paint_panel(canvas, text, (w, h), &model);
}

/// A menu like the window menu, with an entry selected.
fn menu_items() -> Vec<Option<window_menu::Item>> {
    use window_menu::{Choice, Item};
    let item = |label: &str, choice, checked| Some(Item { label: label.into(), choice, checked, active: false, sub: Vec::new() });
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
            quick::paint_quick(canvas, text.as_ref(), (w, h), &quick::tests::sample(None), None, &mut Default::default());
            window_menu::paint_menu(canvas, text.as_ref(), (w as i32, h as i32), (40, ph as i32 + 4), &menu_items(), Some(1), false);
            let mut data = vec![0u8; w * ph * 4];
            let mut bar = Canvas::scaled(&mut data, w, ph, 1.0);
            panel(&mut bar, text.as_ref(), (w, ph), 1.0, Some(6), true);
            blit(canvas, &data, w, ph);
        });
        shot("scaled-list", &mut |canvas| {
            paint_sample(canvas, (text.as_ref(), bold.as_ref()), (w, h), &entries, &open, &cal, None);
        });
        shot("scaled-dialog", &mut |canvas| {
            let state = DialogState::new(DialogKind::End(EndDialog { kind: SHUTDOWN, seconds: 60, blockers: Vec::new(), install: Some(true), upgrade: None }), 60);
            paint_question(canvas, text.as_ref(), (w, h), &state);
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
        for (name, focus, power) in [("panel", None, false), ("panel-focus-open", Some(6), true)] {
            let name = if hc { format!("{name}-hc") } else { name.to_owned() };
            let ph = panel_height() as usize;
            render(&dir, &name, DARK, (1280, ph, 1.0), |canvas| panel(canvas, text.as_ref(), (1280, ph), 1.0, focus, power));
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
            window_menu::paint_menu(canvas, text.as_ref(), (640, 400), (40, 40), &menu_items(), Some(1), false);
        });
    }
    helix_draw::set_high_contrast(false);
}
