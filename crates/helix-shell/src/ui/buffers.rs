//! The shared-memory buffers shell surfaces are painted into
//! (helix-desktop#100).
//!
//! Each surface has a few buffers of its own, each in its own memory file,
//! so closing the surface gives its memory back. A buffer the compositor
//! has released is painted again for the next frame; a new one is made
//! only while fewer than [`MAX_BUSY`] are with the compositor, and all of
//! them are made again when the surface's size changes.
//!
//! A surface is painted at most once between two passes of the event
//! loop: a second paint asked for meanwhile is refused and its surface
//! drawn again (once, by the function given) after the pass, from
//! [`Ui::flush_redraws`]. A paint is also refused while [`MAX_BUSY`]
//! buffers are with the compositor, and done when one comes back. So a
//! burst of redraws (pointer motion, animation steps, pictures arriving)
//! paints the latest state once instead of piling up buffers the
//! compositor has not released yet.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use smithay_client_toolkit::reexports::client::protocol::wl_buffer::{self, WlBuffer};
use smithay_client_toolkit::reexports::client::protocol::wl_shm;
use smithay_client_toolkit::reexports::client::protocol::wl_surface::WlSurface;
use smithay_client_toolkit::reexports::client::{Connection, Dispatch, Proxy, QueueHandle};
use smithay_client_toolkit::shm::raw::RawPool;
use smithay_client_toolkit::shm::Shm;

use super::Ui;

/// Buffers of one surface that may be with the compositor at once.
pub(super) const MAX_BUSY: usize = 3;

/// A buffer back from the compositor and not painted again for this long
/// is freed (a surface that stays as it is keeps only what it shows).
const IDLE: Duration = Duration::from_secs(5);

/// Draws a surface again (and whatever else that function draws).
pub(super) type Redraw = fn(&mut Ui);

/// What a buffer is: width, height, stride (pixels, bytes) and format.
type Shape = (i32, i32, i32, wl_shm::Format);

/// A buffer is with the compositor (attached, not released yet).
pub(super) struct Busy(Arc<AtomicBool>);

/// One buffer and the memory file holding it.
struct Slot {
    buffer: WlBuffer,
    busy: Arc<AtomicBool>,
    /// When it was last handed out for painting.
    used: Instant,
    /// Destroyed (with its memory file) after the buffer.
    pool: RawPool,
}

impl Slot {
    fn busy(&self) -> bool {
        self.busy.load(Ordering::Acquire)
    }
}

impl Drop for Slot {
    fn drop(&mut self) {
        // Allowed while the compositor still shows it: the memory is not
        // written to again, and the compositor keeps its own mapping.
        self.buffer.destroy();
    }
}

struct SurfaceBuffers {
    surface: WlSurface,
    shape: Option<Shape>,
    /// Buffers of the current shape.
    slots: Vec<Slot>,
    /// Buffers of an earlier shape still with the compositor (dropped once
    /// released).
    retired: Vec<Slot>,
    /// A buffer was attached since the last pass of the event loop.
    painted: Arc<AtomicBool>,
    /// A paint was refused: how to draw the surface again.
    redraw: Option<Redraw>,
}

impl SurfaceBuffers {
    fn busy(&self) -> usize {
        self.slots.iter().chain(&self.retired).filter(|s| s.busy()).count()
    }

    fn bytes(&self) -> usize {
        self.slots.iter().chain(&self.retired).map(|s| s.pool.len()).sum()
    }

    /// A buffer could be painted now.
    fn can_paint(&self) -> bool {
        !self.painted.load(Ordering::Acquire) && (self.slots.iter().any(|s| !s.busy()) || self.busy() < MAX_BUSY)
    }
}

/// A buffer to paint and attach; see [`Buffers::create_buffer`].
pub(super) struct ShellBuffer {
    buffer: WlBuffer,
    busy: Arc<AtomicBool>,
    painted: Arc<AtomicBool>,
}

impl ShellBuffer {
    /// Attach to `surface` (the one it was made for); the buffer is the
    /// compositor's until it releases it. Err if it already is.
    pub(super) fn attach_to(&self, surface: &WlSurface) -> Result<(), AlreadyBusy> {
        if self.busy.swap(true, Ordering::AcqRel) {
            return Err(AlreadyBusy);
        }
        surface.attach(Some(&self.buffer), 0, 0);
        self.painted.store(true, Ordering::Release);
        Ok(())
    }
}

/// [`ShellBuffer::attach_to`]: the buffer is with the compositor.
#[derive(Debug)]
pub(super) struct AlreadyBusy;

/// No buffer to paint now; the surface is drawn again later.
#[derive(Debug)]
pub(super) enum NoBuffer {
    /// Painted already in this pass of the event loop.
    Painted,
    /// [`MAX_BUSY`] buffers are with the compositor.
    AllBusy,
    /// Out of memory, or the compositor is gone.
    Failed,
}

/// Totals, for the log and tests.
#[derive(Debug, Default, Clone, Copy, PartialEq)]
pub(super) struct Stats {
    pub surfaces: usize,
    pub buffers: usize,
    pub bytes: usize,
    /// The most buffers one surface has had with the compositor at once.
    pub peak_busy: usize,
    /// Paints refused: painted already in that pass, all buffers busy.
    pub deferred: u64,
    pub starved: u64,
}

/// Every surface's buffers.
pub(super) struct Buffers {
    shm: Shm,
    qh: QueueHandle<Ui>,
    surfaces: Vec<SurfaceBuffers>,
    peak_busy: usize,
    deferred: u64,
    starved: u64,
    /// The totals last logged.
    logged: (usize, usize),
}

impl Buffers {
    pub(super) fn new(shm: &Shm, qh: &QueueHandle<Ui>) -> Self {
        Self { shm: Shm::from(shm.wl_shm().clone()), qh: qh.clone(), surfaces: Vec::new(), peak_busy: 0, deferred: 0, starved: 0, logged: (0, 0) }
    }

    /// A buffer of `width`×`height` pixels (`stride` bytes a row) to paint
    /// `surface`'s next frame into, and its pixels (contents undefined:
    /// paint all of them). Refused (see [`NoBuffer`]) when the surface
    /// cannot take a frame now; `redraw` then draws it again when it can.
    pub(super) fn create_buffer(&mut self, surface: &WlSurface, redraw: Redraw, width: i32, height: i32, stride: i32, format: wl_shm::Format) -> Result<(ShellBuffer, &mut [u8]), NoBuffer> {
        let index = match self.surfaces.iter().position(|s| &s.surface == surface) {
            Some(i) => i,
            None => {
                self.surfaces.push(SurfaceBuffers {
                    surface: surface.clone(),
                    shape: None,
                    slots: Vec::new(),
                    retired: Vec::new(),
                    painted: Arc::new(AtomicBool::new(false)),
                    redraw: None,
                });
                self.surfaces.len() - 1
            }
        };
        let entry = &mut self.surfaces[index];
        if entry.painted.load(Ordering::Acquire) {
            entry.redraw = Some(redraw);
            self.deferred += 1;
            return Err(NoBuffer::Painted);
        }
        let shape = (width, height, stride, format);
        if entry.shape != Some(shape) {
            entry.shape = Some(shape);
            let old = std::mem::take(&mut entry.slots);
            entry.retired.extend(old.into_iter().filter(Slot::busy));
        }
        entry.retired.retain(Slot::busy);
        let busy = entry.busy();
        let slot = match entry.slots.iter().position(|s| !s.busy()) {
            Some(i) => i,
            None if busy < MAX_BUSY => {
                let Some(slot) = new_slot(&self.shm, &self.qh, shape) else {
                    entry.redraw = Some(redraw);
                    return Err(NoBuffer::Failed);
                };
                entry.slots.push(slot);
                entry.slots.len() - 1
            }
            None => {
                entry.redraw = Some(redraw);
                self.starved += 1;
                return Err(NoBuffer::AllBusy);
            }
        };
        entry.redraw = None;
        self.peak_busy = self.peak_busy.max(busy + 1);
        let painted = entry.painted.clone();
        let slot = &mut self.surfaces[index].slots[slot];
        slot.used = Instant::now();
        let buffer = ShellBuffer { buffer: slot.buffer.clone(), busy: slot.busy.clone(), painted };
        let len = (stride.max(0) as usize) * (height.max(0) as usize);
        let data = &mut slot.pool.mmap()[..len];
        Ok((buffer, data))
    }

    /// After a pass of the event loop: surfaces gone are forgotten (their
    /// memory freed), and the redraws that can be done now are handed out
    /// (each function once).
    fn next_pass(&mut self) -> Vec<Redraw> {
        self.surfaces.retain(|s| s.surface.is_alive());
        let mut redraws: Vec<Redraw> = Vec::new();
        for entry in &mut self.surfaces {
            entry.painted.store(false, Ordering::Release);
            entry.retired.retain(Slot::busy);
            entry.slots.retain(|s| s.busy() || s.used.elapsed() < IDLE);
            if entry.redraw.is_some() && entry.can_paint() {
                let redraw = entry.redraw.take().expect("checked");
                if !redraws.iter().any(|r| *r as usize == redraw as usize) {
                    redraws.push(redraw);
                }
            }
        }
        redraws
    }

    fn passed(&mut self) {
        for entry in &self.surfaces {
            entry.painted.store(false, Ordering::Release);
        }
        let stats = self.stats();
        if (stats.buffers, stats.bytes) != self.logged {
            self.logged = (stats.buffers, stats.bytes);
            tracing::debug!(surfaces = stats.surfaces, buffers = stats.buffers, kib = stats.bytes / 1024, peak_busy = stats.peak_busy, deferred = stats.deferred, starved = stats.starved, "shell buffers");
        }
    }

    pub(super) fn stats(&self) -> Stats {
        Stats {
            surfaces: self.surfaces.len(),
            buffers: self.surfaces.iter().map(|s| s.slots.len() + s.retired.len()).sum(),
            bytes: self.surfaces.iter().map(SurfaceBuffers::bytes).sum(),
            peak_busy: self.peak_busy,
            deferred: self.deferred,
            starved: self.starved,
        }
    }
}

fn new_slot(shm: &Shm, qh: &QueueHandle<Ui>, (width, height, stride, format): Shape) -> Option<Slot> {
    let len = (stride.max(1) as usize) * (height.max(1) as usize);
    let mut pool = match RawPool::new(len, shm) {
        Ok(pool) => pool,
        Err(err) => {
            tracing::warn!(%err, len, "no memory for a shell buffer");
            return None;
        }
    };
    let busy = Arc::new(AtomicBool::new(false));
    let buffer = pool.create_buffer(0, width, height, stride, format, Busy(busy.clone()), qh);
    Some(Slot { buffer, busy, used: Instant::now(), pool })
}

impl Dispatch<WlBuffer, Busy> for Ui {
    fn event(_: &mut Self, _: &WlBuffer, event: wl_buffer::Event, data: &Busy, _: &Connection, _: &QueueHandle<Self>) {
        if let wl_buffer::Event::Release = event {
            data.0.store(false, Ordering::Release);
        }
    }
}

impl Ui {
    /// After each pass of the event loop: draw again the surfaces whose
    /// paint was refused, now that they can take one.
    pub(super) fn flush_redraws(&mut self) {
        let redraws = self.buffers.next_pass();
        if !redraws.is_empty() {
            let _busy = crate::watchdog::scope("deferred redraws");
            for redraw in redraws {
                redraw(self);
            }
        }
        self.buffers.passed();
    }
}

