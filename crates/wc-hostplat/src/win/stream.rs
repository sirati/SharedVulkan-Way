//! WASAPI shared-mode streams on one endpoint each, chosen by id: a render
//! stream that plays what is written to it, and a capture stream that is
//! read from. s16le interleaved on our side whatever the device's mix format
//! is (`AUTOCONVERTPCM`, with the default-quality resampler), event-driven,
//! on a thread of their own.
//!
//! **Bounded latency, by dropping** ([`crate::pcm`]): what is queued for the
//! device is at most `queue` long, and samples arriving at a full queue are
//! dropped and counted, as §86's waveOut player does.
//!
//! **The microphone is opened only on demand.** A [`Capture`] holds the
//! endpoint from [`Capture::start`] until it is dropped: the sound-ring
//! wiring starts one when a Linux client starts recording from the source
//! node and drops it when the last one stops, so Windows' microphone
//! indicator is on exactly while Linux records.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::Duration;

use windows_sys::Win32::Foundation::{CloseHandle, WAIT_OBJECT_0};
use windows_sys::Win32::System::Com::CLSCTX_ALL;
use windows_sys::Win32::System::Threading::{
    CreateEventW, GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_TIME_CRITICAL, WaitForSingleObject,
};

use super::audio::{device, enumerator};
use super::com::*;
use crate::pcm::{PcmReader, PcmWriter, Pipe, Stats};

const AUDCLNT_SHAREMODE_SHARED: i32 = 0;
const AUDCLNT_STREAMFLAGS_EVENTCALLBACK: u32 = 0x0004_0000;
const AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM: u32 = 0x8000_0000;
const AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY: u32 = 0x0800_0000;
const AUDCLNT_BUFFERFLAGS_SILENT: u32 = 0x2;
const AUDCLNT_E_DEVICE_INVALIDATED: i32 = 0x8889_0004_u32 as i32;
const WAVE_FORMAT_PCM: u16 = 1;

/// The stream's format on our side: s16le, interleaved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Format {
    pub rate: u32,
    pub channels: u16,
}

impl Format {
    pub fn frame_bytes(&self) -> usize {
        usize::from(self.channels) * 2
    }
    fn frames_in(&self, d: Duration) -> usize {
        (u128::from(self.rate) * d.as_micros() / 1_000_000) as usize
    }
    /// `WAVEFORMATEX` for it, as the 18 bytes WASAPI reads.
    fn waveformatex(&self) -> [u8; 18] {
        let mut b = [0u8; 18];
        let block = self.channels * 2;
        b[0..2].copy_from_slice(&WAVE_FORMAT_PCM.to_le_bytes());
        b[2..4].copy_from_slice(&self.channels.to_le_bytes());
        b[4..8].copy_from_slice(&self.rate.to_le_bytes());
        b[8..12].copy_from_slice(&(self.rate * u32::from(block)).to_le_bytes());
        b[12..14].copy_from_slice(&block.to_le_bytes());
        b[14..16].copy_from_slice(&16u16.to_le_bytes());
        b
    }
}

/// How a stream is sized.
#[derive(Debug, Clone, Copy)]
pub struct Sizing {
    /// The most audio queued between us and the device.
    pub queue: Duration,
    /// The device buffer WASAPI is asked for (shared mode may round up).
    pub device: Duration,
}

impl Default for Sizing {
    /// 100 ms of queue, 40 ms of device buffer: what §86's 160 ms bound
    /// becomes with an event-driven device.
    fn default() -> Self {
        Sizing { queue: Duration::from_millis(100), device: Duration::from_millis(40) }
    }
}

/// Called on the device thread with the bytes of real samples the device
/// just took from the queue (render).
pub type OnDrain = Box<dyn FnMut(usize) + Send>;
/// Called on the device thread with every packet the device recorded
/// (capture), instead of queueing it.
pub type OnData = Box<dyn FnMut(&[u8]) + Send>;

#[derive(Default)]
struct Hooks {
    /// The queue is the caller's: it outlives the stream and is not closed.
    shared: bool,
    on_drain: Option<OnDrain>,
    on_data: Option<OnData>,
}

struct Running {
    pipe: Pipe,
    stop: Arc<AtomicBool>,
    alive: Arc<AtomicBool>,
    shared: bool,
    thread: Option<std::thread::JoinHandle<()>>,
    fmt: Format,
}

impl Running {
    fn stop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
        if !self.shared {
            self.pipe.close();
        }
    }
}

/// A render stream to one endpoint. Write PCM through [`Render::writer`];
/// the stream ends when this is dropped or the device goes away (then the
/// writer fails with `BrokenPipe`).
pub struct Render(Running);

/// A capture stream from one endpoint. Read PCM through
/// [`Capture::reader`]; the endpoint is released when this is dropped.
pub struct Capture(Running);

#[derive(Clone, Copy, PartialEq, Eq)]
enum Way {
    Render,
    Capture,
}

impl Render {
    /// Open endpoint `id` and start playing (silence until written to).
    pub fn start(id: &str, fmt: Format, sizing: Sizing, log: impl FnMut(&str) + Send + 'static) -> Result<Render, String> {
        start(Way::Render, id, fmt, sizing, None, Hooks::default(), Box::new(log)).map(Render)
    }
    /// Play from `pipe`, the caller's queue, which outlives this stream (so
    /// a stream can be stopped while idle and started again on the same
    /// queue), and report every take from it with `on_drain`: the sound
    /// ring grants that much credit back. `pipe`'s frame must be `fmt`'s.
    pub fn start_shared(
        id: &str,
        fmt: Format,
        sizing: Sizing,
        pipe: Pipe,
        on_drain: OnDrain,
        log: impl FnMut(&str) + Send + 'static,
    ) -> Result<Render, String> {
        let h = Hooks { shared: true, on_drain: Some(on_drain), on_data: None };
        start(Way::Render, id, fmt, sizing, Some(pipe), h, Box::new(log)).map(Render)
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
        self.0.alive.load(Ordering::Acquire)
    }
}

impl Capture {
    /// Open endpoint `id` and start recording. Call only while Linux
    /// records; drop it when Linux stops.
    pub fn start(id: &str, fmt: Format, sizing: Sizing, log: impl FnMut(&str) + Send + 'static) -> Result<Capture, String> {
        start(Way::Capture, id, fmt, sizing, None, Hooks::default(), Box::new(log)).map(Capture)
    }
    /// Record and hand every packet to `on_data` on the device thread
    /// rather than queueing it (the sound ring decides, by its credit, what
    /// is sent and what is an overrun). [`Capture::reader`] then reads
    /// nothing.
    pub fn start_to(
        id: &str,
        fmt: Format,
        sizing: Sizing,
        on_data: OnData,
        log: impl FnMut(&str) + Send + 'static,
    ) -> Result<Capture, String> {
        let h = Hooks { shared: false, on_drain: None, on_data: Some(on_data) };
        start(Way::Capture, id, fmt, sizing, None, h, Box::new(log)).map(Capture)
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
        self.0.alive.load(Ordering::Acquire)
    }
}

impl Drop for Render {
    fn drop(&mut self) {
        self.0.stop();
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        self.0.stop();
    }
}

type Log = Box<dyn FnMut(&str) + Send>;

fn start(way: Way, id: &str, fmt: Format, sizing: Sizing, pipe: Option<Pipe>, hooks: Hooks, log: Log) -> Result<Running, String> {
    if fmt.channels == 0 || fmt.channels > 8 || !(8_000..=192_000).contains(&fmt.rate) {
        return Err(format!("unsupported format {fmt:?}"));
    }
    let pipe = pipe.unwrap_or_else(|| Pipe::new(fmt.frame_bytes(), fmt.frames_in(sizing.queue).max(1)));
    if pipe.frame_bytes() != fmt.frame_bytes() {
        return Err("the queue's frame is not the format's".into());
    }
    let stop = Arc::new(AtomicBool::new(false));
    let alive = Arc::new(AtomicBool::new(true));
    let shared = hooks.shared;
    let (ready_tx, ready_rx) = mpsc::channel();
    let (p2, s2, a2, id2) = (pipe.clone(), stop.clone(), alive.clone(), id.to_string());
    let thread = std::thread::Builder::new()
        .name(if way == Way::Render { "wasapi-render" } else { "wasapi-capture" }.into())
        .spawn(move || {
            let mut log = log;
            let mut hooks = hooks;
            let r = device_thread(way, &id2, fmt, sizing, &p2, &s2, &ready_tx, &mut hooks, &mut log);
            if let Err(e) = &r {
                // Before readiness, the error goes back to `start`; after it,
                // to the log.
                if ready_tx.send(Err(e.clone())).is_err() {
                    log(&format!("wasapi: {e}"));
                }
            }
            a2.store(false, Ordering::Release);
            if !hooks.shared {
                p2.close();
            }
        })
        .map_err(|e| format!("spawn: {e}"))?;
    let mut running = Running { pipe, stop, alive, shared, thread: Some(thread), fmt };
    match ready_rx.recv() {
        Ok(Ok(())) => Ok(running),
        Ok(Err(e)) => {
            running.stop();
            Err(e)
        }
        Err(_) => {
            running.stop();
            Err("the stream thread ended".into())
        }
    }
}

struct Event(*mut core::ffi::c_void);
impl Drop for Event {
    fn drop(&mut self) {
        // SAFETY: an event this thread created.
        unsafe { CloseHandle(self.0) };
    }
}

#[allow(clippy::too_many_arguments)]
fn device_thread(
    way: Way,
    id: &str,
    fmt: Format,
    sizing: Sizing,
    pipe: &Pipe,
    stop: &AtomicBool,
    ready: &mpsc::Sender<Result<(), String>>,
    hooks: &mut Hooks,
    log: &mut Log,
) -> Result<(), String> {
    let _com = Apartment::mta();
    // SAFETY: raising this thread's own priority.
    unsafe { SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_TIME_CRITICAL) };
    let e = enumerator()?;
    let d = device(&e, id)?;
    // SAFETY: a live device; the client is released on drop.
    let client: Com<AudioClientVtbl> = unsafe {
        out("Activate(IAudioClient)", |p| (d.v().activate)(d.as_raw(), &IID_AUDIO_CLIENT, CLSCTX_ALL, std::ptr::null(), p))?
    };
    let wfx = fmt.waveformatex();
    let hns = (sizing.device.as_nanos() / 100) as i64;
    let flags =
        AUDCLNT_STREAMFLAGS_EVENTCALLBACK | AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY;
    // SAFETY: a valid WAVEFORMATEX that outlives the call.
    let hr = unsafe {
        (client.v().initialize)(client.as_raw(), AUDCLNT_SHAREMODE_SHARED, flags, hns, 0, wfx.as_ptr().cast(), std::ptr::null())
    };
    if hr < 0 {
        return Err(format!("IAudioClient::Initialize({fmt:?}): {}", hr_text(hr)));
    }
    // SAFETY: an auto-reset, unnamed event.
    let ev = Event(unsafe { CreateEventW(std::ptr::null(), 0, 0, std::ptr::null()) });
    if ev.0.is_null() {
        return Err("CreateEventW failed".into());
    }
    // SAFETY: a live client and event.
    let hr = unsafe { (client.v().set_event_handle)(client.as_raw(), ev.0) };
    if hr < 0 {
        return Err(format!("SetEventHandle: {}", hr_text(hr)));
    }
    let mut buf_frames = 0u32;
    // SAFETY: a live, initialised client.
    unsafe { (client.v().get_buffer_size)(client.as_raw(), &mut buf_frames) };
    let fb = fmt.frame_bytes();

    match way {
        Way::Render => {
            // SAFETY: GetService with the render client's IID.
            let rc: Com<AudioRenderClientVtbl> = unsafe {
                out("GetService(IAudioRenderClient)", |p| {
                    (client.v().get_service)(client.as_raw(), &IID_AUDIO_RENDER_CLIENT, p)
                })?
            };
            // SAFETY: a live client.
            let hr = unsafe { (client.v().start)(client.as_raw()) };
            if hr < 0 {
                return Err(format!("IAudioClient::Start: {}", hr_text(hr)));
            }
            let _ = ready.send(Ok(()));
            log(&format!("wasapi: rendering {fmt:?} to one endpoint, device buffer {buf_frames} frames"));
            while !stop.load(Ordering::Acquire) {
                // SAFETY: a live event.
                if unsafe { WaitForSingleObject(ev.0, 200) } != WAIT_OBJECT_0 {
                    continue;
                }
                let mut pad = 0u32;
                // SAFETY: a live client.
                let hr = unsafe { (client.v().get_current_padding)(client.as_raw(), &mut pad) };
                if hr < 0 {
                    return Err(gone("GetCurrentPadding", hr));
                }
                let n = buf_frames.saturating_sub(pad);
                if n == 0 {
                    continue;
                }
                let mut data: *mut u8 = std::ptr::null_mut();
                // SAFETY: n frames are free in the device buffer.
                let hr = unsafe { (rc.v().get_buffer)(rc.as_raw(), n, &mut data) };
                if hr < 0 {
                    return Err(gone("IAudioRenderClient::GetBuffer", hr));
                }
                // SAFETY: GetBuffer gave n frames of our format at `data`.
                let out = unsafe { std::slice::from_raw_parts_mut(data, n as usize * fb) };
                let took = pipe.pop_or_silence(out);
                if took > 0
                    && let Some(f) = hooks.on_drain.as_mut()
                {
                    f(took);
                }
                // SAFETY: releasing exactly what was got.
                unsafe { (rc.v().release_buffer)(rc.as_raw(), n, 0) };
            }
        }
        Way::Capture => {
            // SAFETY: GetService with the capture client's IID.
            let cc: Com<AudioCaptureClientVtbl> = unsafe {
                out("GetService(IAudioCaptureClient)", |p| {
                    (client.v().get_service)(client.as_raw(), &IID_AUDIO_CAPTURE_CLIENT, p)
                })?
            };
            // SAFETY: a live client.
            let hr = unsafe { (client.v().start)(client.as_raw()) };
            if hr < 0 {
                return Err(format!("IAudioClient::Start: {}", hr_text(hr)));
            }
            let _ = ready.send(Ok(()));
            log(&format!("wasapi: capturing {fmt:?} from one endpoint, device buffer {buf_frames} frames"));
            let mut silence = Vec::new();
            'outer: while !stop.load(Ordering::Acquire) {
                // SAFETY: a live event.
                if unsafe { WaitForSingleObject(ev.0, 200) } != WAIT_OBJECT_0 {
                    continue;
                }
                loop {
                    let mut next = 0u32;
                    // SAFETY: a live capture client.
                    let hr = unsafe { (cc.v().get_next_packet_size)(cc.as_raw(), &mut next) };
                    if hr < 0 {
                        return Err(gone("GetNextPacketSize", hr));
                    }
                    if next == 0 {
                        break;
                    }
                    let (mut data, mut frames, mut fl) = (std::ptr::null_mut::<u8>(), 0u32, 0u32);
                    // SAFETY: out pointers; positions not wanted.
                    let hr = unsafe {
                        (cc.v().get_buffer)(cc.as_raw(), &mut data, &mut frames, &mut fl, std::ptr::null_mut(), std::ptr::null_mut())
                    };
                    if hr < 0 {
                        return Err(gone("IAudioCaptureClient::GetBuffer", hr));
                    }
                    let bytes = frames as usize * fb;
                    let pcm: &[u8] = if fl & AUDCLNT_BUFFERFLAGS_SILENT != 0 || data.is_null() {
                        silence.resize(bytes, 0);
                        &silence
                    } else {
                        // SAFETY: `frames` frames of our format at `data`, until released.
                        unsafe { std::slice::from_raw_parts(data, bytes) }
                    };
                    match hooks.on_data.as_mut() {
                        Some(f) => f(pcm),
                        None => {
                            pipe.push(pcm);
                        }
                    }
                    // SAFETY: releasing exactly what was got.
                    unsafe { (cc.v().release_buffer)(cc.as_raw(), frames) };
                    if stop.load(Ordering::Acquire) {
                        break 'outer;
                    }
                }
            }
        }
    }
    // SAFETY: a live, started client.
    unsafe { (client.v().stop)(client.as_raw()) };
    log(&format!("wasapi: stream stopped: {}", pipe.stats().line(fmt.rate)));
    Ok(())
}

fn gone(what: &str, hr: i32) -> String {
    if hr == AUDCLNT_E_DEVICE_INVALIDATED {
        format!("{what}: the endpoint went away")
    } else {
        format!("{what}: {}", hr_text(hr))
    }
}
