//! §103.8: the session's sound on a Mac, the macOS twin of `audio.rs`'s
//! waveOut player (§86). `wc-session` gives each session a PulseAudio null sink
//! and serves its monitor as raw s16le 48 kHz stereo on `$D/audio.sock`; the
//! launcher's tunnel forwards a loopback port here to it, and this plays what
//! arrives through an AudioQueue on the default output device.
//!
//! The same shape as on Windows: a reader thread fills a bounded queue
//! (160 ms, the oldest chunk dropped when it is full, so the picture never
//! waits for the sound), and AudioQueue's callback takes 20 ms chunks from it,
//! playing silence when there are none (an underrun, counted).

use std::collections::VecDeque;
use std::ffi::c_void;
use std::io::Read;
use std::sync::{Arc, Mutex};

use crate::audio::{audible, Stats, CHANNELS, CHUNK_BYTES, RATE};

#[repr(C)]
struct AudioStreamBasicDescription {
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
struct AudioQueueBuffer {
    capacity: u32,
    data: *mut c_void,
    size: u32,
    user: *mut c_void,
    packet_desc_capacity: u32,
    packet_descs: *mut c_void,
    packet_desc_count: u32,
}

type AudioQueueRef = *mut c_void;
type Callback = extern "C" fn(*mut c_void, AudioQueueRef, *mut AudioQueueBuffer);

#[link(name = "AudioToolbox", kind = "framework")]
unsafe extern "C" {
    fn AudioQueueNewOutput(
        format: *const AudioStreamBasicDescription,
        callback: Callback,
        user: *mut c_void,
        run_loop: *const c_void,
        run_loop_mode: *const c_void,
        flags: u32,
        out: *mut AudioQueueRef,
    ) -> i32;
    fn AudioQueueAllocateBuffer(q: AudioQueueRef, size: u32, out: *mut *mut AudioQueueBuffer) -> i32;
    fn AudioQueueEnqueueBuffer(q: AudioQueueRef, buf: *mut AudioQueueBuffer, n: u32, descs: *const c_void) -> i32;
    fn AudioQueueStart(q: AudioQueueRef, at: *const c_void) -> i32;
    fn AudioQueueSetParameter(q: AudioQueueRef, param: u32, value: f32) -> i32;
}

/// `kAudioFormatLinearPCM`.
const LPCM: u32 = u32::from_be_bytes(*b"lpcm");
/// `kAudioFormatFlagIsSignedInteger | kAudioFormatFlagIsPacked`, little endian.
const S16_PACKED: u32 = 0x4 | 0x8;
/// `kAudioQueueParam_Volume`.
const PARAM_VOLUME: u32 = 1;
/// AudioQueue buffers in flight: 6 x 20 ms. Three left too little slack for
/// PCM that arrives in bursts over ssh, and every underrun is a gap (measured:
/// a 440 Hz tone came out with half its zero crossings).
const AQ_BUFFERS: usize = 6;
/// The most that may be queued, in 20 ms chunks: 600 ms. The session's null
/// sink delivers its monitor in bursts of about 300 ms (measured from the VM's
/// output: 170 ms of tone, 130 ms of silence, repeating, with `audio.rs`'s
/// 160 ms cap), so the queue must hold a whole burst or every burst is cut.
const MAX_QUEUE: usize = 30;
/// Chunks queued before playback starts: one burst.
const PREBUFFER: usize = 15;

struct Shared {
    queue: Mutex<VecDeque<Vec<u8>>>,
    stats: Mutex<Stats>,
}

extern "C" fn refill(user: *mut c_void, q: AudioQueueRef, buf: *mut AudioQueueBuffer) {
    // SAFETY: `user` is the `Arc<Shared>` leaked in `play` for the queue's
    // life (the process's), and `buf` is a buffer of this queue, valid until it
    // is enqueued again below.
    unsafe {
        let shared = &*(user as *const Shared);
        let b = &mut *buf;
        let out = std::slice::from_raw_parts_mut(b.data.cast::<u8>(), b.capacity as usize);
        let chunk = shared.queue.lock().ok().and_then(|mut q| q.pop_front());
        let mut st = shared.stats.lock().unwrap_or_else(|e| e.into_inner());
        match chunk {
            Some(c) => {
                let n = c.len().min(out.len());
                out[..n].copy_from_slice(&c[..n]);
                out[n..].fill(0);
                st.chunks_played += 1;
                if audible(&c) {
                    st.chunks_audible += 1;
                }
            }
            None => {
                out.fill(0);
                st.underruns += 1;
            }
        }
        b.size = b.capacity;
        AudioQueueEnqueueBuffer(q, buf, 0, std::ptr::null());
    }
}

/// Play what arrives on `stream` until it ends. Returns the session's numbers.
pub fn play(mut stream: impl Read, volume_pct: u8) -> Result<Stats, String> {
    let shared = Arc::new(Shared { queue: Mutex::new(VecDeque::new()), stats: Mutex::new(Stats::default()) });
    let fmt = AudioStreamBasicDescription {
        sample_rate: f64::from(RATE),
        format_id: LPCM,
        format_flags: S16_PACKED,
        bytes_per_packet: 4,
        frames_per_packet: 1,
        bytes_per_frame: 4,
        channels_per_frame: u32::from(CHANNELS),
        bits_per_channel: 16,
        reserved: 0,
    };
    let user = Arc::into_raw(shared.clone()) as *mut c_void;
    let mut q: AudioQueueRef = std::ptr::null_mut();
    // SAFETY: a valid format, a callback of the right type, and `user` kept
    // alive (leaked) for as long as the queue runs; no run loop, so the
    // callbacks come on AudioQueue's own thread.
    let st = unsafe { AudioQueueNewOutput(&fmt, refill, user, std::ptr::null(), std::ptr::null(), 0, &mut q) };
    if st != 0 {
        return Err(format!("AudioQueueNewOutput failed: OSStatus {st} (no output device?)"));
    }
    let mut chunk = vec![0u8; CHUNK_BYTES];
    let mut started = false;
    loop {
        if !started && shared.queue.lock().map(|q| q.len()).unwrap_or(0) >= PREBUFFER {
            // SAFETY: `q` is the queue made above; every buffer is allocated
            // by it and filled by the callback before the queue starts.
            unsafe {
                AudioQueueSetParameter(q, PARAM_VOLUME, f32::from(volume_pct.min(100)) / 100.0);
                for _ in 0..AQ_BUFFERS {
                    let mut b: *mut AudioQueueBuffer = std::ptr::null_mut();
                    if AudioQueueAllocateBuffer(q, CHUNK_BYTES as u32, &mut b) == 0 {
                        refill(user, q, b);
                    }
                }
                let s = AudioQueueStart(q, std::ptr::null());
                if s != 0 {
                    return Err(format!("AudioQueueStart failed: OSStatus {s}"));
                }
            }
            started = true;
        }
        if stream.read_exact(&mut chunk).is_err() {
            break;
        }
        let mut q = shared.queue.lock().unwrap_or_else(|e| e.into_inner());
        if q.len() >= MAX_QUEUE {
            q.pop_front();
            shared.stats.lock().unwrap_or_else(|e| e.into_inner()).chunks_dropped += 1;
        }
        q.push_back(chunk.clone());
    }
    let s = *shared.stats.lock().unwrap_or_else(|e| e.into_inner());
    Ok(s)
}
