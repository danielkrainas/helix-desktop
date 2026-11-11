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

/// What the compositor asks of the PipeWire thread.
pub enum Command {
    /// Create a stream; the reply is its node id once PipeWire assigned it.
    Create { id: u32, width: u32, height: u32, reply: std::sync::mpsc::Sender<Result<u32, String>> },
    /// A frame for a stream: BGRx, rows of `width * 4` bytes.
    Frame { id: u32, bgrx: Vec<u8> },
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

/// A stream and the frame waiting for it.
struct Live {
    stream: pw::stream::StreamRc,
    _listener: pw::stream::StreamListener<()>,
    frame: Rc<RefCell<Option<Vec<u8>>>>,
    width: u32,
    height: u32,
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
            Command::Create { id, width, height, reply } => {
                match create(&core, id, width, height, notices.clone(), pending.clone()) {
                    Ok(live) => {
                        pending.borrow_mut().insert(id, reply);
                        streams.borrow_mut().insert(id, live);
                    }
                    Err(err) => {
                        let _ = reply.send(Err(err));
                    }
                }
            }
            Command::Frame { id, bgrx } => {
                if let Some(live) = streams.borrow().get(&id) {
                    if bgrx.len() == (live.width * live.height * 4) as usize {
                        *live.frame.borrow_mut() = Some(bgrx);
                        if let Err(err) = live.stream.trigger_process() {
                            tracing::debug!(id, %err, "cannot trigger a frame");
                        }
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
    notices: LoopSender<Notice>,
    pending: Pending,
) -> Result<Live, String> {
    let props = pw::properties::properties! {
        *pw::keys::MEDIA_CLASS => "Video/Source",
        *pw::keys::MEDIA_ROLE => "Screen",
        *pw::keys::NODE_NAME => "helix-screen-cast",
    };
    let stream = pw::stream::StreamRc::new(core.clone(), "helix-screen-cast", props).map_err(|e| e.to_string())?;
    let frame: Rc<RefCell<Option<Vec<u8>>>> = Rc::default();
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
        .param_changed(move |stream, _, param_id, param| {
            if param_id != spa::param::ParamType::Format.as_raw() || param.is_none() {
                return;
            }
            // The format is fixed; say how big the buffers are.
            match buffers_pod(width, height).map(|bytes| (Pod::from_bytes(&bytes).map(|pod| stream.update_params(&mut [pod])), bytes.len())) {
                Ok((Some(Ok(())), _)) => tracing::debug!(id, "buffer parameters set"),
                other => tracing::warn!(id, ?other, "cannot set buffer parameters"),
            }
        })
        .add_buffer(move |_, _, buffer| {
            // SAFETY: PipeWire hands us a buffer with one data block to fill
            // in; we own the memfd and mapping until remove_buffer.
            unsafe { allocate(buffer, width, height) };
        })
        .remove_buffer(|_, _, buffer| {
            // SAFETY: the same buffer, allocated by `allocate`.
            unsafe { release(buffer) };
        })
        .process({
            let frame = frame.clone();
            move |stream, _| {
                let Some(pixels) = frame.borrow_mut().take() else {
                    tracing::trace!(id, "process without a frame");
                    return;
                };
                let Some(mut buffer) = stream.dequeue_buffer() else {
                    tracing::debug!(id, "no free buffer; frame dropped");
                    return;
                };
                let datas = buffer.datas_mut();
                let Some(data) = datas.first_mut() else { return };
                let stride = (width * 4) as usize;
                let copied = match data.data() {
                    Some(slice) if slice.len() >= pixels.len() => {
                        slice[..pixels.len()].copy_from_slice(&pixels);
                        pixels.len()
                    }
                    _ => 0,
                };
                tracing::trace!(id, copied, "frame queued");
                let chunk = data.chunk_mut();
                *chunk.offset_mut() = 0;
                *chunk.stride_mut() = stride as i32;
                *chunk.size_mut() = copied as u32;
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
    Ok(Live { stream, _listener: listener, frame, width, height })
}
