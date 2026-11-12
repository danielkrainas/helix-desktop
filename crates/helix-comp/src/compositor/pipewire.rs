//! PipeWire video sources for screen casting (helix-desktop-spec
//! `org.gnome.Mutter.ScreenCast.md`): one thread runs PipeWire's loop and
//! the streams; the compositor sends it frames.
//!
//! Each stream is a driving output node with a fixed BGRx format of the
//! monitor's pixel size and at most `MAX_FPS` frames per second, in memfd
//! buffers this thread allocates (as PipeWire's own allocation does not
//! honor our buffer size). A frame sent while the stream is not streaming
//! is dropped.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use pipewire as pw;
use pw::spa;
use pw::spa::pod::{ChoiceValue, Object, Pod, Property, PropertyFlags, Value};
use pw::spa::utils::{Choice, ChoiceEnum, ChoiceFlags, Fraction, Rectangle};
use smithay::reexports::calloop::channel::Sender as LoopSender;

pub const MAX_FPS: u32 = 30;

/// The biggest pointer image sent as cursor metadata.
pub const CURSOR_MAX: u32 = 64;

/// The pointer for cursor metadata (`SPA_META_Cursor`).
#[derive(Debug, Clone, PartialEq)]
pub struct CursorMeta {
    /// Shown on the source (else the metadata says "no pointer").
    pub visible: bool,
    /// Position and hotspot in stream pixels.
    pub x: i32,
    pub y: i32,
    pub hotspot: (i32, i32),
    /// BGRA, `width * 4` per row.
    pub bitmap: std::sync::Arc<(Vec<u8>, u32, u32)>,
}

/// What the compositor asks of the PipeWire thread.
pub enum Command {
    /// Create a stream; the reply is its node id once PipeWire assigned it.
    /// With `cursor_meta`, buffers carry cursor metadata.
    Create { id: u32, width: u32, height: u32, cursor_meta: bool, reply: std::sync::mpsc::Sender<Result<u32, String>> },
    /// The stream's frames change size (renegotiates the format).
    Resize { id: u32, width: u32, height: u32 },
    /// A frame for a stream: BGRx, rows of `width * 4` bytes.
    Frame { id: u32, bgrx: Vec<u8>, cursor: Option<CursorMeta> },
    /// Only the pointer moved: send the last frame again with it.
    Cursor { id: u32, cursor: CursorMeta },
    Destroy { id: u32 },
}

/// What the PipeWire thread tells the compositor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Notice {
    /// A consumer is (or stopped) taking frames.
    Streaming { id: u32, on: bool },
    /// The stream failed or was removed by PipeWire.
    Ended { id: u32 },
}

/// Handle to the PipeWire thread.
pub struct PipeWire {
    commands: pw::channel::Sender<Command>,
}

impl PipeWire {
    pub fn start(notices: LoopSender<Notice>) -> Result<Self, String> {
        let (commands, receiver) = pw::channel::channel::<Command>();
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        std::thread::Builder::new()
            .name("helix-comp-pipewire".into())
            .spawn(move || run(receiver, notices, ready_tx))
            .map_err(|e| e.to_string())?;
        ready_rx.recv().map_err(|_| "PipeWire thread died".to_owned())??;
        Ok(Self { commands })
    }

    pub fn send(&self, command: Command) -> bool {
        self.commands.send(command).is_ok()
    }
}

fn serialize(value: Value) -> Result<Vec<u8>, String> {
    spa::pod::serialize::PodSerializer::serialize(std::io::Cursor::new(Vec::new()), &value)
        .map(|(cursor, _)| cursor.into_inner())
        .map_err(|e| format!("{e:?}"))
}

fn property(key: u32, value: Value) -> Property {
    Property { key, flags: PropertyFlags::empty(), value }
}

fn format_pod(width: u32, height: u32) -> Result<Vec<u8>, String> {
    use spa::param::format::{FormatProperties as F, MediaSubtype, MediaType};
    use spa::param::video::VideoFormat;
    let max = Fraction { num: MAX_FPS, denom: 1 };
    serialize(Value::Object(Object {
        type_: spa::sys::SPA_TYPE_OBJECT_Format,
        id: spa::sys::SPA_PARAM_EnumFormat,
        properties: vec![
            property(F::MediaType.as_raw(), Value::Id(spa::utils::Id(MediaType::Video.as_raw()))),
            property(F::MediaSubtype.as_raw(), Value::Id(spa::utils::Id(MediaSubtype::Raw.as_raw()))),
            property(F::VideoFormat.as_raw(), Value::Id(spa::utils::Id(VideoFormat::BGRx.as_raw()))),
            property(F::VideoSize.as_raw(), Value::Rectangle(Rectangle { width, height })),
            property(F::VideoFramerate.as_raw(), Value::Fraction(Fraction { num: 0, denom: 1 })),
            property(
                F::VideoMaxFramerate.as_raw(),
                Value::Choice(ChoiceValue::Fraction(Choice(
                    ChoiceFlags::empty(),
                    ChoiceEnum::Range { default: max, min: Fraction { num: 1, denom: 1 }, max },
                ))),
            ),
        ],
    }))
}

/// Size of the cursor metadata: the cursor, the bitmap header and image.
fn cursor_meta_size() -> usize {
    std::mem::size_of::<spa::sys::spa_meta_cursor>()
        + std::mem::size_of::<spa::sys::spa_meta_bitmap>()
        + (CURSOR_MAX * CURSOR_MAX * 4) as usize
}

fn cursor_meta_pod() -> Result<Vec<u8>, String> {
    serialize(Value::Object(Object {
        type_: spa::sys::SPA_TYPE_OBJECT_ParamMeta,
        id: spa::sys::SPA_PARAM_Meta,
        properties: vec![
            property(spa::sys::SPA_PARAM_META_type, Value::Id(spa::utils::Id(spa::sys::SPA_META_Cursor))),
            property(spa::sys::SPA_PARAM_META_size, Value::Int(cursor_meta_size() as i32)),
        ],
    }))
}

/// Buffer headers (`SPA_META_Header`): when each frame was made, which
/// consumers such as GStreamer timestamp frames with.
fn header_meta_pod() -> Result<Vec<u8>, String> {
    serialize(Value::Object(Object {
        type_: spa::sys::SPA_TYPE_OBJECT_ParamMeta,
        id: spa::sys::SPA_PARAM_Meta,
        properties: vec![
            property(spa::sys::SPA_PARAM_META_type, Value::Id(spa::utils::Id(spa::sys::SPA_META_Header))),
            property(spa::sys::SPA_PARAM_META_size, Value::Int(std::mem::size_of::<spa::sys::spa_meta_header>() as i32)),
        ],
    }))
}

/// Stamp a buffer's header (if it has one) with the monotonic time now.
///
/// # Safety
/// `buffer` is a buffer PipeWire dequeued for us.
unsafe fn write_header(buffer: *mut spa::sys::spa_buffer, seq: u64) {
    let metas = std::slice::from_raw_parts((*buffer).metas, (*buffer).n_metas as usize);
    let Some(meta) = metas.iter().find(|m| m.type_ == spa::sys::SPA_META_Header) else { return };
    if meta.data.is_null() || (meta.size as usize) < std::mem::size_of::<spa::sys::spa_meta_header>() {
        return;
    }
    let mut now = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut now);
    let header = meta.data as *mut spa::sys::spa_meta_header;
    (*header).flags = 0;
    (*header).offset = 0;
    (*header).pts = now.tv_sec * 1_000_000_000 + now.tv_nsec;
    (*header).dts_offset = 0;
    (*header).seq = seq;
}

/// Write `cursor` into a buffer's cursor metadata, if it has room for it.
///
/// # Safety
/// `buffer` is a buffer PipeWire dequeued for us.
unsafe fn write_cursor(buffer: *mut spa::sys::spa_buffer, cursor: &CursorMeta) {
    if buffer.is_null() {
        return;
    }
    let metas = std::slice::from_raw_parts((*buffer).metas, (*buffer).n_metas as usize);
    let Some(meta) = metas.iter().find(|m| m.type_ == spa::sys::SPA_META_Cursor) else { return };
    if meta.data.is_null() || (meta.size as usize) < cursor_meta_size() {
        return;
    }
    let head = meta.data as *mut spa::sys::spa_meta_cursor;
    if !cursor.visible {
        (*head).id = 0;
        return;
    }
    (*head).id = 1;
    (*head).flags = 0;
    (*head).position = spa::sys::spa_point { x: cursor.x, y: cursor.y };
    (*head).hotspot = spa::sys::spa_point { x: cursor.hotspot.0, y: cursor.hotspot.1 };
    (*head).bitmap_offset = std::mem::size_of::<spa::sys::spa_meta_cursor>() as u32;
    let bitmap = (meta.data as *mut u8).add((*head).bitmap_offset as usize) as *mut spa::sys::spa_meta_bitmap;
    let (pixels, w, h) = &*cursor.bitmap;
    let (w, h) = ((*w).min(CURSOR_MAX), (*h).min(CURSOR_MAX));
    (*bitmap).format = spa::param::video::VideoFormat::BGRA.as_raw();
    (*bitmap).size = spa::sys::spa_rectangle { width: w, height: h };
    (*bitmap).stride = (w * 4) as i32;
    (*bitmap).offset = std::mem::size_of::<spa::sys::spa_meta_bitmap>() as u32;
    let dst = (bitmap as *mut u8).add((*bitmap).offset as usize);
    let src_stride = (cursor.bitmap.1 * 4) as usize;
    for row in 0..h as usize {
        std::ptr::copy_nonoverlapping(pixels.as_ptr().add(row * src_stride), dst.add(row * (w * 4) as usize), (w * 4) as usize);
    }
}

fn buffers_pod(width: u32, height: u32) -> Result<Vec<u8>, String> {
    let stride = (width * 4) as i32;
    serialize(Value::Object(Object {
        type_: spa::sys::SPA_TYPE_OBJECT_ParamBuffers,
        id: spa::sys::SPA_PARAM_Buffers,
        properties: vec![
            property(
                spa::sys::SPA_PARAM_BUFFERS_buffers,
                Value::Choice(ChoiceValue::Int(Choice(ChoiceFlags::empty(), ChoiceEnum::Range { default: 4, min: 2, max: 8 }))),
            ),
            property(spa::sys::SPA_PARAM_BUFFERS_blocks, Value::Int(1)),
            property(spa::sys::SPA_PARAM_BUFFERS_size, Value::Int(stride * height as i32)),
            property(spa::sys::SPA_PARAM_BUFFERS_stride, Value::Int(stride)),
            property(
                spa::sys::SPA_PARAM_BUFFERS_dataType,
                Value::Choice(ChoiceValue::Int(Choice(
                    ChoiceFlags::empty(),
                    ChoiceEnum::Flags { default: 1 << spa::sys::SPA_DATA_MemFd, flags: vec![1 << spa::sys::SPA_DATA_MemFd] },
                ))),
            ),
        ],
    }))
}

/// Replies waiting for a stream's node id, by stream.
type Pending = Rc<RefCell<HashMap<u32, std::sync::mpsc::Sender<Result<u32, String>>>>>;

/// The frame (and pointer) waiting for the next `process`.
type FrameSlot = Rc<RefCell<Option<(Vec<u8>, Option<CursorMeta>)>>>;

/// A stream and the frame waiting for it.
struct Live {
    stream: pw::stream::StreamRc,
    _listener: pw::stream::StreamListener<()>,
    frame: FrameSlot,
    /// The last frame sent (for pointer-only updates).
    last: Rc<RefCell<Option<Vec<u8>>>>,
    size: Rc<std::cell::Cell<(u32, u32)>>,
}

/// Give a buffer's first data block a memfd of one frame, mapped.
unsafe fn allocate(buffer: *mut pw::sys::pw_buffer, width: u32, height: u32) {
    let spa_buffer = (*buffer).buffer;
    if spa_buffer.is_null() || (*spa_buffer).n_datas < 1 {
        return;
    }
    let data = &mut *(*spa_buffer).datas;
    let size = (width * height * 4) as usize;
    let fd = libc::memfd_create(c"helix-screen-cast".as_ptr(), libc::MFD_CLOEXEC | libc::MFD_ALLOW_SEALING);
    if fd < 0 {
        tracing::warn!("cannot create a screen cast buffer");
        return;
    }
    if libc::ftruncate(fd, size as libc::off_t) < 0 {
        libc::close(fd);
        return;
    }
    let ptr = libc::mmap(std::ptr::null_mut(), size, libc::PROT_READ | libc::PROT_WRITE, libc::MAP_SHARED, fd, 0);
    if ptr == libc::MAP_FAILED {
        libc::close(fd);
        return;
    }
    data.type_ = spa::sys::SPA_DATA_MemFd;
    data.flags = spa::sys::SPA_DATA_FLAG_READWRITE | spa::sys::SPA_DATA_FLAG_MAPPABLE;
    data.fd = i64::from(fd);
    data.mapoffset = 0;
    data.maxsize = size as u32;
    data.data = ptr;
}

unsafe fn release(buffer: *mut pw::sys::pw_buffer) {
    let spa_buffer = (*buffer).buffer;
    if spa_buffer.is_null() || (*spa_buffer).n_datas < 1 {
        return;
    }
    let data = &mut *(*spa_buffer).datas;
    if !data.data.is_null() {
        libc::munmap(data.data, data.maxsize as usize);
        data.data = std::ptr::null_mut();
    }
    if data.fd >= 0 && data.type_ == spa::sys::SPA_DATA_MemFd {
        libc::close(data.fd as i32);
        data.fd = -1;
    }
}

fn run(
    receiver: pw::channel::Receiver<Command>,
    notices: LoopSender<Notice>,
    ready: std::sync::mpsc::Sender<Result<(), String>>,
) {
    pw::init();
    let setup = (|| -> Result<_, pw::Error> {
        let mainloop = pw::main_loop::MainLoopRc::new(None)?;
        let context = pw::context::ContextRc::new(&mainloop, None)?;
        let core = context.connect_rc(None)?;
        Ok((mainloop, context, core))
    })();
    let (mainloop, _context, core) = match setup {
        Ok(parts) => {
            let _ = ready.send(Ok(()));
            parts
        }
        Err(err) => {
            let _ = ready.send(Err(format!("cannot connect to PipeWire: {err}")));
            return;
        }
    };
    let streams: Rc<RefCell<HashMap<u32, Live>>> = Rc::default();
    // Node ids are known once the stream is set up; replies wait for them.
    let pending: Pending = Rc::default();

    let attached = receiver.attach(mainloop.loop_(), {
        let streams = streams.clone();
        let pending = pending.clone();
        let core = core.clone();
        let notices = notices.clone();
        move |command| match command {
            Command::Create { id, width, height, cursor_meta, reply } => {
                match create(&core, id, width, height, cursor_meta, notices.clone(), pending.clone()) {
                    Ok(live) => {
                        pending.borrow_mut().insert(id, reply);
                        streams.borrow_mut().insert(id, live);
                    }
                    Err(err) => {
                        let _ = reply.send(Err(err));
                    }
                }
            }
            Command::Frame { id, bgrx, cursor } => {
                if let Some(live) = streams.borrow().get(&id) {
                    let (width, height) = live.size.get();
                    if bgrx.len() == (width * height * 4) as usize {
                        *live.frame.borrow_mut() = Some((bgrx, cursor));
                        if let Err(err) = live.stream.trigger_process() {
                            tracing::debug!(id, %err, "cannot trigger a frame");
                        }
                    }
                }
            }
            Command::Cursor { id, cursor } => {
                if let Some(live) = streams.borrow().get(&id) {
                    let last = live.last.borrow().clone();
                    if let Some(bgrx) = last {
                        if live.frame.borrow().is_none() {
                            *live.frame.borrow_mut() = Some((bgrx, Some(cursor)));
                            let _ = live.stream.trigger_process();
                        }
                    }
                }
            }
            Command::Resize { id, width, height } => {
                if let Some(live) = streams.borrow().get(&id) {
                    live.size.set((width, height));
                    *live.frame.borrow_mut() = None;
                    *live.last.borrow_mut() = None;
                    match format_pod(width, height) {
                        Ok(bytes) => {
                            if let Some(pod) = Pod::from_bytes(&bytes) {
                                if let Err(err) = live.stream.update_params(&mut [pod]) {
                                    tracing::warn!(id, %err, "cannot change the stream size");
                                }
                            }
                        }
                        Err(err) => tracing::warn!(id, err, "cannot describe the new size"),
                    }
                }
            }
            Command::Destroy { id } => {
                pending.borrow_mut().remove(&id);
                if let Some(live) = streams.borrow_mut().remove(&id) {
                    let _ = live.stream.disconnect();
                }
            }
        }
    });
    mainloop.run();
    drop(attached);
}

fn create(
    core: &pw::core::CoreRc,
    id: u32,
    width: u32,
    height: u32,
    cursor_meta: bool,
    notices: LoopSender<Notice>,
    pending: Pending,
) -> Result<Live, String> {
    let props = pw::properties::properties! {
        *pw::keys::MEDIA_CLASS => "Video/Source",
        *pw::keys::MEDIA_ROLE => "Screen",
        *pw::keys::NODE_NAME => "helix-screen-cast",
    };
    let stream = pw::stream::StreamRc::new(core.clone(), "helix-screen-cast", props).map_err(|e| e.to_string())?;
    let frame: FrameSlot = Rc::default();
    let last: Rc<RefCell<Option<Vec<u8>>>> = Rc::default();
    let size = Rc::new(std::cell::Cell::new((width, height)));
    let listener = stream
        .add_local_listener_with_user_data(())
        .state_changed({
            let notices = notices.clone();
            move |stream, _, old, new| {
                tracing::debug!(id, ?old, ?new, "screen cast stream state");
                match new {
                    pw::stream::StreamState::Paused => {
                        if let Some(reply) = pending.borrow_mut().remove(&id) {
                            let _ = reply.send(Ok(stream.node_id()));
                        }
                        if matches!(old, pw::stream::StreamState::Streaming) {
                            let _ = notices.send(Notice::Streaming { id, on: false });
                        }
                    }
                    pw::stream::StreamState::Streaming => {
                        let _ = notices.send(Notice::Streaming { id, on: true });
                    }
                    pw::stream::StreamState::Error(err) => {
                        tracing::warn!(id, err, "screen cast stream failed");
                        if let Some(reply) = pending.borrow_mut().remove(&id) {
                            let _ = reply.send(Err(err));
                        }
                        let _ = notices.send(Notice::Ended { id });
                    }
                    pw::stream::StreamState::Unconnected => {
                        let _ = notices.send(Notice::Ended { id });
                    }
                    _ => {}
                }
            }
        })
        .param_changed({
            let size = size.clone();
            move |stream, _, param_id, param| {
            let (width, height) = size.get();
            if param_id != spa::param::ParamType::Format.as_raw() || param.is_none() {
                return;
            }
            // The format is fixed; say how big the buffers are (and that
            // they carry cursor metadata).
            let buffers = buffers_pod(width, height);
            let meta = if cursor_meta { cursor_meta_pod().map(Some) } else { Ok(None) };
            match (buffers, meta, header_meta_pod()) {
                (Ok(buffers), Ok(meta), Ok(header)) => {
                    let mut pods: Vec<&Pod> = Pod::from_bytes(&buffers).into_iter().collect();
                    pods.extend(Pod::from_bytes(&header));
                    if let Some(meta) = meta.as_ref() {
                        pods.extend(Pod::from_bytes(meta));
                    }
                    match stream.update_params(&mut pods) {
                        Ok(()) => tracing::debug!(id, "buffer parameters set"),
                        Err(err) => tracing::warn!(id, %err, "cannot set buffer parameters"),
                    }
                }
                other => tracing::warn!(id, ?other, "cannot describe buffer parameters"),
            }
        }})
        .add_buffer({
            let size = size.clone();
            move |_, _, buffer| {
                let (width, height) = size.get();
                // SAFETY: PipeWire hands us a buffer with one data block to
                // fill in; we own the memfd and mapping until remove_buffer.
                unsafe { allocate(buffer, width, height) };
            }
        })
        .remove_buffer(|_, _, buffer| {
            // SAFETY: the same buffer, allocated by `allocate`.
            unsafe { release(buffer) };
        })
        .process({
            let frame = frame.clone();
            let last = last.clone();
            let size = size.clone();
            let mut seq = 0u64;
            move |stream, _| {
                let (width, _) = size.get();
                let Some((pixels, cursor)) = frame.borrow_mut().take() else {
                    tracing::trace!(id, "process without a frame");
                    return;
                };
                // Raw buffers: the cursor metadata is not reachable through
                // the safe wrapper.
                // SAFETY: dequeue/queue pair on our own stream; the data
                // block was mapped by `allocate` and is `maxsize` long.
                let copied = unsafe {
                    let raw = pw::sys::pw_stream_dequeue_buffer(stream.as_raw_ptr());
                    if raw.is_null() {
                        tracing::debug!(id, "no free buffer; frame dropped");
                        return;
                    }
                    let spa_buffer = (*raw).buffer;
                    let mut copied = 0usize;
                    if !spa_buffer.is_null() && (*spa_buffer).n_datas > 0 {
                        let data = &mut *(*spa_buffer).datas;
                        if !data.data.is_null() && data.maxsize as usize >= pixels.len() {
                            std::ptr::copy_nonoverlapping(pixels.as_ptr(), data.data as *mut u8, pixels.len());
                            copied = pixels.len();
                        }
                        if !data.chunk.is_null() {
                            (*data.chunk).offset = 0;
                            (*data.chunk).stride = (width * 4) as i32;
                            (*data.chunk).size = copied as u32;
                        }
                    }
                    if let Some(cursor) = &cursor {
                        write_cursor(spa_buffer, cursor);
                    }
                    if !spa_buffer.is_null() {
                        seq += 1;
                        write_header(spa_buffer, seq);
                    }
                    pw::sys::pw_stream_queue_buffer(stream.as_raw_ptr(), raw);
                    copied
                };
                tracing::trace!(id, copied, "frame queued");
                if cursor.is_some() {
                    *last.borrow_mut() = Some(pixels);
                }
            }
        })
        .register()
        .map_err(|e| e.to_string())?;
    let format = format_pod(width, height)?;
    let mut params = [Pod::from_bytes(&format).ok_or("bad format")?];
    stream
        .connect(
            spa::utils::Direction::Output,
            None,
            pw::stream::StreamFlags::DRIVER | pw::stream::StreamFlags::ALLOC_BUFFERS,
            &mut params,
        )
        .map_err(|e| e.to_string())?;
    Ok(Live { stream, _listener: listener, frame, last, size })
}
