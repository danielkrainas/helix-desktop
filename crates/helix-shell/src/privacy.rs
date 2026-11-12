//! Microphone and camera indicators (helix-desktop-spec
//! `contracts/wayland/README.md`, "System status"): which PipeWire nodes
//! are running, watched on a thread of their own.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Mutex;

use pipewire as pw;

/// What matters about a node.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Node {
    pub class: String,
    pub role: String,
    pub api: String,
    pub capture_sink: bool,
    pub monitor: bool,
    pub running: bool,
}

impl Node {
    pub fn uses_microphone(&self) -> bool {
        self.running && self.class == "Stream/Input/Audio" && !self.capture_sink && !self.monitor
    }

    pub fn is_running_camera(&self) -> bool {
        self.running && self.class == "Video/Source" && (self.role == "Camera" || matches!(self.api.as_str(), "v4l2" | "libcamera"))
    }
}

/// (microphone in use, camera in use).
pub fn in_use<'a>(nodes: impl IntoIterator<Item = &'a Node>) -> (bool, bool) {
    nodes.into_iter().fold((false, false), |(mic, cam), n| (mic || n.uses_microphone(), cam || n.is_running_camera()))
}

static STATE: Mutex<(bool, bool)> = Mutex::new((false, false));

/// (microphone in use, camera in use) now.
pub fn current() -> (bool, bool) {
    *STATE.lock().unwrap_or_else(|e| e.into_inner())
}

type Bound = (pw::node::Node, pw::node::NodeListener);

/// Watch PipeWire for the rest of the process; `changed` runs when the
/// answer of [`current`] changes.
pub fn run(changed: impl Fn() + Send + 'static) {
    let spawned = std::thread::Builder::new().name("helix-privacy".into()).spawn(move || {
        pw::init();
        let setup = (|| -> Result<_, pw::Error> {
            let mainloop = pw::main_loop::MainLoopRc::new(None)?;
            let context = pw::context::ContextRc::new(&mainloop, None)?;
            let core = context.connect_rc(None)?;
            let registry = core.get_registry_rc()?;
            Ok((mainloop, context, core, registry))
        })();
        let (mainloop, _context, _core, registry) = match setup {
            Ok(parts) => parts,
            Err(err) => {
                tracing::info!(%err, "no PipeWire; no microphone or camera indicators");
                return;
            }
        };
        let nodes: Rc<RefCell<HashMap<u32, Node>>> = Rc::default();
        let bound: Rc<RefCell<HashMap<u32, Bound>>> = Rc::default();
        let changed = Rc::new(changed);
        let update = {
            let (nodes, changed) = (nodes.clone(), changed.clone());
            Rc::new(move || {
                let now = in_use(nodes.borrow().values());
                let mut state = STATE.lock().unwrap_or_else(|e| e.into_inner());
                if *state != now {
                    tracing::debug!(microphone = now.0, camera = now.1, "privacy indicators");
                    *state = now;
                    drop(state);
                    changed();
                }
            })
        };
        let _listener = registry
            .add_listener_local()
            .global({
                let (registry, nodes, bound, update) = (registry.clone(), nodes.clone(), bound.clone(), update.clone());
                move |global| {
                    if global.type_ != pw::types::ObjectType::Node {
                        return;
                    }
                    let node = match registry.bind::<pw::node::Node, _>(global) {
                        Ok(node) => node,
                        Err(err) => {
                            tracing::debug!(%err, id = global.id, "cannot follow a PipeWire node");
                            return;
                        }
                    };
                    let id = global.id;
                    let (nodes, update) = (nodes.clone(), update.clone());
                    let listener = node
                        .add_listener_local()
                        .info(move |info| {
                            {
                                let mut nodes = nodes.borrow_mut();
                                let entry = nodes.entry(id).or_default();
                                // Later events may carry only some properties.
                                if let Some(props) = info.props().filter(|_| info.change_mask().contains(pw::node::NodeChangeMask::PROPS)) {
                                    let set = |field: &mut String, key: &str| {
                                        if let Some(v) = props.get(key) {
                                            *field = v.to_owned();
                                        }
                                    };
                                    set(&mut entry.class, "media.class");
                                    set(&mut entry.role, "media.role");
                                    set(&mut entry.api, "device.api");
                                    if let Some(v) = props.get("stream.capture.sink") {
                                        entry.capture_sink = v == "true";
                                    }
                                    if let Some(v) = props.get("stream.monitor") {
                                        entry.monitor = v == "true";
                                    }
                                }
                                entry.running = matches!(info.state(), pw::node::NodeState::Running);
                            tracing::trace!(id, ?entry, "PipeWire node");
                            }
                            update();
                        })
                        .register();
                    bound.borrow_mut().insert(id, (node, listener));
                }
            })
            .global_remove({
                let (nodes, bound, update) = (nodes.clone(), bound.clone(), update.clone());
                move |id| {
                    bound.borrow_mut().remove(&id);
                    if nodes.borrow_mut().remove(&id).is_some() {
                        update();
                    }
                }
            })
            .register();
        mainloop.run();
    });
    if let Err(err) = spawned {
        tracing::warn!(%err, "cannot watch PipeWire");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(class: &str, f: impl FnOnce(&mut Node)) -> Node {
        let mut n = Node { class: class.into(), running: true, ..Node::default() };
        f(&mut n);
        n
    }

    #[test]
    fn indicators() {
        let recorder = node("Stream/Input/Audio", |_| {});
        let meter = node("Stream/Input/Audio", |n| n.monitor = true);
        let desktop_audio = node("Stream/Input/Audio", |n| n.capture_sink = true);
        let idle_recorder = node("Stream/Input/Audio", |n| n.running = false);
        let webcam = node("Video/Source", |n| n.api = "v4l2".into());
        let cast = node("Video/Source", |_| {});
        assert_eq!(in_use([&recorder]), (true, false));
        assert_eq!(in_use([&meter, &desktop_audio, &idle_recorder, &cast]), (false, false));
        assert_eq!(in_use([&webcam, &cast]), (false, true));
        assert!(node("Video/Source", |n| n.role = "Camera".into()).is_running_camera());
    }
}
