//! Agents' screenshots and synthetic input (helixdesktop/desktop#150),
//! through the compositor's `helix_screenshot_v1` (version 8):
//! `agent_capture` draws the picture with what the agent code did not
//! clear filled, and `helix_agent_input_v1` runs carry input into one
//! window, checked by the compositor before every event. The policy is
//! the agent code's (`crate::agent::screen`); this only carries requests.

use std::collections::{HashMap, VecDeque};
use std::os::fd::AsFd;
use std::sync::{Arc, Mutex};

use helix_protocols::client::helix_agent_input_v1::{self, HelixAgentInputV1};
use helix_protocols::client::helix_screenshot_frame_v1::{self, HelixScreenshotFrameV1, Reason};
use tokio::sync::oneshot;

use super::*;
use crate::agent::screen::{Request, Shot, ShotError, Step, Stopped};

/// What a run's object shares with its events.
#[derive(Default)]
pub(super) struct RunData {
    /// Replies to the steps sent and not yet answered, oldest first.
    pending: Mutex<VecDeque<oneshot::Sender<Result<(), Stopped>>>>,
    /// Why the run stopped, once it did.
    stopped: Mutex<Option<Stopped>>,
}

/// One input run.
pub(super) struct Run {
    object: HelixAgentInputV1,
    /// The agent it is for (the indicator's *Stop* ends it at once).
    agent: String,
    data: Arc<RunData>,
}

/// The input runs going on.
#[derive(Default)]
pub(super) struct AgentScreen {
    runs: HashMap<u64, Run>,
    next: u64,
}

/// What an agent capture's frame collects until it is done.
pub(super) struct ShotData {
    file: Mutex<Option<std::fs::File>>,
    filled: Mutex<u32>,
    reply: Mutex<Option<oneshot::Sender<Result<Shot, ShotError>>>>,
}

impl Ui {
    /// An agent's screen request.
    pub(super) fn agent_screen(&mut self, request: Request) {
        let manager = self.screenshots.as_ref().filter(|m| m.version() >= 8).cloned();
        match request {
            Request::Capture { window, area, shown, layers, reply } => {
                let Some(m) = manager else {
                    let _ = reply.send(Err(ShotError::Failed));
                    return;
                };
                let Some(file) = crate::screenshot::memfd() else {
                    let _ = reply.send(Err(ShotError::Failed));
                    return;
                };
                let (x, y, w, h) = area.unwrap_or((0, 0, 0, 0));
                let ids: Vec<u8> = shown.iter().flat_map(|id| id.to_ne_bytes()).collect();
                let fd = file.as_fd().try_clone_to_owned();
                let Ok(fd) = fd else {
                    let _ = reply.send(Err(ShotError::Failed));
                    return;
                };
                let data = ShotData { file: Mutex::new(Some(file)), filled: Mutex::new(0), reply: Mutex::new(Some(reply)) };
                let _ = m.agent_capture(window, x, y, w, h, ids, layers.join(","), fd.as_fd(), &self.qh, data);
            }
            Request::Start { agent, window, reply } => {
                let Some(m) = manager else {
                    let _ = reply.send(Err("this compositor cannot send input for agents".into()));
                    return;
                };
                let data = Arc::new(RunData::default());
                let object = m.agent_input(window, &self.qh, data.clone());
                self.agent_screen.next += 1;
                let id = self.agent_screen.next;
                self.agent_screen.runs.insert(id, Run { object, agent, data });
                let _ = reply.send(Ok(id));
            }
            Request::Step { run, step, reply } => {
                let Some(r) = self.agent_screen.runs.get(&run) else {
                    let _ = reply.send(Err(Stopped::Failed));
                    return;
                };
                if let Some(why) = *r.data.stopped.lock().unwrap_or_else(|e| e.into_inner()) {
                    let _ = reply.send(Err(why));
                    return;
                }
                r.data.pending.lock().unwrap_or_else(|e| e.into_inner()).push_back(reply);
                let state = |down: bool| if down { helix_agent_input_v1::State::Down } else { helix_agent_input_v1::State::Up };
                match step {
                    Step::Motion(x, y) => r.object.motion(x, y),
                    Step::Button(button, down) => r.object.button(button, state(down)),
                    Step::Scroll(x, y) => r.object.scroll(x, y),
                    Step::Key(sym, down) => r.object.key(sym, state(down)),
                    Step::Text(text) => r.object.text(text),
                    // Pauses are the agent code's.
                    Step::Wait(_) => {
                        if let Some(reply) = r.data.pending.lock().unwrap_or_else(|e| e.into_inner()).pop_back() {
                            let _ = reply.send(Ok(()));
                        }
                    }
                }
            }
            Request::Cancel { run } => {
                if let Some(r) = self.agent_screen.runs.get(&run) {
                    r.object.cancel();
                }
            }
            Request::End { run } => {
                if let Some(r) = self.agent_screen.runs.remove(&run) {
                    r.object.destroy();
                }
            }
        }
    }

    /// *Stop* in the indicator: `agent`'s input stops now, before the
    /// agent code hears of it.
    pub(super) fn stop_agent_input(&mut self, agent: &str) {
        for r in self.agent_screen.runs.values().filter(|r| r.agent == agent) {
            r.object.cancel();
        }
    }
}

impl Dispatch<HelixAgentInputV1, Arc<RunData>> for Ui {
    fn event(_: &mut Self, _: &HelixAgentInputV1, event: helix_agent_input_v1::Event, data: &Arc<RunData>, _: &Connection, _: &QueueHandle<Self>) {
        match event {
            helix_agent_input_v1::Event::Done => {
                if let Some(reply) = data.pending.lock().unwrap_or_else(|e| e.into_inner()).pop_front() {
                    let _ = reply.send(Ok(()));
                }
            }
            helix_agent_input_v1::Event::Stopped { reason } => {
                let why = Stopped::from_wire(match reason {
                    WEnum::Value(v) => v.into(),
                    WEnum::Unknown(n) => n,
                });
                *data.stopped.lock().unwrap_or_else(|e| e.into_inner()) = Some(why);
                for reply in data.pending.lock().unwrap_or_else(|e| e.into_inner()).drain(..) {
                    let _ = reply.send(Err(why));
                }
            }
            _ => {}
        }
    }
}

impl Dispatch<HelixScreenshotFrameV1, ShotData> for Ui {
    fn event(_: &mut Self, frame: &HelixScreenshotFrameV1, event: helix_screenshot_frame_v1::Event, data: &ShotData, _: &Connection, _: &QueueHandle<Self>) {
        let result = match event {
            helix_screenshot_frame_v1::Event::Filled { count } => {
                *data.filled.lock().unwrap_or_else(|e| e.into_inner()) = count;
                return;
            }
            helix_screenshot_frame_v1::Event::Done { x, y, width, height } => match data.file.lock().unwrap_or_else(|e| e.into_inner()).take() {
                Some(file) => Ok(Shot { file, rect: (x, y, width, height), filled: *data.filled.lock().unwrap_or_else(|e| e.into_inner()) }),
                None => Err(ShotError::Failed),
            },
            helix_screenshot_frame_v1::Event::Failed { reason } => Err(match reason {
                WEnum::Value(Reason::Locked) => ShotError::Locked,
                WEnum::Value(Reason::NoWindow) => ShotError::NoWindow,
                WEnum::Value(Reason::Invalid) => ShotError::Invalid,
                _ => ShotError::Failed,
            }),
            _ => return,
        };
        frame.destroy();
        if let Some(reply) = data.reply.lock().unwrap_or_else(|e| e.into_inner()).take() {
            let _ = reply.send(result);
        }
    }
}
