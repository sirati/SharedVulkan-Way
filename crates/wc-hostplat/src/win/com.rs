//! Hand-written COM, as in `wc-launch/src/shell.rs` and the compositor:
//! `windows-sys` has no COM interfaces, so each one used here is declared
//! from its header, every method in the header's order (a slot in the wrong
//! place is a call through the wrong pointer, which is why unused methods
//! are listed too). Headers: mmdeviceapi.h, audioclient.h, propsys.h,
//! shobjidl_core.h.

#![allow(dead_code)]

use std::ffi::c_void;

use windows_sys::Win32::System::Com::{
    COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE, COINIT_MULTITHREADED, CoInitializeEx, CoTaskMemFree,
    CoUninitialize,
};
use windows_sys::core::{GUID, HRESULT};

pub const CLSID_MM_DEVICE_ENUMERATOR: GUID = GUID::from_u128(0xbcde0395_e52f_467c_8e3d_c4579291692e);
pub const IID_UNKNOWN: GUID = GUID::from_u128(0x00000000_0000_0000_c000_000000000046);
pub const IID_MM_DEVICE_ENUMERATOR: GUID = GUID::from_u128(0xa95664d2_9614_4f35_a746_de8db63617e6);
pub const IID_MM_NOTIFICATION_CLIENT: GUID = GUID::from_u128(0x7991eec9_7e89_4d85_8390_6c703cec60c0);
pub const IID_AUDIO_CLIENT: GUID = GUID::from_u128(0x1cb9ad4c_dbfa_4c32_b178_c2f568a703b2);
pub const IID_AUDIO_RENDER_CLIENT: GUID = GUID::from_u128(0xf294acfc_3146_4483_a7bf_addca7c260e2);
pub const IID_AUDIO_CAPTURE_CLIENT: GUID = GUID::from_u128(0xc8adbd64_e71e_48a0_a4de_185c395cd317);
pub const IID_ENUM_ASSOC_HANDLERS: GUID = GUID::from_u128(0x973810ae_9599_4b88_9e4d_6ee98c9552da);

/// `PROPERTYKEY`.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct PropertyKey {
    pub fmtid: GUID,
    pub pid: u32,
}

/// `PKEY_Device_FriendlyName`: "Speakers (Realtek(R) Audio)".
pub const PKEY_DEVICE_FRIENDLY_NAME: PropertyKey =
    PropertyKey { fmtid: GUID::from_u128(0xa45c254e_df1c_4efd_8020_67d146a850e0), pid: 14 };
/// `PKEY_AudioEngine_DeviceFormat`: the endpoint's format changed.
pub const PKEY_AUDIO_ENGINE_DEVICE_FORMAT: PropertyKey =
    PropertyKey { fmtid: GUID::from_u128(0xf19f064d_082c_4e27_bc73_6882a1bb8e4c), pid: 0 };

/// `PROPVARIANT`, 24 bytes on x64. Only `VT_LPWSTR` is read.
#[repr(C)]
pub struct PropVariant {
    pub vt: u16,
    pub reserved: [u16; 3],
    pub value: [u64; 2],
}
const _: () = assert!(std::mem::size_of::<PropVariant>() == 24);
pub const VT_LPWSTR: u16 = 31;

pub type Fp = unsafe extern "system" fn();
type This = *mut c_void;

#[repr(C)]
pub struct UnknownVtbl {
    pub query_interface: unsafe extern "system" fn(This, *const GUID, *mut *mut c_void) -> HRESULT,
    pub add_ref: unsafe extern "system" fn(This) -> u32,
    pub release: unsafe extern "system" fn(This) -> u32,
}

/// `IMMDeviceEnumerator`.
#[repr(C)]
pub struct DeviceEnumeratorVtbl {
    pub base: UnknownVtbl,
    pub enum_audio_endpoints: unsafe extern "system" fn(This, i32, u32, *mut *mut c_void) -> HRESULT,
    pub get_default_audio_endpoint: unsafe extern "system" fn(This, i32, i32, *mut *mut c_void) -> HRESULT,
    pub get_device: unsafe extern "system" fn(This, *const u16, *mut *mut c_void) -> HRESULT,
    pub register_endpoint_notification_callback: unsafe extern "system" fn(This, *mut c_void) -> HRESULT,
    pub unregister_endpoint_notification_callback: unsafe extern "system" fn(This, *mut c_void) -> HRESULT,
}

/// `IMMDeviceCollection`.
#[repr(C)]
pub struct DeviceCollectionVtbl {
    pub base: UnknownVtbl,
    pub get_count: unsafe extern "system" fn(This, *mut u32) -> HRESULT,
    pub item: unsafe extern "system" fn(This, u32, *mut *mut c_void) -> HRESULT,
}

/// `IMMDevice`.
#[repr(C)]
pub struct DeviceVtbl {
    pub base: UnknownVtbl,
    pub activate: unsafe extern "system" fn(This, *const GUID, u32, *const c_void, *mut *mut c_void) -> HRESULT,
    pub open_property_store: unsafe extern "system" fn(This, u32, *mut *mut c_void) -> HRESULT,
    pub get_id: unsafe extern "system" fn(This, *mut *mut u16) -> HRESULT,
    pub get_state: unsafe extern "system" fn(This, *mut u32) -> HRESULT,
}

/// `IPropertyStore`.
#[repr(C)]
pub struct PropertyStoreVtbl {
    pub base: UnknownVtbl,
    pub get_count: Fp,
    pub get_at: Fp,
    pub get_value: unsafe extern "system" fn(This, *const PropertyKey, *mut PropVariant) -> HRESULT,
    pub set_value: Fp,
    pub commit: Fp,
}

/// `IMMNotificationClient`: implemented here, see `audio.rs`.
#[repr(C)]
pub struct NotificationClientVtbl {
    pub base: UnknownVtbl,
    pub on_device_state_changed: unsafe extern "system" fn(This, *const u16, u32) -> HRESULT,
    pub on_device_added: unsafe extern "system" fn(This, *const u16) -> HRESULT,
    pub on_device_removed: unsafe extern "system" fn(This, *const u16) -> HRESULT,
    pub on_default_device_changed: unsafe extern "system" fn(This, i32, i32, *const u16) -> HRESULT,
    pub on_property_value_changed: unsafe extern "system" fn(This, *const u16, PropertyKey) -> HRESULT,
}

/// `IAudioClient`.
#[repr(C)]
pub struct AudioClientVtbl {
    pub base: UnknownVtbl,
    pub initialize: unsafe extern "system" fn(This, i32, u32, i64, i64, *const c_void, *const GUID) -> HRESULT,
    pub get_buffer_size: unsafe extern "system" fn(This, *mut u32) -> HRESULT,
    pub get_stream_latency: unsafe extern "system" fn(This, *mut i64) -> HRESULT,
    pub get_current_padding: unsafe extern "system" fn(This, *mut u32) -> HRESULT,
    pub is_format_supported: Fp,
    pub get_mix_format: unsafe extern "system" fn(This, *mut *mut c_void) -> HRESULT,
    pub get_device_period: unsafe extern "system" fn(This, *mut i64, *mut i64) -> HRESULT,
    pub start: unsafe extern "system" fn(This) -> HRESULT,
    pub stop: unsafe extern "system" fn(This) -> HRESULT,
    pub reset: unsafe extern "system" fn(This) -> HRESULT,
    pub set_event_handle: unsafe extern "system" fn(This, *mut c_void) -> HRESULT,
    pub get_service: unsafe extern "system" fn(This, *const GUID, *mut *mut c_void) -> HRESULT,
}

/// `IAudioRenderClient`.
#[repr(C)]
pub struct AudioRenderClientVtbl {
    pub base: UnknownVtbl,
    pub get_buffer: unsafe extern "system" fn(This, u32, *mut *mut u8) -> HRESULT,
    pub release_buffer: unsafe extern "system" fn(This, u32, u32) -> HRESULT,
}

/// `IAudioCaptureClient`.
#[repr(C)]
pub struct AudioCaptureClientVtbl {
    pub base: UnknownVtbl,
    pub get_buffer: unsafe extern "system" fn(This, *mut *mut u8, *mut u32, *mut u32, *mut u64, *mut u64) -> HRESULT,
    pub release_buffer: unsafe extern "system" fn(This, u32) -> HRESULT,
    pub get_next_packet_size: unsafe extern "system" fn(This, *mut u32) -> HRESULT,
}

/// `IEnumAssocHandlers`.
#[repr(C)]
pub struct EnumAssocHandlersVtbl {
    pub base: UnknownVtbl,
    pub next: unsafe extern "system" fn(This, u32, *mut *mut c_void, *mut u32) -> HRESULT,
}

/// `IAssocHandler`.
#[repr(C)]
pub struct AssocHandlerVtbl {
    pub base: UnknownVtbl,
    pub get_name: unsafe extern "system" fn(This, *mut *mut u16) -> HRESULT,
    pub get_ui_name: unsafe extern "system" fn(This, *mut *mut u16) -> HRESULT,
    pub get_icon_location: Fp,
    pub is_recommended: unsafe extern "system" fn(This) -> HRESULT,
    pub make_default: Fp,
    pub invoke: Fp,
    pub create_invoker: Fp,
}

/// An owned interface pointer of vtable type `V`, released on drop.
pub struct Com<V> {
    ptr: *mut c_void,
    _v: std::marker::PhantomData<V>,
}

// The pointers held here are free-threaded (MTA objects, or used on the
// thread that made them); moving the owner between threads is what the
// stream and watcher threads do with them before first use.
unsafe impl<V> Send for Com<V> {}

impl<V> Com<V> {
    /// Take ownership of a pointer an API returned with one reference.
    ///
    /// # Safety
    /// `ptr` must be null or a live interface pointer whose vtable is `V`.
    pub unsafe fn from_raw(ptr: *mut c_void) -> Option<Self> {
        (!ptr.is_null()).then_some(Com { ptr, _v: std::marker::PhantomData })
    }
    pub fn as_raw(&self) -> *mut c_void {
        self.ptr
    }
    /// The vtable.
    pub fn v(&self) -> &V {
        // SAFETY: a COM object starts with its vtable pointer, and `V` is
        // that vtable's layout by the constructor's contract.
        unsafe { &**(self.ptr as *const *const V) }
    }
}

impl<V> Drop for Com<V> {
    fn drop(&mut self) {
        // SAFETY: every vtable starts with IUnknown's.
        unsafe {
            let u = &**(self.ptr as *const *const UnknownVtbl);
            (u.release)(self.ptr);
        }
    }
}

/// Call `f` with an out pointer and wrap what it returned.
///
/// # Safety
/// `f` must store an interface of vtable `V` (or null) into the pointer.
pub unsafe fn out<V>(what: &str, f: impl FnOnce(*mut *mut c_void) -> HRESULT) -> Result<Com<V>, String> {
    let mut p: *mut c_void = std::ptr::null_mut();
    let hr = f(&mut p);
    if hr < 0 {
        return Err(format!("{what}: {}", hr_text(hr)));
    }
    // SAFETY: per the contract above.
    unsafe { Com::from_raw(p) }.ok_or_else(|| format!("{what}: returned nothing"))
}

pub fn hr_text(hr: HRESULT) -> String {
    format!("HRESULT 0x{:08x}", hr as u32)
}

/// NUL-terminated UTF-16.
pub fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// A NUL-terminated UTF-16 string a COM API allocated, copied and freed.
///
/// # Safety
/// `p` must be null or a `CoTaskMemAlloc`ed NUL-terminated string.
pub unsafe fn take_co_string(p: *mut u16) -> String {
    if p.is_null() {
        return String::new();
    }
    // SAFETY: NUL-terminated per the contract.
    let s = unsafe {
        let mut n = 0;
        while *p.add(n) != 0 {
            n += 1;
        }
        String::from_utf16_lossy(std::slice::from_raw_parts(p, n))
    };
    // SAFETY: allocated with CoTaskMemAlloc per the contract.
    unsafe { CoTaskMemFree(p as *const c_void) };
    s
}

/// A UTF-16 buffer up to its first NUL.
pub fn from_wide(b: &[u16]) -> String {
    let n = b.iter().position(|c| *c == 0).unwrap_or(b.len());
    String::from_utf16_lossy(&b[..n])
}

/// COM on this thread for as long as the guard lives. If the thread is
/// already initialised in another model, that model is kept and the guard
/// does nothing (`RPC_E_CHANGED_MODE`).
pub struct Apartment(bool);

impl Apartment {
    /// The multithreaded apartment: device enumeration and WASAPI, whose
    /// objects are free-threaded and whose notifications arrive on threads
    /// of their own.
    pub fn mta() -> Apartment {
        // SAFETY: plain initialisation; balanced in Drop when it succeeded.
        let hr = unsafe { CoInitializeEx(std::ptr::null(), COINIT_MULTITHREADED as u32) };
        Apartment(hr >= 0)
    }
    /// A single-threaded apartment, which the shell's association objects
    /// and `ShellExecuteW` want.
    pub fn sta() -> Apartment {
        // SAFETY: as above.
        let hr = unsafe {
            CoInitializeEx(std::ptr::null(), (COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE) as u32)
        };
        Apartment(hr >= 0)
    }
}

impl Drop for Apartment {
    fn drop(&mut self) {
        if self.0 {
            // SAFETY: balances the successful CoInitializeEx of this guard.
            unsafe { CoUninitialize() };
        }
    }
}
