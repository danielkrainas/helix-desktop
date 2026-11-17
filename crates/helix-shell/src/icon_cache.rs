//! Icons for the UI, decoded off the UI thread (so it keeps reading its
//! Wayland socket): asking for an icon that is not ready yet starts decoding it on a worker and gives nothing (the
//! caller draws without it); when it is done the UI is told, keeps it, and
//! draws again. The same icon is decoded once however often it is asked
//! for meanwhile.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use helix_draw::Image;

use crate::workers::Workers;

/// A decoded icon on its way back to the UI.
#[derive(Debug)]
pub struct Done {
    generation: u64,
    key: String,
    image: Option<Image>,
}

/// Where finished icons go (the UI thread's event loop).
pub type Sink = Arc<dyn Fn(Done) + Send + Sync>;

/// Icons by key (which says what and at what size): `None` when there is
/// none. Without workers (tests, pictures for the docs) icons are decoded
/// when asked for.
/// How many icons are kept: past it, the least recently used quarter is
/// forgotten (helixdesktop/desktop#120: keys made from a notification's
/// own picture, file or bytes, are new for nearly every notification, so
/// an unbounded cache grows for as long as the session runs).
const MAX_ICONS: usize = 768;

#[derive(Default)]
pub struct IconCache {
    /// The icons and when each was last asked for (a counter).
    done: HashMap<String, (Option<Image>, u64)>,
    used: u64,
    pending: HashSet<String>,
    /// Bumped by `clear`: icons decoded for an older one are dropped.
    generation: u64,
    workers: Option<(Workers, Sink)>,
}

impl IconCache {
    /// Decode on `workers`, telling `sink` about each icon done.
    pub fn spawning(workers: Workers, sink: Sink) -> Self {
        Self { workers: Some((workers, sink)), ..Self::default() }
    }

    /// The icon `key`, if ready; else `load` runs on a worker (unless it
    /// already is for `key`) and nothing is returned yet.
    pub fn get(&mut self, key: String, load: impl FnOnce() -> Option<Image> + Send + 'static) -> Option<Image> {
        self.used += 1;
        if let Some((found, used)) = self.done.get_mut(&key) {
            *used = self.used;
            return found.clone();
        }
        let Some((workers, sink)) = self.workers.as_mut() else {
            let image = load();
            self.keep(key, image.clone());
            return image;
        };
        if self.pending.insert(key.clone()) {
            let (generation, sink) = (self.generation, sink.clone());
            workers.run(move || {
                let image = load();
                sink(Done { generation, key, image });
            });
        }
        None
    }

    /// Whether `key` is being decoded.
    pub fn is_pending(&self, key: &str) -> bool {
        self.pending.contains(key)
    }

    /// An icon came back: true if it is still wanted (draw again).
    pub fn finish(&mut self, done: Done) -> bool {
        if done.generation != self.generation || !self.pending.remove(&done.key) {
            return false;
        }
        self.keep(done.key, done.image);
        true
    }

    /// Keep `image` as `key`, forgetting the least recently used quarter
    /// when there are too many.
    fn keep(&mut self, key: String, image: Option<Image>) {
        self.used += 1;
        self.done.insert(key, (image, self.used));
        if self.done.len() > MAX_ICONS {
            let mut ages: Vec<u64> = self.done.values().map(|(_, used)| *used).collect();
            ages.sort_unstable();
            let cut = ages[ages.len() / 4];
            self.done.retain(|_, (_, used)| *used > cut);
        }
    }

    /// How many icons are kept.
    pub fn len(&self) -> usize {
        self.done.len()
    }

    pub fn is_empty(&self) -> bool {
        self.done.is_empty()
    }

    /// Forget every icon (the icon theme changed): ones being decoded
    /// are dropped when they come back.
    pub fn clear(&mut self) {
        self.done.clear();
        self.pending.clear();
        self.generation += 1;
        if let Some((workers, _)) = self.workers.as_mut() {
            workers.cancel_queued();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{mpsc, Mutex};
    use std::time::Duration;

    fn image(v: u8) -> Image {
        Image { width: 1, height: 1, rgba: vec![v, v, v, 255] }
    }

    fn cache() -> (IconCache, mpsc::Receiver<Done>) {
        let (tx, rx) = mpsc::channel();
        let tx = Mutex::new(tx);
        let sink: Sink = Arc::new(move |done| {
            let _ = tx.lock().unwrap().send(done);
        });
        (IconCache::spawning(Workers::new("icons-test", 2), sink), rx)
    }

    /// The cache stays bounded and keeps what is still used.
    #[test]
    fn bounded() {
        let mut cache = IconCache::default();
        for i in 0..5000 {
            cache.get(format!("source:{i}@48"), || Some(image(1)));
            // One icon asked for all the time.
            cache.get("app:files@48".into(), || Some(image(2)));
        }
        assert!(cache.len() <= MAX_ICONS, "{} icons", cache.len());
        assert_eq!(cache.get("app:files@48".into(), || panic!("forgotten")), Some(image(2)));
    }

    #[test]
    fn nothing_first_then_the_icon_decoded_once() {
        let (mut cache, rx) = cache();
        let runs = Arc::new(AtomicUsize::new(0));
        let (gate_tx, gate) = mpsc::channel::<()>();
        let gate = Arc::new(Mutex::new(gate));
        let load = |runs: &Arc<AtomicUsize>, gate: &Arc<Mutex<mpsc::Receiver<()>>>| {
            let (runs, gate) = (runs.clone(), gate.clone());
            move || {
                runs.fetch_add(1, Ordering::SeqCst);
                let _ = gate.lock().unwrap().recv_timeout(Duration::from_secs(5));
                Some(image(7))
            }
        };
        // Asked for three times while it decodes (a redraw per pointer
        // motion): one decode.
        assert_eq!(cache.get("app:a@48".into(), load(&runs, &gate)), None, "placeholder: not ready");
        assert_eq!(cache.get("app:a@48".into(), load(&runs, &gate)), None);
        assert_eq!(cache.get("app:a@48".into(), load(&runs, &gate)), None);
        assert!(cache.is_pending("app:a@48"));
        gate_tx.send(()).unwrap();
        let done = rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(cache.finish(done), "wanted: draw again");
        assert_eq!(cache.get("app:a@48".into(), || panic!("decoded again")), Some(image(7)));
        assert_eq!(runs.load(Ordering::SeqCst), 1);
        assert!(rx.recv_timeout(Duration::from_millis(200)).is_err(), "no second decode");
    }

    #[test]
    fn not_found_is_remembered_too() {
        let (mut cache, rx) = cache();
        assert_eq!(cache.get("named:none@16".into(), || None), None);
        assert!(cache.finish(rx.recv_timeout(Duration::from_secs(5)).unwrap()));
        assert_eq!(cache.get("named:none@16".into(), || panic!("looked up again")), None);
        assert!(!cache.is_pending("named:none@16"));
    }

    #[test]
    fn icons_from_before_a_theme_change_are_dropped() {
        let (mut cache, rx) = cache();
        let (gate_tx, gate) = mpsc::channel::<()>();
        let (started_tx, started) = mpsc::channel::<()>();
        cache.get("app:a@48".into(), move || {
            let _ = started_tx.send(());
            let _ = gate.recv_timeout(Duration::from_secs(5));
            Some(image(1))
        });
        // Decoding (not just queued, which `clear` would drop) when the
        // theme changes.
        started.recv_timeout(Duration::from_secs(5)).unwrap();
        cache.clear();
        gate_tx.send(()).unwrap();
        let old = rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(!cache.finish(old), "decoded for the old theme");
        assert_eq!(cache.get("app:a@48".into(), || Some(image(2))), None, "decoded again for the new one");
        assert!(cache.finish(rx.recv_timeout(Duration::from_secs(5)).unwrap()));
        assert_eq!(cache.get("app:a@48".into(), || None), Some(image(2)));
    }

    #[test]
    fn without_workers_icons_load_at_once() {
        let mut cache = IconCache::default();
        assert_eq!(cache.get("k".into(), || Some(image(3))), Some(image(3)));
        assert_eq!(cache.get("k".into(), || panic!("cached")), Some(image(3)));
    }
}
