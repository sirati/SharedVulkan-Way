//! The host's audio endpoints, and a watch on them.
//!
//! [`enumerate`] lists every *active* render and capture endpoint
//! (`IMMDeviceEnumerator::EnumAudioEndpoints`, `DEVICE_STATE_ACTIVE`): its id,
//! its friendly name and its shared-mode mix format, plus the two defaults
//! for the console role. [`watch`] registers an `IMMNotificationClient`
//! and sends the `State` with the new state after every burst of changes
//! (debounced, [`crate::debounce`]), and once at the start.
//!
//! Nothing here changes a default or opens a stream: activating an
//! `IAudioClient` to read the mix format starts nothing.

use std::ffi::c_void;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::time::{Duration, Instant};

use windows_sys::Win32::System::Com::{CLSCTX_ALL, CoCreateInstance, CoTaskMemFree};
use windows_sys::Win32::System::Com::StructuredStorage::PropVariantClear;
use windows_sys::core::{GUID, HRESULT};

use wc_hostint::audio::{Device, Dir, State};

use super::com::*;
use crate::debounce;

pub(crate) const E_RENDER: i32 = 0;
pub(crate) const E_CAPTURE: i32 = 1;
const E_CONSOLE: i32 = 0;
const DEVICE_STATE_ACTIVE: u32 = 1;
const STGM_READ: u32 = 0;
const S_OK: HRESULT = 0;
const E_NOINTERFACE: HRESULT = 0x8000_4002_u32 as i32;

pub(crate) type Enumerator = Com<DeviceEnumeratorVtbl>;
pub(crate) type MmDevice = Com<DeviceVtbl>;

pub(crate) fn enumerator() -> Result<Enumerator, String> {
    // SAFETY: the out pointer receives an IMMDeviceEnumerator.
    unsafe {
        out("CoCreateInstance(MMDeviceEnumerator)", |p| {
            CoCreateInstance(&CLSID_MM_DEVICE_ENUMERATOR, std::ptr::null_mut(), CLSCTX_ALL, &IID_MM_DEVICE_ENUMERATOR, p)
        })
    }
}

/// The endpoint with this id.
pub(crate) fn device(e: &Enumerator, id: &str) -> Result<MmDevice, String> {
    let w = wide(id);
    // SAFETY: a live enumerator and a NUL-terminated id.
    unsafe { out("IMMDeviceEnumerator::GetDevice", |p| (e.v().get_device)(e.as_raw(), w.as_ptr(), p)) }
}

fn device_id(d: &MmDevice) -> Result<String, String> {
    let mut p: *mut u16 = std::ptr::null_mut();
    // SAFETY: a live device; the id is CoTaskMemAlloc'ed.
    let hr = unsafe { (d.v().get_id)(d.as_raw(), &mut p) };
    if hr < 0 {
        return Err(format!("IMMDevice::GetId: {}", hr_text(hr)));
    }
    // SAFETY: as GetId documents.
    Ok(unsafe { take_co_string(p) })
}

fn friendly_name(d: &MmDevice) -> Option<String> {
    // SAFETY: a live device; the store is released by Com's Drop.
    let store: Com<PropertyStoreVtbl> =
        unsafe { out("OpenPropertyStore", |p| (d.v().open_property_store)(d.as_raw(), STGM_READ, p)).ok()? };
    let mut pv = PropVariant { vt: 0, reserved: [0; 3], value: [0; 2] };
    // SAFETY: a live store and a PROPVARIANT to fill, cleared below.
    let hr = unsafe { (store.v().get_value)(store.as_raw(), &PKEY_DEVICE_FRIENDLY_NAME, &mut pv) };
    let name = (hr >= 0 && pv.vt == VT_LPWSTR && pv.value[0] != 0).then(|| {
        let p = pv.value[0] as *const u16;
        // SAFETY: VT_LPWSTR is a NUL-terminated string owned by the variant.
        unsafe {
            let mut n = 0;
            while *p.add(n) != 0 {
                n += 1;
            }
            String::from_utf16_lossy(std::slice::from_raw_parts(p, n))
        }
    });
    // SAFETY: frees what GetValue allocated; same layout as PROPVARIANT.
    unsafe { PropVariantClear((&mut pv as *mut PropVariant).cast()) };
    name
}

/// `WAVEFORMATEX`'s first fields: channels and rate.
pub(crate) fn mix_format(d: &MmDevice) -> Option<(u16, u32)> {
    // SAFETY: activating an IAudioClient starts nothing; it is released.
    let client: Com<AudioClientVtbl> = unsafe {
        out("Activate(IAudioClient)", |p| (d.v().activate)(d.as_raw(), &IID_AUDIO_CLIENT, CLSCTX_ALL, std::ptr::null(), p))
            .ok()?
    };
    let mut f: *mut c_void = std::ptr::null_mut();
    // SAFETY: GetMixFormat CoTaskMemAllocs a WAVEFORMATEX(EXTENSIBLE).
    let hr = unsafe { (client.v().get_mix_format)(client.as_raw(), &mut f) };
    if hr < 0 || f.is_null() {
        return None;
    }
    // SAFETY: WAVEFORMATEX: wFormatTag u16, nChannels u16, nSamplesPerSec u32.
    let r = unsafe {
        let b = f as *const u8;
        (std::ptr::read_unaligned(b.add(2) as *const u16), std::ptr::read_unaligned(b.add(4) as *const u32))
    };
    // SAFETY: allocated by GetMixFormat.
    unsafe { CoTaskMemFree(f) };
    Some(r)
}

fn default_id(e: &Enumerator, flow: i32) -> Option<String> {
    let d: MmDevice = unsafe {
        out("GetDefaultAudioEndpoint", |p| (e.v().get_default_audio_endpoint)(e.as_raw(), flow, E_CONSOLE, p)).ok()?
    };
    device_id(&d).ok()
}

/// The host's audio state now. Must be called on a thread in a COM
/// apartment (see [`Apartment`]).
pub fn enumerate_with(e: &Enumerator) -> Result<State, String> {
    let mut st = State::default();
    for (flow, dir) in [(E_RENDER, Dir::Output), (E_CAPTURE, Dir::Input)] {
        // SAFETY: a live enumerator; the collection is released on drop.
        let coll: Com<DeviceCollectionVtbl> = unsafe {
            out("EnumAudioEndpoints", |p| (e.v().enum_audio_endpoints)(e.as_raw(), flow, DEVICE_STATE_ACTIVE, p))?
        };
        let mut n = 0u32;
        // SAFETY: a live collection.
        unsafe { (coll.v().get_count)(coll.as_raw(), &mut n) };
        for i in 0..n {
            // SAFETY: i < count.
            let Ok(d) = (unsafe { out::<DeviceVtbl>("IMMDeviceCollection::Item", |p| (coll.v().item)(coll.as_raw(), i, p)) })
            else {
                continue;
            };
            let Ok(id) = device_id(&d) else { continue };
            let description = friendly_name(&d).unwrap_or_else(|| id.clone());
            let (channels, rate) = mix_format(&d).unwrap_or((0, 0));
            st.devices.push(Device { dir, id, description, channels, rate });
        }
    }
    st.default_output = default_id(e, E_RENDER);
    st.default_input = default_id(e, E_CAPTURE);
    Ok(st)
}

/// [`enumerate_with`] on a fresh enumerator, in a COM apartment of its own.
pub fn enumerate() -> Result<State, String> {
    let _com = Apartment::mta();
    enumerate_with(&enumerator()?)
}

// ------------------------------------------------------------- the watch --

/// Our `IMMNotificationClient`: a vtable pointer, a reference count and
/// the channel a change is announced on. It does nothing else on the
/// caller's thread: the documentation forbids blocking in these callbacks
/// and calling back into the enumerator from them.
#[repr(C)]
struct Client {
    vtbl: *const NotificationClientVtbl,
    refs: AtomicU32,
    tx: std::sync::Mutex<Sender<Ping>>,
}

enum Ping {
    Changed,
    Stop,
}

static CLIENT_VTBL: NotificationClientVtbl = NotificationClientVtbl {
    base: UnknownVtbl { query_interface: c_query_interface, add_ref: c_add_ref, release: c_release },
    on_device_state_changed: c_state_changed,
    on_device_added: c_added,
    on_device_removed: c_removed,
    on_default_device_changed: c_default_changed,
    on_property_value_changed: c_property_changed,
};

unsafe extern "system" fn c_query_interface(this: *mut c_void, iid: *const GUID, out: *mut *mut c_void) -> HRESULT {
    // SAFETY: COM's contract for QueryInterface.
    unsafe {
        let iid = &*iid;
        let is = |g: &GUID| g.data1 == iid.data1 && g.data2 == iid.data2 && g.data3 == iid.data3 && g.data4 == iid.data4;
        if is(&IID_UNKNOWN) || is(&IID_MM_NOTIFICATION_CLIENT) {
            c_add_ref(this);
            *out = this;
            S_OK
        } else {
            *out = std::ptr::null_mut();
            E_NOINTERFACE
        }
    }
}

unsafe extern "system" fn c_add_ref(this: *mut c_void) -> u32 {
    // SAFETY: `this` is a live Client.
    unsafe { (*(this as *const Client)).refs.fetch_add(1, Ordering::AcqRel) + 1 }
}

unsafe extern "system" fn c_release(this: *mut c_void) -> u32 {
    // SAFETY: `this` is a live Client, made by Box::into_raw in `watch`.
    unsafe {
        let n = (*(this as *const Client)).refs.fetch_sub(1, Ordering::AcqRel) - 1;
        if n == 0 {
            drop(Box::from_raw(this as *mut Client));
        }
        n
    }
}

fn ping(this: *mut c_void) -> HRESULT {
    // SAFETY: `this` is a live Client.
    let c = unsafe { &*(this as *const Client) };
    if let Ok(tx) = c.tx.lock() {
        let _ = tx.send(Ping::Changed);
    }
    S_OK
}

unsafe extern "system" fn c_state_changed(this: *mut c_void, _id: *const u16, _s: u32) -> HRESULT {
    ping(this)
}
unsafe extern "system" fn c_added(this: *mut c_void, _id: *const u16) -> HRESULT {
    ping(this)
}
unsafe extern "system" fn c_removed(this: *mut c_void, _id: *const u16) -> HRESULT {
    ping(this)
}
unsafe extern "system" fn c_default_changed(this: *mut c_void, _flow: i32, role: i32, _id: *const u16) -> HRESULT {
    // Only the console role is followed; the other roles' calls for the
    // same change are redundant.
    if role == E_CONSOLE { ping(this) } else { S_OK }
}
unsafe extern "system" fn c_property_changed(this: *mut c_void, _id: *const u16, key: PropertyKey) -> HRESULT {
    // Properties change all the time (jack info, volumes' metadata); only a
    // new name or a new format changes what Linux is told.
    let same = |k: &PropertyKey| {
        k.pid == key.pid
            && k.fmtid.data1 == key.fmtid.data1
            && k.fmtid.data2 == key.fmtid.data2
            && k.fmtid.data3 == key.fmtid.data3
            && k.fmtid.data4 == key.fmtid.data4
    };
    if same(&PKEY_DEVICE_FRIENDLY_NAME) || same(&PKEY_AUDIO_ENGINE_DEVICE_FORMAT) { ping(this) } else { S_OK }
}

/// A running watch. Dropping it unregisters and ends the thread.
pub struct Watch {
    stop: Sender<Ping>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Drop for Watch {
    fn drop(&mut self) {
        let _ = self.stop.send(Ping::Stop);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// Send the host's audio state on `events` now and after every change,
/// within a second of the change. `log` gets a line on failures. Ends when
/// the [`Watch`] is dropped or `events`' receiver is gone.
pub fn watch(events: Sender<State>, log: impl FnMut(&str) + Send + 'static) -> Result<Watch, String> {
    watch_states(move |s| events.send(s).is_ok(), log)
}

/// [`watch`] with a callback: `on_state` gets the state now and after every
/// burst of changes (only when it differs from the last), on the watch's own
/// thread; returning `false` ends the watch. The sound ring's device side
/// (wc-compositor `sound_win`) uses this.
pub fn watch_states(
    mut on_state: impl FnMut(State) -> bool + Send + 'static,
    mut log: impl FnMut(&str) + Send + 'static,
) -> Result<Watch, String> {
    let (tx, rx) = mpsc::channel::<Ping>();
    let (ready_tx, ready_rx) = mpsc::channel::<Result<(), String>>();
    let client_tx = tx.clone();
    let thread = std::thread::Builder::new()
        .name("hostint-audio".into())
        .spawn(move || {
            let _com = Apartment::mta();
            let e = match enumerator() {
                Ok(e) => e,
                Err(err) => {
                    let _ = ready_tx.send(Err(err));
                    return;
                }
            };
            let client = Box::into_raw(Box::new(Client {
                vtbl: &CLIENT_VTBL,
                refs: AtomicU32::new(1),
                tx: std::sync::Mutex::new(client_tx),
            }));
            // SAFETY: a live enumerator and a live client; the enumerator
            // takes its own reference.
            let hr = unsafe { (e.v().register_endpoint_notification_callback)(e.as_raw(), client.cast()) };
            if hr < 0 {
                // SAFETY: our own reference.
                unsafe { c_release(client.cast()) };
                let _ = ready_tx.send(Err(format!("RegisterEndpointNotificationCallback: {}", hr_text(hr))));
                return;
            }
            let _ = ready_tx.send(Ok(()));
            run(&e, &rx, &mut on_state, &mut log);
            // SAFETY: unregister before the last reference goes.
            unsafe {
                (e.v().unregister_endpoint_notification_callback)(e.as_raw(), client.cast());
                c_release(client.cast());
            }
        })
        .map_err(|e| format!("spawn: {e}"))?;
    let w = Watch { stop: tx, thread: Some(thread) };
    match ready_rx.recv() {
        Ok(Ok(())) => Ok(w),
        Ok(Err(e)) => Err(e),
        Err(_) => Err("the audio watch thread ended".into()),
    }
}

fn run(e: &Enumerator, rx: &Receiver<Ping>, on_state: &mut dyn FnMut(State) -> bool, log: &mut dyn FnMut(&str)) {
    let mut last: Option<State> = None;
    let mut send = |last: &mut Option<State>| -> bool {
        match enumerate_with(e) {
            Ok(s) => {
                if last.as_ref() != Some(&s) {
                    *last = Some(s.clone());
                    return on_state(s);
                }
                true
            }
            Err(err) => {
                log(&format!("hostint audio: {err}"));
                true
            }
        }
    };
    if !send(&mut last) {
        return;
    }
    loop {
        match rx.recv() {
            Ok(Ping::Changed) => {}
            Ok(Ping::Stop) | Err(_) => return,
        }
        let first = Instant::now();
        let mut latest = first;
        while let Some(left) = debounce::wait_left(first, latest, Instant::now()) {
            match rx.recv_timeout(left) {
                Ok(Ping::Changed) => latest = Instant::now(),
                Ok(Ping::Stop) | Err(RecvTimeoutError::Disconnected) => return,
                Err(RecvTimeoutError::Timeout) => {}
            }
        }
        if !send(&mut last) {
            return;
        }
    }
}

/// How long [`watch`] may take from a change to its event.
pub const LATENCY: Duration = debounce::MAX;
