//! AudioQueue streams on one CoreAudio device each, chosen by UID: a render
//! stream that plays what is written to it and a capture stream that is
//! read from. s16le interleaved on our side; the queue converts to the
//! device's format and rate. The callbacks run on AudioQueue's own thread
//! (no run loop is given), as `wc-launch`'s `audio_mac.rs` player does.
//!
//! **Bounded latency, by dropping** ([`crate::pcm`]): at most `queue` of
//! audio waits for the device, and what arrives at a full queue is dropped
//! and counted, never queued behind the rest.
//!
//! **The microphone is opened only on demand and only in the main
//! process.** A [`Capture`] holds the device from [`Capture::start`] until it
//! is dropped: the sound-ring wiring starts one when a Linux client starts
//! recording from the source node and drops it when the last one stops
//! (security-model R37), so the menu bar's microphone indicator is on
//! exactly while Linux records. `start` refuses unless this process's
//! bundle is `org.waycompose.host` (TCC rule 1): the grant belongs to that
//! identity, and a face must never be the one macOS asks about.

use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use super::cf;
use super::plan::HOST_BUNDLE_ID;
use crate::pcm::{PcmReader, PcmWriter, Pipe, Stats};

/// The stream's format on our side: s16le, interleaved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Format {
    pub rate: u32,
    pub channels: u16,
}

impl Format {
    pub fn frame_bytes(&self) -> usize {
        usize::from(self.channels.max(1)) * 2
    }
    fn frames_in(&self, d: Duration) -> usize {
        (u128::from(self.rate) * d.as_micros() / 1_000_000) as usize
    }
}

/// How a stream is sized.
#[derive(Debug, Clone, Copy)]
pub struct Sizing {
    /// The most audio queued between us and the device.
    pub queue: Duration,
    /// The audio in AudioQueue's buffers, in [`BUFFERS`] buffers.
    pub device: Duration,
}

impl Default for Sizing {
    /// The Windows streams' sizes: 100 ms of queue, 40 ms in the device's
    /// buffers.
    fn default() -> Self {
        Sizing { queue: Duration::from_millis(100), device: Duration::from_millis(40) }
    }
}

/// AudioQueue buffers per stream; `Sizing::device` is split among them.
pub const BUFFERS: usize = 4;

#[repr(C)]
struct Asbd {
    sample_rate: f64,
    format_id: u32,
    format_flags: u32,
    bytes_per_packet: u32,
    frames_per_packet: u32,
    bytes_per_frame: u32,
    channels_per_frame: u32,
    bits_per_channel: u32,
    reserved: u32,
}

#[repr(C)]
struct AqBuffer {
    capacity: u32,
    data: *mut c_void,
    size: u32,
    user: *mut c_void,
    packet_desc_capacity: u32,
    packet_descs: *mut c_void,
    packet_desc_count: u32,
}

type Aq = *mut c_void;
type OutputCallback = extern "C" fn(*mut c_void, Aq, *mut AqBuffer);
type InputCallback = extern "C" fn(*mut c_void, Aq, *mut AqBuffer, *const c_void, u32, *const c_void);
type PropertyListener = extern "C" fn(*mut c_void, Aq, u32);

#[link(name = "AudioToolbox", kind = "framework")]
unsafe extern "C" {
    fn AudioQueueNewOutput(f: *const Asbd, cb: OutputCallback, user: *mut c_void, rl: *const c_void, mode: *const c_void, flags: u32, out: *mut Aq) -> i32;
    fn AudioQueueNewInput(f: *const Asbd, cb: InputCallback, user: *mut c_void, rl: *const c_void, mode: *const c_void, flags: u32, out: *mut Aq) -> i32;
    fn AudioQueueSetProperty(q: Aq, id: u32, data: *const c_void, size: u32) -> i32;
    fn AudioQueueGetProperty(q: Aq, id: u32, data: *mut c_void, size: *mut u32) -> i32;
    fn AudioQueueAddPropertyListener(q: Aq, id: u32, l: PropertyListener, user: *mut c_void) -> i32;
    fn AudioQueueAllocateBuffer(q: Aq, size: u32, out: *mut *mut AqBuffer) -> i32;
    fn AudioQueueEnqueueBuffer(q: Aq, b: *mut AqBuffer, n: u32, descs: *const c_void) -> i32;
    fn AudioQueueStart(q: Aq, at: *const c_void) -> i32;
    fn AudioQueueStop(q: Aq, immediate: u8) -> i32;
    fn AudioQueueDispose(q: Aq, immediate: u8) -> i32;
}

const LPCM: u32 = u32::from_be_bytes(*b"lpcm");
/// Signed integer, packed, little endian (no big-endian flag).
const S16_PACKED: u32 = 0x4 | 0x8;
/// `kAudioQueueProperty_CurrentDevice`: the device's UID, a CFString.
const CURRENT_DEVICE: u32 = u32::from_be_bytes(*b"aqcd");
/// `kAudioQueueProperty_IsRunning`.
const IS_RUNNING: u32 = u32::from_be_bytes(*b"aqrn");

/// What the callbacks share with the stream's owner.
struct Inner {
    pipe: Pipe,
    /// Set before the queue is stopped on purpose; the callbacks stop
    /// re-enqueueing, and the running listener does not take it as a loss.
    stopping: AtomicBool,
}

extern "C" fn render(user: *mut c_void, q: Aq, b: *mut AqBuffer) {
    // SAFETY: `user` is the `Inner` the stream keeps alive until after the
    // queue is disposed; `b` is this queue's buffer, ours until enqueued.
    unsafe {
        let inner = &*(user as *const Inner);
        let b = &mut *b;
        let out = std::slice::from_raw_parts_mut(b.data.cast::<u8>(), b.capacity as usize);
        inner.pipe.pop_or_silence(out);
        b.size = b.capacity;
        if !inner.stopping.load(Ordering::Acquire) {
            AudioQueueEnqueueBuffer(q, b, 0, std::ptr::null());
        }
    }
}

extern "C" fn capture(user: *mut c_void, q: Aq, b: *mut AqBuffer, _t: *const c_void, _n: u32, _d: *const c_void) {
    // SAFETY: as in `render`.
    unsafe {
        let inner = &*(user as *const Inner);
        let b = &mut *b;
        let data = std::slice::from_raw_parts(b.data.cast::<u8>(), b.size as usize);
        inner.pipe.push(data);
        if !inner.stopping.load(Ordering::Acquire) {
            AudioQueueEnqueueBuffer(q, b, 0, std::ptr::null());
        }
    }
}

extern "C" fn running_changed(user: *mut c_void, q: Aq, _id: u32) {
    // SAFETY: as in `render`.
    unsafe {
        let inner = &*(user as *const Inner);
        let mut on = 1u32;
        let mut size = 4u32;
        if AudioQueueGetProperty(q, IS_RUNNING, (&raw mut on).cast(), &mut size) == 0
            && on == 0
            && !inner.stopping.load(Ordering::Acquire)
        {
            // The device went away (unplugged, or the system stopped us).
            inner.pipe.close();
        }
    }
}

struct Running {
    q: Aq,
    inner: *const Inner,
    pipe: Pipe,
    fmt: Format,
}

// SAFETY: the queue and the shared state are only touched again in drop.
unsafe impl Send for Running {}

impl Drop for Running {
    fn drop(&mut self) {
        // SAFETY: an immediate stop and dispose: once they return, AudioQueue
        // calls neither callback again, so the shared state can go.
        unsafe {
            (*self.inner).stopping.store(true, Ordering::Release);
            AudioQueueStop(self.q, 1);
            AudioQueueDispose(self.q, 1);
            drop(Arc::from_raw(self.inner));
        }
        self.pipe.close();
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Way {
    Render,
    Capture,
}

fn start(way: Way, uid: Option<&str>, fmt: Format, sizing: Sizing) -> Result<Running, String> {
    if fmt.rate == 0 || fmt.channels == 0 || fmt.channels > 8 {
        return Err(format!("unsupported format: {fmt:?}"));
    }
    let fb = fmt.frame_bytes();
    let asbd = Asbd {
        sample_rate: f64::from(fmt.rate),
        format_id: LPCM,
        format_flags: S16_PACKED,
        bytes_per_packet: fb as u32,
        frames_per_packet: 1,
        bytes_per_frame: fb as u32,
        channels_per_frame: u32::from(fmt.channels),
        bits_per_channel: 16,
        reserved: 0,
    };
    let pipe = Pipe::new(fb, fmt.frames_in(sizing.queue).max(1));
    let inner = Arc::into_raw(Arc::new(Inner { pipe: pipe.clone(), stopping: AtomicBool::new(false) }));
    let user = inner as *mut c_void;
    let mut q: Aq = std::ptr::null_mut();
    // SAFETY: a valid format, callbacks of the right types, and `user` alive
    // until `Running::drop` has disposed of the queue. No run loop: the
    // callbacks come on AudioQueue's own thread.
    let st = unsafe {
        match way {
            Way::Render => AudioQueueNewOutput(&asbd, render, user, std::ptr::null(), std::ptr::null(), 0, &mut q),
            Way::Capture => AudioQueueNewInput(&asbd, capture, user, std::ptr::null(), std::ptr::null(), 0, &mut q),
        }
    };
    if st != 0 {
        // SAFETY: no queue exists, so nothing else holds `inner`.
        unsafe { drop(Arc::from_raw(inner)) };
        return Err(format!("AudioQueueNew{}: OSStatus {st}", if way == Way::Render { "Output" } else { "Input" }));
    }
    let run = Running { q, inner, pipe, fmt };
    // SAFETY (all below): `q` is the live queue `run` owns; property values
    // are of the documented types and sizes.
    if let Some(uid) = uid {
        let s = cf::string(uid);
        let v = s.get();
        let st = unsafe { AudioQueueSetProperty(q, CURRENT_DEVICE, (&raw const v).cast(), std::mem::size_of_val(&v) as u32) };
        if st != 0 {
            return Err(format!("no such device {uid:?} (OSStatus {st})"));
        }
    }
    unsafe { AudioQueueAddPropertyListener(q, IS_RUNNING, running_changed, user) };
    let per = (fmt.frames_in(sizing.device) / BUFFERS).max(64) * fb;
    for _ in 0..BUFFERS {
        let mut b: *mut AqBuffer = std::ptr::null_mut();
        let st = unsafe { AudioQueueAllocateBuffer(q, per as u32, &mut b) };
        if st != 0 {
            return Err(format!("AudioQueueAllocateBuffer: OSStatus {st}"));
        }
        match way {
            // Silence first, then whatever is written.
            Way::Render => render(user, q, b),
            Way::Capture => unsafe {
                AudioQueueEnqueueBuffer(q, b, 0, std::ptr::null());
            },
        }
    }
    let st = unsafe { AudioQueueStart(q, std::ptr::null()) };
    if st != 0 {
        return Err(format!("AudioQueueStart: OSStatus {st}"));
    }
    Ok(run)
}

/// A render stream to one device. Write PCM through [`Render::writer`]; the
/// stream ends when this is dropped or the device goes away (then the
/// writer fails with `BrokenPipe`).
pub struct Render(Running);

impl Render {
    /// Play to the device with this UID (`None`: the system's default
    /// output, followed when it changes). Silence until written to.
    pub fn start(uid: Option<&str>, fmt: Format, sizing: Sizing) -> Result<Render, String> {
        start(Way::Render, uid, fmt, sizing).map(Render)
    }
    pub fn writer(&self) -> PcmWriter {
        PcmWriter::new(self.0.pipe.clone())
    }
    pub fn format(&self) -> Format {
        self.0.fmt
    }
    pub fn stats(&self) -> Stats {
        self.0.pipe.stats()
    }
    /// False once the device went away.
    pub fn running(&self) -> bool {
        !self.0.pipe.is_closed()
    }
}

/// A capture stream from one device. Read PCM through [`Capture::reader`];
/// the microphone is released when this is dropped.
pub struct Capture(Running);

/// Whether this process may open a microphone: only the main process,
/// whose bundle is `org.waycompose.host`.
pub fn may_capture() -> Result<(), String> {
    match cf::main_bundle_id() {
        Some(id) if id == HOST_BUNDLE_ID => Ok(()),
        other => Err(format!(
            "capture only in the main process ({HOST_BUNDLE_ID}); this process is {}",
            other.as_deref().unwrap_or("not a bundle")
        )),
    }
}

impl Capture {
    /// Record from the device with this UID (`None`: the default input).
    /// Call only while Linux records; drop it when Linux stops.
    pub fn start(uid: Option<&str>, fmt: Format, sizing: Sizing) -> Result<Capture, String> {
        may_capture()?;
        start(Way::Capture, uid, fmt, sizing).map(Capture)
    }
    pub fn reader(&self) -> PcmReader {
        PcmReader::new(self.0.pipe.clone())
    }
    pub fn format(&self) -> Format {
        self.0.fmt
    }
    pub fn stats(&self) -> Stats {
        self.0.pipe.stats()
    }
    pub fn running(&self) -> bool {
        !self.0.pipe.is_closed()
    }
}
