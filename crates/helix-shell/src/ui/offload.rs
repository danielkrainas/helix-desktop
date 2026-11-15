//! Work the UI thread hands to workers so it keeps reading its Wayland
//! socket (a compositor drops a client whose socket stays full): the
//! result comes back through the event loop and is applied there.
//! Icons decoded meanwhile are drawn once they are all in (or every
//! [`ICON_REDRAW`] while they come).

use std::time::Duration;

use smithay_client_toolkit::reexports::calloop::channel;
use smithay_client_toolkit::reexports::calloop::timer::{TimeoutAction, Timer};

use super::Ui;
use crate::workers::Workers;

/// A result, applied on the UI thread.
pub(super) type Finish = Box<dyn FnOnce(&mut Ui) + Send>;

/// Workers for pictures (blurred and scaled backgrounds, window shots)
/// and the way back to the UI. Without a way back, work runs at once.
#[derive(Default)]
pub(super) struct Offload {
    workers: Option<(Workers, channel::Sender<Finish>)>,
}

impl Offload {
    pub(super) fn new(done: channel::Sender<Finish>) -> Self {
        Self { workers: Some((Workers::new("pictures", 2), done)) }
    }
}

/// The icon `source` gives at `px` (a search result's, a notification's,
/// a desktop item's), cached by its picture; image data needs no decoding.
pub(super) fn source_icon(cache: &mut crate::icon_cache::IconCache, source: crate::icons::IconSource, px: u32) -> Option<helix_draw::Image> {
    if let crate::icons::IconSource::Data(image) = source {
        return Some(image);
    }
    let key = format!("source:{}@{px}", crate::icons::source_key(&source));
    cache.get(key, move || crate::icons::load(&source, px))
}

/// Icons arriving are drawn at most this often.
const ICON_REDRAW: Duration = Duration::from_millis(40);

impl Ui {
    /// Run `work` on a worker, then `then` with its result on the UI
    /// thread.
    pub(super) fn offload<T: Send + 'static>(&mut self, work: impl FnOnce() -> T + Send + 'static, then: impl FnOnce(&mut Ui, T) + Send + 'static) {
        match self.offload.workers.as_mut() {
            Some((workers, done)) => {
                let done = done.clone();
                workers.run(move || {
                    let result = work();
                    let _ = done.send(Box::new(move |ui: &mut Ui| then(ui, result)));
                });
            }
            None => {
                let result = work();
                then(self, result);
            }
        }
    }

    /// An icon came back from a worker.
    pub(super) fn icon_done(&mut self, done: crate::icon_cache::Done) {
        if !self.icon_cache.finish(done) || self.icon_redraw.is_some() {
            return;
        }
        let Some(handle) = self.loop_handle.clone() else {
            self.redraw_icons();
            return;
        };
        let timer = Timer::from_duration(ICON_REDRAW);
        self.icon_redraw = handle
            .insert_source(timer, |_, _, ui| {
                ui.icon_redraw = None;
                ui.redraw_icons();
                TimeoutAction::Drop
            })
            .ok();
    }

    /// Draw again what shows icons.
    fn redraw_icons(&mut self) {
        let _busy = crate::watchdog::scope("redraw with new icons");
        self.draw_panel();
        self.draw_launcher();
        self.draw_switcher();
        self.dock_draw();
        self.draw_osd();
        self.draw_notification_list();
        self.draw_banner();
        self.draw_desktop_icons();
        self.draw_osk();
        if self.window_menu.is_some() {
            self.draw_window_menu();
        }
        if self.overview.is_some() {
            self.draw_overview();
        }
        if self.lock.session.is_some() {
            self.draw_lock();
        }
    }
}
