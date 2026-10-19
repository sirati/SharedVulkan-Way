//! §103.16: the Mac end of the sound ring (`sv_transport::soundring`): one
//! stream per CoreAudio device and direction, the defaults announced as they
//! change, PCM played through an AudioQueue on its own device, and microphones
//! recorded only between the session side's `Start` and `Stop`.
//!
//! Credit, not drops: each output stream grants the session side what its
//! queue holds (`CREDIT_INIT`) and gives back what the AudioQueue has played,
//! so the session side's pipe sink, and with it the Linux application, waits
//! for the Mac rather than the Mac discarding. An input stream sends no more
//! than the session side granted; what a microphone delivers beyond that is
//! counted as an overrun, because a microphone cannot be made to wait.

use std::collections::{HashMap, VecDeque};
use std::ffi::{c_char, c_void, CString};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use sv_transport::soundring::{Dir, Link, Msg, VERSION};

use crate::audio::{CHANNELS, CHUNK_BYTES, RATE};

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

#[repr(C)]
struct Addr {
    selector: u32,
    scope: u32,
    element: u32,
}

type Aq = *mut c_void;
type OutCb = extern "C" fn(*mut c_void, Aq, *mut AqBuffer);
type InCb = extern "C" fn(*mut c_void, Aq, *mut AqBuffer, *const c_void, u32, *const c_void);

#[link(name = "AudioToolbox", kind = "framework")]
unsafe extern "C" {
    fn AudioQueueNewOutput(f: *const Asbd, cb: OutCb, user: *mut c_void, rl: *const c_void, m: *const c_void, fl: u32, out: *mut Aq) -> i32;
    fn AudioQueueNewInput(f: *const Asbd, cb: InCb, user: *mut c_void, rl: *const c_void, m: *const c_void, fl: u32, out: *mut Aq) -> i32;
    fn AudioQueueAllocateBuffer(q: Aq, size: u32, out: *mut *mut AqBuffer) -> i32;
    fn AudioQueueEnqueueBuffer(q: Aq, b: *mut AqBuffer, n: u32, d: *const c_void) -> i32;
    fn AudioQueueStart(q: Aq, at: *const c_void) -> i32;
    fn AudioQueuePause(q: Aq) -> i32;
    fn AudioQueueStop(q: Aq, immediate: u8) -> i32;
    fn AudioQueueDispose(q: Aq, immediate: u8) -> i32;
    fn AudioQueueSetParameter(q: Aq, p: u32, v: f32) -> i32;
    fn AudioQueueSetProperty(q: Aq, id: u32, data: *const c_void, size: u32) -> i32;
}

#[link(name = "CoreAudio", kind = "framework")]
unsafe extern "C" {
    fn AudioObjectGetPropertyDataSize(id: u32, a: *const Addr, qs: u32, qd: *const c_void, size: *mut u32) -> i32;
    fn AudioObjectGetPropertyData(id: u32, a: *const Addr, qs: u32, qd: *const c_void, size: *mut u32, data: *mut c_void) -> i32;
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFStringGetCString(s: *const c_void, buf: *mut c_char, len: isize, enc: u32) -> u8;
    fn CFStringCreateWithCString(a: *const c_void, s: *const c_char, enc: u32) -> *const c_void;
    fn CFRelease(cf: *const c_void);
}

const fn fc(s: &[u8; 4]) -> u32 {
    u32::from_be_bytes(*s)
}
const SYSTEM: u32 = 1;
const DEVICES: u32 = fc(b"dev#");
const DEFAULT_OUT: u32 = fc(b"dOut");
const DEFAULT_IN: u32 = fc(b"dIn ");
const NAME: u32 = fc(b"lnam");
const UID: u32 = fc(b"uid ");
const STREAMS: u32 = fc(b"stm#");
const GLOBAL: u32 = fc(b"glob");
const OUTPUT: u32 = fc(b"outp");
const INPUT: u32 = fc(b"inpt");
const CURRENT_DEVICE: u32 = fc(b"aqcd");
const UTF8: u32 = 0x0800_0100;
const LPCM: u32 = fc(b"lpcm");
const S16_PACKED: u32 = 0x4 | 0x8;
const PARAM_VOLUME: u32 = 1;

/// What an output stream's queue holds, granted up front: 240 ms.
const CREDIT_INIT: u32 = (CHUNK_BYTES * 12) as u32;
/// AudioQueue buffers per output: 6 x 20 ms.
const AQ_BUFFERS: usize = 6;
/// Queued before a stopped output starts: 60 ms.
const PREBUFFER: usize = CHUNK_BYTES * 3;
/// An output with nothing to play for this long is paused.
const IDLE_PAUSE: Duration = Duration::from_secs(3);

fn format() -> Asbd {
    Asbd {
        sample_rate: f64::from(RATE),
        format_id: LPCM,
        format_flags: S16_PACKED,
        bytes_per_packet: 4,
        frames_per_packet: 1,
        bytes_per_frame: 4,
        channels_per_frame: u32::from(CHANNELS),
        bits_per_channel: 16,
        reserved: 0,
    }
}

fn prop_size(id: u32, sel: u32, scope: u32) -> Option<u32> {
    let a = Addr { selector: sel, scope, element: 0 };
    let mut n = 0u32;
    // SAFETY: a valid address and out-parameter.
    (unsafe { AudioObjectGetPropertyDataSize(id, &a, 0, std::ptr::null(), &mut n) } == 0).then_some(n)
}

fn prop_u32(id: u32, sel: u32) -> Option<u32> {
    let a = Addr { selector: sel, scope: GLOBAL, element: 0 };
    let (mut v, mut n) = (0u32, 4u32);
    // SAFETY: a 4-byte property into a 4-byte value.
    (unsafe { AudioObjectGetPropertyData(id, &a, 0, std::ptr::null(), &mut n, (&raw mut v).cast()) } == 0).then_some(v)
}

fn prop_string(id: u32, sel: u32) -> Option<String> {
    let a = Addr { selector: sel, scope: GLOBAL, element: 0 };
    let mut s: *const c_void = std::ptr::null();
    let mut n = std::mem::size_of::<*const c_void>() as u32;
    // SAFETY: a CFStringRef property into a pointer; the string is ours (+1).
    if unsafe { AudioObjectGetPropertyData(id, &a, 0, std::ptr::null(), &mut n, (&raw mut s).cast()) } != 0 || s.is_null() {
        return None;
    }
    let mut buf = vec![0 as c_char; 1024];
    // SAFETY: a live CFString and a buffer of the length passed.
    let ok = unsafe { CFStringGetCString(s, buf.as_mut_ptr(), buf.len() as isize, UTF8) } != 0;
    // SAFETY: the +1 reference the property gave.
    unsafe { CFRelease(s) };
    // SAFETY: NUL-terminated by CFStringGetCString when it succeeds.
    ok.then(|| unsafe { std::ffi::CStr::from_ptr(buf.as_ptr()) }.to_string_lossy().into_owned())
}

/// A device: its id now, its UID (stable), its name, and its directions.
#[derive(Clone)]
struct Device {
    id: u32,
    uid: String,
    name: String,
    out: bool,
    inp: bool,
}

fn devices() -> Vec<Device> {
    let Some(n) = prop_size(SYSTEM, DEVICES, GLOBAL) else { return Vec::new() };
    let mut ids = vec![0u32; n as usize / 4];
    let a = Addr { selector: DEVICES, scope: GLOBAL, element: 0 };
    let mut size = n;
    // SAFETY: a buffer of the size the property reported.
    if unsafe { AudioObjectGetPropertyData(SYSTEM, &a, 0, std::ptr::null(), &mut size, ids.as_mut_ptr().cast()) } != 0 {
        return Vec::new();
    }
    ids.truncate(size as usize / 4);
    ids.into_iter()
        .filter_map(|id| {
            Some(Device {
                id,
                uid: prop_string(id, UID)?,
                name: prop_string(id, NAME).unwrap_or_else(|| format!("device {id}")),
                out: prop_size(id, STREAMS, OUTPUT).is_some_and(|s| s > 0),
                inp: prop_size(id, STREAMS, INPUT).is_some_and(|s| s > 0),
            })
        })
        .collect()
}

fn set_device(q: Aq, uid: &str) -> i32 {
    let Ok(c) = CString::new(uid) else { return -1 };
    // SAFETY: a NUL-terminated UTF-8 string; the CFString is released after
    // the queue has copied the property.
    unsafe {
        let s = CFStringCreateWithCString(std::ptr::null(), c.as_ptr(), UTF8);
        if s.is_null() {
            return -1;
        }
        let r = AudioQueueSetProperty(q, CURRENT_DEVICE, (&raw const s).cast(), std::mem::size_of::<*const c_void>() as u32);
        CFRelease(s);
        r
    }
}

/// Numbers per stream, for the log.
#[derive(Default, Clone, Copy)]
struct Count {
    bytes: u64,
    underruns: u64,
    overruns: u64,
}

struct Out {
    buf: Mutex<VecDeque<u8>>,
    link: Link,
    stream: u16,
    count: Mutex<Count>,
}

extern "C" fn refill(user: *mut c_void, q: Aq, b: *mut AqBuffer) {
    // SAFETY: `user` is an `Arc<Out>` leaked for the queue's life; `b` is
    // this queue's buffer until it is enqueued again.
    unsafe {
        let o = &*(user as *const Out);
        let b = &mut *b;
        let dst = std::slice::from_raw_parts_mut(b.data.cast::<u8>(), b.capacity as usize);
        let n = {
            let mut src = o.buf.lock().unwrap_or_else(|e| e.into_inner());
            let n = src.len().min(dst.len()) & !3;
            for (d, s) in dst.iter_mut().zip(src.drain(..n)) {
                *d = s;
            }
            n
        };
        dst[n..].fill(0);
        {
            let mut c = o.count.lock().unwrap_or_else(|e| e.into_inner());
            c.bytes += n as u64;
            if n < dst.len() {
                c.underruns += 1;
            }
        }
        if n > 0 {
            o.link.send(Msg::Credit { stream: o.stream, bytes: n as u32 });
        }
        b.size = b.capacity;
        AudioQueueEnqueueBuffer(q, b, 0, std::ptr::null());
    }
}

struct Player {
    name: String,
    q: Aq,
    out: Arc<Out>,
    running: bool,
    last: Instant,
}

// SAFETY: an AudioQueueRef may be used from any thread.
unsafe impl Send for Player {}

impl Player {
    fn new(stream: u16, d: &Device, link: &Link, volume: u8) -> Result<Player, String> {
        let out = Arc::new(Out { buf: Mutex::new(VecDeque::new()), link: link.clone(), stream, count: Mutex::new(Count::default()) });
        let user = Arc::into_raw(Arc::clone(&out)) as *mut c_void;
        let mut q: Aq = std::ptr::null_mut();
        let f = format();
        // SAFETY: a valid format and callback; `user` leaked for the queue's life.
        let st = unsafe { AudioQueueNewOutput(&f, refill, user, std::ptr::null(), std::ptr::null(), 0, &mut q) };
        if st != 0 {
            return Err(format!("AudioQueueNewOutput: OSStatus {st}"));
        }
        let st = set_device(q, &d.uid);
        if st != 0 {
            return Err(format!("the queue would not take device {}: OSStatus {st}", d.name));
        }
        // SAFETY: the queue made above.
        unsafe {
            AudioQueueSetParameter(q, PARAM_VOLUME, f32::from(volume.min(100)) / 100.0);
            for _ in 0..AQ_BUFFERS {
                let mut b: *mut AqBuffer = std::ptr::null_mut();
                if AudioQueueAllocateBuffer(q, CHUNK_BYTES as u32, &mut b) == 0 {
                    (*b).size = (*b).capacity;
                    std::ptr::write_bytes((*b).data.cast::<u8>(), 0, (*b).capacity as usize);
                    AudioQueueEnqueueBuffer(q, b, 0, std::ptr::null());
                }
            }
        }
        Ok(Player { name: d.name.clone(), q, out, running: false, last: Instant::now() })
    }

    fn push(&mut self, pcm: &[u8]) {
        let queued = {
            let mut b = self.out.buf.lock().unwrap_or_else(|e| e.into_inner());
            b.extend(pcm.iter().copied());
            b.len()
        };
        self.last = Instant::now();
        if !self.running && queued >= PREBUFFER {
            // SAFETY: the queue made in `new`.
            self.running = unsafe { AudioQueueStart(self.q, std::ptr::null()) } == 0;
        }
    }

    fn idle(&mut self) {
        let empty = self.out.buf.lock().map(|b| b.is_empty()).unwrap_or(true);
        if self.running && empty && self.last.elapsed() > IDLE_PAUSE {
            // SAFETY: the queue made in `new`.
            unsafe { AudioQueuePause(self.q) };
            self.running = false;
        }
    }
}

impl Drop for Player {
    fn drop(&mut self) {
        // SAFETY: the queue made in `new`; the leaked `Arc<Out>` stays, small.
        unsafe {
            AudioQueueStop(self.q, 1);
            AudioQueueDispose(self.q, 1);
        }
    }
}

struct In {
    link: Link,
    stream: u16,
    credit: Mutex<u64>,
    count: Mutex<Count>,
}

extern "C" fn captured(user: *mut c_void, q: Aq, b: *mut AqBuffer, _t: *const c_void, _n: u32, _d: *const c_void) {
    // SAFETY: as `refill`.
    unsafe {
        let i = &*(user as *const In);
        let b = &mut *b;
        let src = std::slice::from_raw_parts(b.data.cast::<u8>(), b.size as usize);
        let n = {
            let mut c = i.credit.lock().unwrap_or_else(|e| e.into_inner());
            let n = (*c).min(src.len() as u64) as usize & !3;
            *c -= n as u64;
            n
        };
        if n > 0 {
            i.link.send(Msg::Data { stream: i.stream, pcm: src[..n].to_vec() });
        }
        {
            let mut c = i.count.lock().unwrap_or_else(|e| e.into_inner());
            c.bytes += n as u64;
            if n < src.len() {
                c.overruns += 1;
            }
        }
        AudioQueueEnqueueBuffer(q, b, 0, std::ptr::null());
    }
}

struct Recorder {
    uid: String,
    name: String,
    inp: Arc<In>,
    q: Option<Aq>,
}

// SAFETY: as `Player`.
unsafe impl Send for Recorder {}

impl Recorder {
    /// Open the microphone (macOS asks the user the first time: TCC, and
    /// `NSMicrophoneUsageDescription` in the application's bundle).
    fn start(&mut self) -> Result<(), String> {
        if self.q.is_some() {
            return Ok(());
        }
        let user = Arc::into_raw(Arc::clone(&self.inp)) as *mut c_void;
        let mut q: Aq = std::ptr::null_mut();
        let f = format();
        // SAFETY: as in `Player::new`.
        let st = unsafe { AudioQueueNewInput(&f, captured, user, std::ptr::null(), std::ptr::null(), 0, &mut q) };
        if st != 0 {
            return Err(format!("AudioQueueNewInput: OSStatus {st}"));
        }
        let st = set_device(q, &self.uid);
        if st != 0 {
            return Err(format!("the input queue would not take device {}: OSStatus {st}", self.name));
        }
        // SAFETY: the queue made above.
        unsafe {
            for _ in 0..3 {
                let mut b: *mut AqBuffer = std::ptr::null_mut();
                if AudioQueueAllocateBuffer(q, CHUNK_BYTES as u32, &mut b) == 0 {
                    AudioQueueEnqueueBuffer(q, b, 0, std::ptr::null());
                }
            }
            let st = AudioQueueStart(q, std::ptr::null());
            if st != 0 {
                AudioQueueDispose(q, 1);
                return Err(format!("AudioQueueStart (input): OSStatus {st} (no microphone permission?)"));
            }
        }
        self.q = Some(q);
        Ok(())
    }

    fn stop(&mut self) {
        if let Some(q) = self.q.take() {
            // SAFETY: the queue made in `start`.
            unsafe {
                AudioQueueStop(q, 1);
                AudioQueueDispose(q, 1);
            }
        }
    }
}

#[derive(Default)]
struct State {
    ids: HashMap<(String, bool), u16>,
    next: u16,
    players: HashMap<u16, Player>,
    recorders: HashMap<u16, Recorder>,
    default_out: Option<u16>,
    default_in: Option<u16>,
}

/// Run the Mac end of the sound ring until the channel ends.
pub fn run(
    ch: sv_transport::wlbytes::ByteChannel,
    control: &Arc<sv_transport::control::Control>,
    volume: u8,
    say: impl Fn(&str) + Send + Sync + Clone + 'static,
) -> Result<(), String> {
    let state = Arc::new(Mutex::new(State::default()));
    let ended = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let (s1, say1) = (Arc::clone(&state), say.clone());
    let e1 = Arc::clone(&ended);
    let say2 = say.clone();
    let link = sv_transport::soundring::spawn(
        ch,
        control,
        wc_compositor::ring_wait_slice(),
        move |m, _link| {
            let mut st = s1.lock().unwrap_or_else(|e| e.into_inner());
            match m {
                Msg::Data { stream, pcm } => {
                    if let Some(p) = st.players.get_mut(&stream) {
                        p.push(&pcm);
                    }
                }
                Msg::Credit { stream, bytes } => {
                    if let Some(r) = st.recorders.get(&stream) {
                        *r.inp.credit.lock().unwrap_or_else(|e| e.into_inner()) += u64::from(bytes);
                    }
                }
                Msg::Start { stream } => {
                    if let Some(r) = st.recorders.get_mut(&stream) {
                        match r.start() {
                            Ok(()) => say1(&format!("sound: recording {} for the session", r.name)),
                            Err(e) => say1(&format!("sound: {}: {e}", r.name)),
                        }
                    }
                }
                Msg::Stop { stream } => {
                    if let Some(r) = st.recorders.get_mut(&stream) {
                        r.stop();
                        say1(&format!("sound: stopped recording {}", r.name));
                    }
                }
                Msg::Hello { version } => say1(&format!("sound: the session side speaks version {version}")),
                _ => {}
            }
        },
        move |why| {
            e1.store(true, std::sync::atomic::Ordering::SeqCst);
            say2(&format!("sound: the ring ended: {why}"));
        },
    )?;
    link.send(Msg::Hello { version: VERSION });
    let mut said = String::new();
    while !ended.load(std::sync::atomic::Ordering::SeqCst) {
        sync_devices(&state, &link, volume, &say, &mut said);
        std::thread::sleep(Duration::from_secs(1));
    }
    let st = state.lock().unwrap_or_else(|e| e.into_inner());
    for p in st.players.values() {
        let c = *p.out.count.lock().unwrap_or_else(|e| e.into_inner());
        say(&format!("sound: {} played {} bytes, {} underrun buffers", p.name, c.bytes, c.underruns));
    }
    for r in st.recorders.values() {
        let c = *r.inp.count.lock().unwrap_or_else(|e| e.into_inner());
        say(&format!("sound: {} recorded {} bytes, {} overruns", r.name, c.bytes, c.overruns));
    }
    Ok(())
}

/// Announce new devices, retire gone ones, follow the defaults, pause idle outputs.
fn sync_devices(state: &Arc<Mutex<State>>, link: &Link, volume: u8, say: &impl Fn(&str), said: &mut String) {
    let devs = devices();
    let mut st = state.lock().unwrap_or_else(|e| e.into_inner());
    let st = &mut *st;
    let mut alive: Vec<u16> = Vec::new();
    for d in &devs {
        for (dir, has) in [(Dir::Out, d.out), (Dir::In, d.inp)] {
            if !has {
                continue;
            }
            let key = (d.uid.clone(), dir == Dir::Out);
            let id = match st.ids.get(&key) {
                Some(id) => *id,
                None => {
                    st.next += 1;
                    let id = st.next;
                    st.ids.insert(key, id);
                    match dir {
                        Dir::Out => match Player::new(id, d, link, volume) {
                            Ok(p) => {
                                st.players.insert(id, p);
                                link.send(Msg::Device { stream: id, dir, channels: CHANNELS as u8, rate: RATE, name: d.name.clone() });
                                link.send(Msg::Credit { stream: id, bytes: CREDIT_INIT });
                            }
                            Err(e) => say(&format!("sound: {}: {e}", d.name)),
                        },
                        Dir::In => {
                            let inp = Arc::new(In { link: link.clone(), stream: id, credit: Mutex::new(0), count: Mutex::new(Count::default()) });
                            st.recorders.insert(id, Recorder { uid: d.uid.clone(), name: d.name.clone(), inp, q: None });
                            link.send(Msg::Device { stream: id, dir, channels: CHANNELS as u8, rate: RATE, name: d.name.clone() });
                        }
                    }
                    id
                }
            };
            alive.push(id);
        }
    }
    let gone: Vec<u16> = st.ids.values().copied().filter(|id| !alive.contains(id)).collect();
    for id in gone {
        st.ids.retain(|_, v| *v != id);
        st.players.remove(&id);
        if let Some(mut r) = st.recorders.remove(&id) {
            r.stop();
        }
        link.send(Msg::Gone { stream: id });
    }
    let find = |dev: Option<u32>, out: bool| -> Option<u16> {
        let d = devs.iter().find(|d| Some(d.id) == dev)?;
        st.ids.get(&(d.uid.clone(), out)).copied()
    };
    let (dout, din) = (find(prop_u32(SYSTEM, DEFAULT_OUT), true), find(prop_u32(SYSTEM, DEFAULT_IN), false));
    if dout.is_some() && dout != st.default_out {
        st.default_out = dout;
        link.send(Msg::Default { stream: dout.unwrap_or(0), dir: Dir::Out });
    }
    if din.is_some() && din != st.default_in {
        st.default_in = din;
        link.send(Msg::Default { stream: din.unwrap_or(0), dir: Dir::In });
    }
    for p in st.players.values_mut() {
        p.idle();
    }
    let line = format!(
        "sound: {} output(s) [{}], {} input(s) [{}]; default out {:?}, in {:?}",
        st.players.len(),
        st.players.values().map(|p| p.name.as_str()).collect::<Vec<_>>().join(", "),
        st.recorders.len(),
        st.recorders.values().map(|r| r.name.as_str()).collect::<Vec<_>>().join(", "),
        st.default_out,
        st.default_in
    );
    if line != *said {
        say(&line);
        *said = line;
    }
}
