//! The host's audio devices from CoreAudio, and their changes.
//!
//! [`state`] enumerates every device (its UID is the id Linux names it by,
//! `kAudioObjectPropertyName` its description, the channels its stream
//! configuration has in each direction, its nominal rate) and the two
//! defaults. A device with output streams is an output, one with input
//! streams an input, and one with both (a USB headset) is both.
//!
//! [`watch`] listens to the device list and both defaults and sends the new
//! state as the `State` once a burst of changes is over, within a second
//! of its start (`crate::debounce`). The listeners run on CoreAudio's own
//! notification thread, not the main run loop (`kAudioHardwarePropertyRunLoop`
//! is set to none), so this works in a process that never runs one.

use std::ffi::c_void;
use std::sync::mpsc::{channel, RecvTimeoutError, Sender};
use std::time::Instant;

use wc_hostint::audio::{Device, Dir, State};

use super::cf;
use super::plan::channels_of_buffer_list;

type AudioObjectID = u32;
type OSStatus = i32;

#[repr(C)]
#[derive(Clone, Copy)]
struct Address {
    selector: u32,
    scope: u32,
    element: u32,
}

type Listener = extern "C" fn(AudioObjectID, u32, *const Address, *mut c_void) -> OSStatus;

#[link(name = "CoreAudio", kind = "framework")]
unsafe extern "C" {
    fn AudioObjectGetPropertyDataSize(o: AudioObjectID, a: *const Address, qs: u32, q: *const c_void, size: *mut u32) -> OSStatus;
    fn AudioObjectGetPropertyData(
        o: AudioObjectID,
        a: *const Address,
        qs: u32,
        q: *const c_void,
        size: *mut u32,
        data: *mut c_void,
    ) -> OSStatus;
    fn AudioObjectSetPropertyData(o: AudioObjectID, a: *const Address, qs: u32, q: *const c_void, size: u32, data: *const c_void) -> OSStatus;
    fn AudioObjectAddPropertyListener(o: AudioObjectID, a: *const Address, l: Listener, client: *mut c_void) -> OSStatus;
    fn AudioObjectRemovePropertyListener(o: AudioObjectID, a: *const Address, l: Listener, client: *mut c_void) -> OSStatus;
}

const fn fourcc(s: &[u8; 4]) -> u32 {
    u32::from_be_bytes(*s)
}

const SYSTEM: AudioObjectID = 1;
const DEVICES: u32 = fourcc(b"dev#");
const DEFAULT_OUTPUT: u32 = fourcc(b"dOut");
const DEFAULT_INPUT: u32 = fourcc(b"dIn ");
const RUN_LOOP: u32 = fourcc(b"rnlp");
const UID: u32 = fourcc(b"uid ");
const NAME: u32 = fourcc(b"lnam");
const STREAM_CONFIGURATION: u32 = fourcc(b"slay");
const NOMINAL_RATE: u32 = fourcc(b"nsrt");
const IS_HIDDEN: u32 = fourcc(b"hidn");
const GLOBAL: u32 = fourcc(b"glob");
const INPUT: u32 = fourcc(b"inpt");
const OUTPUT: u32 = fourcc(b"outp");
const MAIN: u32 = 0;

fn addr(selector: u32, scope: u32) -> Address {
    Address { selector, scope, element: MAIN }
}

/// A fixed-size property.
fn get<T: Copy + Default>(o: AudioObjectID, a: Address) -> Option<T> {
    let mut v = T::default();
    let mut size = std::mem::size_of::<T>() as u32;
    // SAFETY: `v` is a `T` of the size passed.
    let st = unsafe { AudioObjectGetPropertyData(o, &a, 0, std::ptr::null(), &mut size, (&raw mut v).cast()) };
    (st == 0 && size as usize == std::mem::size_of::<T>()).then_some(v)
}

/// A variable-size property, as bytes.
fn get_bytes(o: AudioObjectID, a: Address) -> Option<Vec<u8>> {
    let mut size = 0u32;
    // SAFETY: an out-pointer for the size.
    if unsafe { AudioObjectGetPropertyDataSize(o, &a, 0, std::ptr::null(), &mut size) } != 0 {
        return None;
    }
    // u64s, so the buffer is 8-aligned for an AudioBufferList.
    let mut buf = vec![0u64; (size as usize).div_ceil(8)];
    // SAFETY: a buffer of at least `size` bytes.
    let st = unsafe { AudioObjectGetPropertyData(o, &a, 0, std::ptr::null(), &mut size, buf.as_mut_ptr().cast()) };
    if st != 0 {
        return None;
    }
    // SAFETY: the u64 buffer's bytes, of which `size` were written.
    let bytes = unsafe { std::slice::from_raw_parts(buf.as_ptr().cast::<u8>(), size as usize) };
    Some(bytes.to_vec())
}

/// A CFString property (a +1 reference, released here).
fn get_string(o: AudioObjectID, a: Address) -> Option<String> {
    let s: usize = get(o, a)?;
    let owned = cf::Owned::new(s as cf::CFStringRef)?;
    cf::to_string(owned.get())
}

fn device_ids() -> Vec<AudioObjectID> {
    let Some(b) = get_bytes(SYSTEM, addr(DEVICES, GLOBAL)) else { return Vec::new() };
    b.chunks_exact(4).map(|c| u32::from_ne_bytes(c.try_into().unwrap())).collect()
}

/// The UID of a default device, by its selector.
fn default_uid(selector: u32) -> Option<String> {
    let id: AudioObjectID = get(SYSTEM, addr(selector, GLOBAL))?;
    if id == 0 {
        return None;
    }
    get_string(id, addr(UID, GLOBAL))
}

/// The host's devices and defaults, now.
pub fn state() -> State {
    let mut devices = Vec::new();
    for id in device_ids() {
        if get::<u32>(id, addr(IS_HIDDEN, GLOBAL)).unwrap_or(0) != 0 {
            continue;
        }
        let Some(uid) = get_string(id, addr(UID, GLOBAL)) else { continue };
        let name = get_string(id, addr(NAME, GLOBAL)).unwrap_or_else(|| uid.clone());
        let rate = get::<f64>(id, addr(NOMINAL_RATE, GLOBAL)).unwrap_or(0.0);
        for (dir, scope) in [(Dir::Output, OUTPUT), (Dir::Input, INPUT)] {
            let ch = get_bytes(id, addr(STREAM_CONFIGURATION, scope)).map_or(0, |b| channels_of_buffer_list(&b));
            if ch > 0 {
                devices.push(Device {
                    dir,
                    id: uid.clone(),
                    description: wc_hostint::wire::field(&name),
                    channels: ch.min(u32::from(u16::MAX)) as u16,
                    rate: if rate.is_finite() && rate > 0.0 { rate.round() as u32 } else { 0 },
                });
            }
        }
    }
    State { devices, default_output: default_uid(DEFAULT_OUTPUT), default_input: default_uid(DEFAULT_INPUT) }
}

/// What the listener is given: where to say "something changed".
struct Client(Sender<()>);

extern "C" fn changed(_: AudioObjectID, _: u32, _: *const Address, client: *mut c_void) -> OSStatus {
    // SAFETY: `client` is the `Client` boxed in `watch`, alive until the
    // listeners are removed in `Watch::drop`.
    let c = unsafe { &*(client as *const Client) };
    let _ = c.0.send(());
    0
}

const WATCHED: [u32; 3] = [DEVICES, DEFAULT_OUTPUT, DEFAULT_INPUT];

/// The listeners, removed on drop. The thread that sends the states ends
/// when they are gone or when nobody receives the events any more.
pub struct Watch {
    client: *mut Client,
}

// SAFETY: the pointer is only used to unregister and free, once, in drop.
unsafe impl Send for Watch {}

impl Drop for Watch {
    fn drop(&mut self) {
        // SAFETY: the same addresses, function and client that were added;
        // after removal CoreAudio calls the listener no more, so the box can
        // go.
        unsafe {
            for s in WATCHED {
                AudioObjectRemovePropertyListener(SYSTEM, &addr(s, GLOBAL), changed, self.client.cast());
            }
            drop(Box::from_raw(self.client));
        }
    }
}

/// Send the current state now, and again after every burst of device or
/// default changes, as the `State` on `events`.
pub fn watch(events: Sender<State>) -> Result<Watch, String> {
    // Notifications on CoreAudio's own thread rather than the main run loop.
    let none: *const c_void = std::ptr::null();
    // SAFETY: the property is a CFRunLoopRef; null means "the HAL's own".
    unsafe {
        AudioObjectSetPropertyData(SYSTEM, &addr(RUN_LOOP, GLOBAL), 0, std::ptr::null(), std::mem::size_of_val(&none) as u32, (&raw const none).cast());
    }
    let (tx, rx) = channel::<()>();
    let client = Box::into_raw(Box::new(Client(tx)));
    for s in WATCHED {
        // SAFETY: a static listener and a client that lives until `Watch`
        // removes the listeners.
        let st = unsafe { AudioObjectAddPropertyListener(SYSTEM, &addr(s, GLOBAL), changed, client.cast()) };
        if st != 0 {
            // SAFETY: as in drop: remove what was added, then free.
            drop(Watch { client });
            return Err(format!("AudioObjectAddPropertyListener: OSStatus {st}"));
        }
    }
    std::thread::Builder::new()
        .name("hostint-audio".into())
        .spawn(move || {
            if events.send(state()).is_err() {
                return;
            }
            // The first change of a burst; wait for the burst to end.
            while rx.recv().is_ok() {
                let first = Instant::now();
                let mut last = first;
                loop {
                    let Some(left) = crate::debounce::wait_left(first, last, Instant::now()) else { break };
                    match rx.recv_timeout(left) {
                        Ok(()) => last = Instant::now(),
                        Err(RecvTimeoutError::Timeout) => {}
                        Err(RecvTimeoutError::Disconnected) => return,
                    }
                }
                if events.send(state()).is_err() {
                    return;
                }
            }
        })
        .map_err(|e| e.to_string())?;
    Ok(Watch { client })
}
