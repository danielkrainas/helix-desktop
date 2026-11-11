//! Recording monitors for screen casts (helix-desktop-spec
//! `org.gnome.Mutter.ScreenCast.md`): frames of a monitor, rendered
//! off-screen at most `MAX_FPS` times a second while a consumer streams,
//! go to the stream's PipeWire node.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use helix_comp::screencast::CastRequest;
use helix_comp::Event;
use smithay::backend::renderer::element::memory::MemoryRenderBuffer;
use smithay::backend::renderer::gles::GlesRenderer;
use smithay::reexports::calloop::timer::{TimeoutAction, Timer};
use smithay::utils::{Physical, Size};

use super::pipewire::{Command, Notice, PipeWire, MAX_FPS};
use super::Helix;

const FRAME: Duration = Duration::from_millis(1000 / MAX_FPS as u64);

struct Cast {
    connector: String,
    cursor: bool,
    size: Size<i32, Physical>,
    streaming: bool,
    last: Option<Instant>,
}

#[derive(Default)]
pub struct CastState {
    pipewire: Option<PipeWire>,
    casts: HashMap<u32, Cast>,
    notices: Option<smithay::reexports::calloop::channel::Sender<Notice>>,
    timer: bool,
}

/// RGBA → BGRx, in place.
pub fn to_bgrx(rgba: &mut [u8]) {
    for px in rgba.as_chunks_mut::<4>().0 {
        px.swap(0, 2);
        px[3] = 255;
    }
}

impl Helix {
    fn output_named(&self, connector: &str) -> Option<smithay::output::Output> {
        self.space.outputs().find(|o| o.name() == connector).cloned()
    }

    fn pipewire(&mut self) -> Result<&PipeWire, String> {
        if self.screencast.pipewire.is_none() {
            let (tx, rx) = smithay::reexports::calloop::channel::channel::<Notice>();
            self.loop_handle
                .insert_source(rx, |event, _, state| {
                    if let smithay::reexports::calloop::channel::Event::Msg(notice) = event {
                        state.cast_notice(notice);
                    }
                })
                .map_err(|e| e.to_string())?;
            self.screencast.notices = Some(tx.clone());
            self.screencast.pipewire = Some(PipeWire::start(tx)?);
        }
        Ok(self.screencast.pipewire.as_ref().expect("just started"))
    }

    pub fn cast_request(&mut self, request: CastRequest) {
        match request {
            CastRequest::Start { stream, connector, cursor, reply } => {
                let Some(output) = self.output_named(&connector) else {
                    let _ = reply.send(Err(format!("no monitor {connector}")));
                    return;
                };
                let Some(geometry) = self.space.output_geometry(&output) else {
                    let _ = reply.send(Err(format!("monitor {connector} is off")));
                    return;
                };
                let scale = output.current_scale().fractional_scale();
                let size: Size<i32, Physical> = geometry.size.to_f64().to_physical(scale).to_i32_round();
                let (node_tx, node_rx) = std::sync::mpsc::channel();
                let created = self.pipewire().map(|pw| {
                    pw.send(Command::Create { id: stream, width: size.w as u32, height: size.h as u32, reply: node_tx })
                });
                match created {
                    Ok(true) => {}
                    Ok(false) => {
                        let _ = reply.send(Err("PipeWire thread gone".into()));
                        return;
                    }
                    Err(err) => {
                        tracing::warn!(%err, "screen casting unavailable");
                        let _ = reply.send(Err(err));
                        return;
                    }
                }
                // The node id arrives once PipeWire set the stream up.
                std::thread::spawn(move || {
                    let node = node_rx
                        .recv_timeout(Duration::from_secs(5))
                        .unwrap_or_else(|_| Err("PipeWire did not set up the stream".into()));
                    let _ = reply.send(node);
                });
                self.screencast.casts.insert(stream, Cast { connector, cursor, size, streaming: false, last: None });
                self.start_cast_timer();
            }
            CastRequest::Stop { stream } => {
                if self.screencast.casts.remove(&stream).is_some() {
                    if let Some(pw) = &self.screencast.pipewire {
                        pw.send(Command::Destroy { id: stream });
                    }
                }
            }
        }
    }

    fn cast_notice(&mut self, notice: Notice) {
        match notice {
            Notice::Streaming { id, on } => {
                if let Some(cast) = self.screencast.casts.get_mut(&id) {
                    tracing::info!(stream = id, on, "screen cast consumer");
                    cast.streaming = on;
                }
            }
            Notice::Ended { id } => self.end_cast(id),
        }
    }

    fn end_cast(&mut self, id: u32) {
        if self.screencast.casts.remove(&id).is_some() {
            if let Some(pw) = &self.screencast.pipewire {
                pw.send(Command::Destroy { id });
            }
            self.send(Event::ScreenCastEnded(id));
        }
    }

    /// On bare hardware, frames are made on a timer (nested: every redraw).
    fn start_cast_timer(&mut self) {
        if self.udev.is_none() || self.screencast.timer {
            return;
        }
        self.screencast.timer = true;
        let _ = self.loop_handle.insert_source(Timer::from_duration(FRAME), |_, _, state| {
            if state.screencast.casts.is_empty() {
                state.screencast.timer = false;
                return TimeoutAction::Drop;
            }
            if let Some(mut udev) = state.udev.take() {
                let cursor = udev.cursor_buffer().clone();
                state.cast_frames(udev.renderer(), Some(&cursor));
                state.udev = Some(udev);
            }
            TimeoutAction::ToDuration(FRAME)
        });
    }

    /// Send a frame to every streaming cast that is due one.
    pub fn cast_frames(&mut self, renderer: &mut GlesRenderer, named_cursor: Option<&MemoryRenderBuffer>) {
        let now = Instant::now();
        let due: Vec<u32> = self
            .screencast
            .casts
            .iter()
            .filter(|(_, c)| c.streaming && c.last.is_none_or(|t| now.duration_since(t) >= FRAME))
            .map(|(id, _)| *id)
            .collect();
        for id in due {
            let Some(cast) = self.screencast.casts.get(&id) else { continue };
            let (connector, cursor, size) = (cast.connector.clone(), cast.cursor, cast.size);
            let Some(output) = self.output_named(&connector) else {
                // The monitor went away.
                self.end_cast(id);
                continue;
            };
            let bytes = (size.w * size.h * 4) as usize;
            let frame = if self.lock.locked {
                let mut black = vec![0u8; bytes];
                to_bgrx(&mut black);
                Some(black)
            } else {
                match self.output_image(renderer, &output, cursor.then_some(named_cursor)) {
                    Ok(image) if image.rgba.len() == bytes => {
                        let mut pixels = image.rgba;
                        to_bgrx(&mut pixels);
                        Some(pixels)
                    }
                    Ok(_) => {
                        // The monitor changed size; this stream can't follow.
                        self.end_cast(id);
                        None
                    }
                    Err(err) => {
                        tracing::debug!(err, "screen cast frame failed");
                        None
                    }
                }
            };
            if let Some(cast) = self.screencast.casts.get_mut(&id) {
                cast.last = Some(now);
            }
            if let (Some(bgrx), Some(pw)) = (frame, &self.screencast.pipewire) {
                pw.send(Command::Frame { id, bgrx });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn bgrx() {
        let mut px = [1, 2, 3, 4, 5, 6, 7, 8];
        super::to_bgrx(&mut px);
        assert_eq!(px, [3, 2, 1, 255, 7, 6, 5, 255]);
    }
}
