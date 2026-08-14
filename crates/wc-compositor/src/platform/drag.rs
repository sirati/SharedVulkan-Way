//! System drag and drop, from Win32's side.
//!
//! Windows' drag and drop is OLE. A source calls `DoDragDrop` with an
//! `IDataObject` and an `IDropSource` and does not get control back until the
//! user lets go; a target registers an `IDropTarget` with `RegisterDragDrop`.
//! None of that is a window message, which is why it is a module of its own
//! rather than three more arms in [`super::win32`]'s window procedure.
//!
//! # What is here
//!
//! **The drag source** — a Wayland client dragging out onto the Windows
//! desktop. That is the direction `docs/goal-firefox.md` asks for first,
//! because it is dragging a link or a file out of Firefox.
//!
//! **And the drop target**, added by §42: `RegisterDragDrop` and an
//! [`IDropTarget`] per toplevel, so that something dragged out of a Windows
//! application and over one of our windows becomes a `wl_data_device.enter` and
//! — if the client accepts — a drop whose bytes [`payload`] answers from.
//!
//! The two halves meet at exactly one place, and it is worth naming: both go
//! through [`super::win32::system_formats`] and `crate::clipboard`'s
//! conversions, so the bytes a Wayland client drags *out* and the bytes it is
//! offered on the way *in* cannot be described by two different tables.
//!
//! # Three vtables, written out
//!
//! `windows-sys` types every COM interface pointer as `*mut c_void` and ships
//! no vtable definitions, so [`IDropSource`], [`IDataObject`] and
//! [`IEnumFORMATETC`] are declared here. That is more honest than it sounds: a
//! vtable is a struct of function pointers in a documented order, the order is
//! the interface's inheritance chain followed by its own methods, and writing
//! it out is the whole of what a COM binding generator would do. Getting the
//! *order* wrong is the failure mode, and it presents as a call into the wrong
//! function rather than as a compile error — which is why each one is written
//! against the method list in the interface's own IDL, in order, with the
//! `IUnknown` three first.
//!
//! # The modal loop, and why this is entered from a posted message
//!
//! `DoDragDrop` runs a modal loop that does not return until the drop. **This
//! project has been bitten by a Win32 modal loop before**, twice over:
//! docs/waycompose-design.md §8.2's wakeup is a `PostMessage` to a window
//! rather than a `PostThreadMessage` precisely because `DefWindowProc`'s resize
//! loop discards thread messages, and stage 17's `SC_MOVE`/`SC_SIZE`/
//! `TrackPopupMenu` are **posted** rather than sent for a sharper reason that
//! applies here word for word:
//!
//! > This code runs inside `drain`, which sets `ui.draining` for its duration
//! > and re-posts `WM_WC_COMMAND` whenever another arrives while it is set — so
//! > a `SendMessageW` here would hold `draining` for the whole gesture and spin
//! > the pump re-posting a wake it cannot service.
//!
//! So [`run`] does not call `DoDragDrop`. It stashes the payload and posts
//! [`WM_WC_DRAG`] to the controller window; the drain returns, `draining`
//! clears, and the loop is entered from the pump in `controller_proc`. §8.2's
//! finding then applies unchanged: a posted wake still reaches a window from
//! inside a modal loop, so the compositor keeps painting while the user drags.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use windows_sys::core::{BOOL, GUID, HRESULT};
use windows_sys::Win32::Foundation::{GlobalFree, HGLOBAL, HWND};
use windows_sys::Win32::System::Com::{FORMATETC, STGMEDIUM};
use windows_sys::Win32::System::Memory::{
    GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock, GMEM_MOVEABLE,
};
use windows_sys::Win32::System::Ole::{
    DoDragDrop, OleInitialize, RegisterDragDrop, ReleaseStgMedium, RevokeDragDrop, DROPEFFECT,
    DROPEFFECT_COPY, DROPEFFECT_LINK, DROPEFFECT_MOVE, DROPEFFECT_NONE,
};
use windows_sys::Win32::UI::WindowsAndMessaging::PostMessageW;

use super::dnd_action;
use super::win32::Ui;

/// "Enter the OLE drag loop." Posted to the controller window by [`run`]; see
/// this module's header for why it is posted rather than called.
pub(crate) const WM_WC_DRAG: u32 = windows_sys::Win32::UI::WindowsAndMessaging::WM_APP + 2;

// --- the handful of constants that are not in windows-sys -------------------

const S_OK: HRESULT = 0;
const S_FALSE: HRESULT = 1;
const E_NOINTERFACE: HRESULT = 0x8000_4002u32 as HRESULT;
const E_NOTIMPL: HRESULT = 0x8000_4001u32 as HRESULT;
const E_INVALIDARG: HRESULT = 0x8007_0057u32 as HRESULT;
const E_OUTOFMEMORY: HRESULT = 0x8007_000Eu32 as HRESULT;
const DV_E_FORMATETC: HRESULT = 0x8004_0064u32 as HRESULT;
const OLE_E_ADVISENOTSUPPORTED: HRESULT = 0x8004_0003u32 as HRESULT;
const DRAGDROP_S_DROP: HRESULT = 0x0004_0100;
const DRAGDROP_S_CANCEL: HRESULT = 0x0004_0101;
const DRAGDROP_S_USEDEFAULTCURSORS: HRESULT = 0x0004_0102;

const TYMED_HGLOBAL: u32 = 1;
const DVASPECT_CONTENT: u32 = 1;
const DATADIR_GET: u32 = 1;
const MK_LBUTTON: u32 = 0x0001;

/// `IID_IUnknown`, and the three interfaces implemented here.
///
/// All four are in the `…-0000-0000-C000-000000000046` block, which is the
/// original OLE range; only the first word differs. Written out because
/// `windows-sys` exports the *functions* and not the interface identities.
const IID_IUNKNOWN: GUID = ole_iid(0x0000_0000);
const IID_IDROPSOURCE: GUID = ole_iid(0x0000_0121);
const IID_IDATAOBJECT: GUID = ole_iid(0x0000_010e);
const IID_IENUMFORMATETC: GUID = ole_iid(0x0000_0103);

const fn ole_iid(data1: u32) -> GUID {
    GUID {
        data1,
        data2: 0,
        data3: 0,
        data4: [0xC0, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x46],
    }
}

/// SAFETY: both pointers must be readable `GUID`s.
unsafe fn guid_eq(a: *const GUID, b: &GUID) -> bool {
    if a.is_null() {
        return false;
    }
    // SAFETY: the caller guarantees `a` is readable.
    let a = unsafe { &*a };
    a.data1 == b.data1 && a.data2 == b.data2 && a.data3 == b.data3 && a.data4 == b.data4
}

// --- vtable layouts ---------------------------------------------------------
//
// Each is `IUnknown`'s three methods followed by the interface's own, in the
// order the IDL declares them. The order is the entire contract.

#[repr(C)]
struct IUnknownVtbl<T> {
    query_interface: unsafe extern "system" fn(*mut T, *const GUID, *mut *mut core::ffi::c_void) -> HRESULT,
    add_ref: unsafe extern "system" fn(*mut T) -> u32,
    release: unsafe extern "system" fn(*mut T) -> u32,
}

#[repr(C)]
struct IDropSourceVtbl {
    base: IUnknownVtbl<DropSource>,
    query_continue_drag: unsafe extern "system" fn(*mut DropSource, BOOL, u32) -> HRESULT,
    give_feedback: unsafe extern "system" fn(*mut DropSource, DROPEFFECT) -> HRESULT,
}

#[repr(C)]
struct IDataObjectVtbl {
    base: IUnknownVtbl<DataObject>,
    get_data: unsafe extern "system" fn(*mut DataObject, *const FORMATETC, *mut STGMEDIUM) -> HRESULT,
    get_data_here: unsafe extern "system" fn(*mut DataObject, *const FORMATETC, *mut STGMEDIUM) -> HRESULT,
    query_get_data: unsafe extern "system" fn(*mut DataObject, *const FORMATETC) -> HRESULT,
    get_canonical_format_etc: unsafe extern "system" fn(*mut DataObject, *const FORMATETC, *mut FORMATETC) -> HRESULT,
    set_data: unsafe extern "system" fn(*mut DataObject, *const FORMATETC, *const STGMEDIUM, BOOL) -> HRESULT,
    enum_format_etc: unsafe extern "system" fn(*mut DataObject, u32, *mut *mut core::ffi::c_void) -> HRESULT,
    d_advise: unsafe extern "system" fn(*mut DataObject, *const FORMATETC, u32, *mut core::ffi::c_void, *mut u32) -> HRESULT,
    d_unadvise: unsafe extern "system" fn(*mut DataObject, u32) -> HRESULT,
    enum_d_advise: unsafe extern "system" fn(*mut DataObject, *mut *mut core::ffi::c_void) -> HRESULT,
}

#[repr(C)]
struct IEnumFormatEtcVtbl {
    base: IUnknownVtbl<EnumFormatEtc>,
    next: unsafe extern "system" fn(*mut EnumFormatEtc, u32, *mut FORMATETC, *mut u32) -> HRESULT,
    skip: unsafe extern "system" fn(*mut EnumFormatEtc, u32) -> HRESULT,
    reset: unsafe extern "system" fn(*mut EnumFormatEtc) -> HRESULT,
    clone: unsafe extern "system" fn(*mut EnumFormatEtc, *mut *mut core::ffi::c_void) -> HRESULT,
}

// --- IDropSource ------------------------------------------------------------

#[repr(C)]
struct DropSource {
    vtbl: *const IDropSourceVtbl,
    refs: Cell<u32>,
}

static DROP_SOURCE_VTBL: IDropSourceVtbl = IDropSourceVtbl {
    base: IUnknownVtbl {
        query_interface: ds_query_interface,
        add_ref: ds_add_ref,
        release: ds_release,
    },
    query_continue_drag: ds_query_continue_drag,
    give_feedback: ds_give_feedback,
};

unsafe extern "system" fn ds_query_interface(
    this: *mut DropSource,
    iid: *const GUID,
    out: *mut *mut core::ffi::c_void,
) -> HRESULT {
    if out.is_null() {
        return E_INVALIDARG;
    }
    // SAFETY: OLE passes a writable out-pointer.
    unsafe { *out = std::ptr::null_mut() };
    // SAFETY: `iid` is OLE's, and readable.
    if unsafe { guid_eq(iid, &IID_IUNKNOWN) || guid_eq(iid, &IID_IDROPSOURCE) } {
        // SAFETY: `this` is one of ours, alive for the call.
        unsafe {
            ds_add_ref(this);
            *out = this.cast();
        }
        return S_OK;
    }
    E_NOINTERFACE
}

unsafe extern "system" fn ds_add_ref(this: *mut DropSource) -> u32 {
    // SAFETY: `this` is a live `DropSource` this module allocated.
    let me = unsafe { &*this };
    let n = me.refs.get() + 1;
    me.refs.set(n);
    n
}

unsafe extern "system" fn ds_release(this: *mut DropSource) -> u32 {
    // SAFETY: as above.
    let n = {
        let me = unsafe { &*this };
        let n = me.refs.get().saturating_sub(1);
        me.refs.set(n);
        n
    };
    if n == 0 {
        // SAFETY: the last reference; reclaim the `Box` `new_drop_source` made.
        drop(unsafe { Box::from_raw(this) });
    }
    n
}

/// The whole of the drag's control flow, and it is three lines because OLE
/// asks the source only two questions.
///
/// Escape cancels; letting go of the button that started the drag drops; while
/// it is held, keep going. `MK_LBUTTON` rather than "any button", because a
/// drag started with the left button is not ended by releasing the right one.
unsafe extern "system" fn ds_query_continue_drag(
    _this: *mut DropSource,
    escape_pressed: BOOL,
    key_state: u32,
) -> HRESULT {
    if escape_pressed != 0 {
        return DRAGDROP_S_CANCEL;
    }
    if key_state & MK_LBUTTON == 0 {
        return DRAGDROP_S_DROP;
    }
    S_OK
}

/// Let the shell draw the cursors.
///
/// A compositor could draw its own — Wayland's `start_drag` even carries an
/// icon surface — but the cursor the user is looking at during a drag onto the
/// Windows desktop should be *Windows'*, so that the copy/move/no-entry glyph
/// matches what every other drag on that desktop looks like. The Wayland icon
/// surface is dropped, and that is recorded in §29.8 rather than here.
unsafe extern "system" fn ds_give_feedback(_this: *mut DropSource, _effect: DROPEFFECT) -> HRESULT {
    DRAGDROP_S_USEDEFAULTCURSORS
}

fn new_drop_source() -> *mut DropSource {
    Box::into_raw(Box::new(DropSource {
        vtbl: &DROP_SOURCE_VTBL,
        refs: Cell::new(1),
    }))
}

// --- IDataObject ------------------------------------------------------------

#[repr(C)]
struct DataObject {
    vtbl: *const IDataObjectVtbl,
    refs: Cell<u32>,
    /// The formats, already converted, in announcement order. **Eager**: OLE
    /// asks for the bytes with a synchronous `GetData` from inside the modal
    /// loop, and there is no round trip available to a Wayland client on
    /// another machine at that moment. §29.5 argues the same point for the
    /// clipboard; this is where it is forced rather than merely convenient.
    formats: Vec<(u32, Vec<u8>)>,
}

static DATA_OBJECT_VTBL: IDataObjectVtbl = IDataObjectVtbl {
    base: IUnknownVtbl {
        query_interface: do_query_interface,
        add_ref: do_add_ref,
        release: do_release,
    },
    get_data: do_get_data,
    get_data_here: do_get_data_here,
    query_get_data: do_query_get_data,
    get_canonical_format_etc: do_get_canonical,
    set_data: do_set_data,
    enum_format_etc: do_enum_format_etc,
    d_advise: do_d_advise,
    d_unadvise: do_d_unadvise,
    enum_d_advise: do_enum_d_advise,
};

unsafe extern "system" fn do_query_interface(
    this: *mut DataObject,
    iid: *const GUID,
    out: *mut *mut core::ffi::c_void,
) -> HRESULT {
    if out.is_null() {
        return E_INVALIDARG;
    }
    // SAFETY: OLE passes a writable out-pointer.
    unsafe { *out = std::ptr::null_mut() };
    // SAFETY: `iid` is OLE's, and readable.
    if unsafe { guid_eq(iid, &IID_IUNKNOWN) || guid_eq(iid, &IID_IDATAOBJECT) } {
        // SAFETY: `this` is one of ours, alive for the call.
        unsafe {
            do_add_ref(this);
            *out = this.cast();
        }
        return S_OK;
    }
    E_NOINTERFACE
}

unsafe extern "system" fn do_add_ref(this: *mut DataObject) -> u32 {
    // SAFETY: `this` is a live `DataObject` this module allocated.
    let me = unsafe { &*this };
    let n = me.refs.get() + 1;
    me.refs.set(n);
    n
}

unsafe extern "system" fn do_release(this: *mut DataObject) -> u32 {
    // SAFETY: as above.
    let n = {
        let me = unsafe { &*this };
        let n = me.refs.get().saturating_sub(1);
        me.refs.set(n);
        n
    };
    if n == 0 {
        // SAFETY: the last reference; reclaim the `Box`.
        drop(unsafe { Box::from_raw(this) });
    }
    n
}

/// Whether a request is one this object can answer, and which payload it means.
///
/// SAFETY: `fmt` must be readable.
unsafe fn wanted(this: *mut DataObject, fmt: *const FORMATETC) -> Option<usize> {
    if fmt.is_null() {
        return None;
    }
    // SAFETY: the caller guarantees `fmt` is readable, and `this` is ours.
    let (f, me) = unsafe { (&*fmt, &*this) };
    if f.tymed & TYMED_HGLOBAL == 0 || f.dwAspect != DVASPECT_CONTENT {
        return None;
    }
    me.formats.iter().position(|(cf, _)| *cf == u32::from(f.cfFormat))
}

/// Hand over a **copy**, which the caller then owns and frees.
///
/// Not the object's own buffer: `STGMEDIUM` with a null `pUnkForRelease` means
/// the receiver calls `ReleaseStgMedium` and the `HGLOBAL` is freed. Handing
/// out the same handle twice would be a double free, and a drop target is
/// entitled to ask twice.
unsafe extern "system" fn do_get_data(
    this: *mut DataObject,
    fmt: *const FORMATETC,
    medium: *mut STGMEDIUM,
) -> HRESULT {
    if medium.is_null() {
        return E_INVALIDARG;
    }
    // SAFETY: OLE passes a writable `STGMEDIUM`.
    unsafe { std::ptr::write(medium, std::mem::zeroed()) };
    // SAFETY: `fmt` is OLE's and readable.
    let Some(i) = (unsafe { wanted(this, fmt) }) else {
        return DV_E_FORMATETC;
    };
    // SAFETY: `this` is ours and `i` came from its own `formats`.
    // SAFETY: `this` is ours, alive for the call, and `i` indexes its own list.
    let me = unsafe { &*this };
    let bytes = &me.formats[i].1;
    let Some(handle) = copy_to_hglobal(bytes) else {
        return E_OUTOFMEMORY;
    };
    // SAFETY: writing the medium OLE gave us; `pUnkForRelease` stays null, so
    // the receiver owns and frees `handle`.
    unsafe {
        (*medium).tymed = TYMED_HGLOBAL;
        (*medium).u.hGlobal = handle;
        (*medium).pUnkForRelease = std::ptr::null_mut();
    }
    S_OK
}

/// Filling a caller-provided medium is optional and nothing this project talks
/// to asks for it. Refusing by name is what the interface expects; returning
/// `S_OK` without writing anything is how a drop target gets a garbage handle.
unsafe extern "system" fn do_get_data_here(
    _this: *mut DataObject,
    _fmt: *const FORMATETC,
    _medium: *mut STGMEDIUM,
) -> HRESULT {
    E_NOTIMPL
}

unsafe extern "system" fn do_query_get_data(
    this: *mut DataObject,
    fmt: *const FORMATETC,
) -> HRESULT {
    // SAFETY: `fmt` is OLE's and readable.
    if unsafe { wanted(this, fmt) }.is_some() {
        S_OK
    } else {
        DV_E_FORMATETC
    }
}

/// There is no canonical form here: every format this object offers is already
/// the only spelling of itself. `S_FALSE` with the input copied out is what the
/// interface asks for in that case.
unsafe extern "system" fn do_get_canonical(
    _this: *mut DataObject,
    input: *const FORMATETC,
    out: *mut FORMATETC,
) -> HRESULT {
    if out.is_null() {
        return E_INVALIDARG;
    }
    // SAFETY: OLE passes a writable `FORMATETC`; `input` may be null.
    unsafe {
        std::ptr::write(out, std::mem::zeroed());
        if !input.is_null() {
            std::ptr::write(out, *input);
        }
        (*out).ptd = std::ptr::null_mut();
    }
    S_FALSE
}

/// A drag source is read-only. A target that wanted to write back — which is
/// how `CFSTR_PERFORMEDDROPEFFECT` reports a move — is told so rather than
/// silently succeeding, because "the move happened" is a claim this object
/// cannot make on the client's behalf.
unsafe extern "system" fn do_set_data(
    _this: *mut DataObject,
    _fmt: *const FORMATETC,
    _medium: *const STGMEDIUM,
    _release: BOOL,
) -> HRESULT {
    E_NOTIMPL
}

unsafe extern "system" fn do_enum_format_etc(
    this: *mut DataObject,
    direction: u32,
    out: *mut *mut core::ffi::c_void,
) -> HRESULT {
    if out.is_null() {
        return E_INVALIDARG;
    }
    // SAFETY: OLE passes a writable out-pointer.
    unsafe { *out = std::ptr::null_mut() };
    if direction != DATADIR_GET {
        // Enumerating what can be *written* is meaningless for a read-only
        // source, and `E_NOTIMPL` is the documented answer.
        return E_NOTIMPL;
    }
    // SAFETY: `this` is ours.
    let formats: Vec<u32> = unsafe { (*this).formats.iter().map(|(cf, _)| *cf).collect() };
    // SAFETY: OLE owns the returned pointer and will `Release` it.
    unsafe { *out = new_enum(formats, 0).cast() };
    S_OK
}

unsafe extern "system" fn do_d_advise(
    _this: *mut DataObject,
    _fmt: *const FORMATETC,
    _advf: u32,
    _sink: *mut core::ffi::c_void,
    _conn: *mut u32,
) -> HRESULT {
    OLE_E_ADVISENOTSUPPORTED
}

unsafe extern "system" fn do_d_unadvise(_this: *mut DataObject, _conn: u32) -> HRESULT {
    OLE_E_ADVISENOTSUPPORTED
}

unsafe extern "system" fn do_enum_d_advise(
    _this: *mut DataObject,
    _out: *mut *mut core::ffi::c_void,
) -> HRESULT {
    OLE_E_ADVISENOTSUPPORTED
}

fn new_data_object(formats: Vec<(u32, Vec<u8>)>) -> *mut DataObject {
    Box::into_raw(Box::new(DataObject {
        vtbl: &DATA_OBJECT_VTBL,
        refs: Cell::new(1),
        formats,
    }))
}

/// A fresh moveable global with `bytes` in it, for a caller that will free it.
fn copy_to_hglobal(bytes: &[u8]) -> Option<HGLOBAL> {
    // SAFETY: an allocation request; the handle is checked.
    let handle = unsafe { GlobalAlloc(GMEM_MOVEABLE, bytes.len().max(1)) };
    if handle.is_null() {
        return None;
    }
    // SAFETY: a handle just allocated here.
    let ptr = unsafe { GlobalLock(handle) };
    if ptr.is_null() {
        // SAFETY: ours, never handed out.
        unsafe { GlobalFree(handle) };
        return None;
    }
    // SAFETY: `ptr` addresses at least `bytes.len()` bytes, and the two ranges
    // cannot overlap.
    unsafe {
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), ptr.cast::<u8>(), bytes.len());
        GlobalUnlock(handle);
    }
    Some(handle)
}

// --- IEnumFORMATETC ---------------------------------------------------------

#[repr(C)]
struct EnumFormatEtc {
    vtbl: *const IEnumFormatEtcVtbl,
    refs: Cell<u32>,
    formats: Vec<u32>,
    at: Cell<usize>,
}

static ENUM_VTBL: IEnumFormatEtcVtbl = IEnumFormatEtcVtbl {
    base: IUnknownVtbl {
        query_interface: en_query_interface,
        add_ref: en_add_ref,
        release: en_release,
    },
    next: en_next,
    skip: en_skip,
    reset: en_reset,
    clone: en_clone,
};

unsafe extern "system" fn en_query_interface(
    this: *mut EnumFormatEtc,
    iid: *const GUID,
    out: *mut *mut core::ffi::c_void,
) -> HRESULT {
    if out.is_null() {
        return E_INVALIDARG;
    }
    // SAFETY: OLE passes a writable out-pointer.
    unsafe { *out = std::ptr::null_mut() };
    // SAFETY: `iid` is OLE's, and readable.
    if unsafe { guid_eq(iid, &IID_IUNKNOWN) || guid_eq(iid, &IID_IENUMFORMATETC) } {
        // SAFETY: `this` is one of ours, alive for the call.
        unsafe {
            en_add_ref(this);
            *out = this.cast();
        }
        return S_OK;
    }
    E_NOINTERFACE
}

unsafe extern "system" fn en_add_ref(this: *mut EnumFormatEtc) -> u32 {
    // SAFETY: `this` is a live `EnumFormatEtc` this module allocated.
    let me = unsafe { &*this };
    let n = me.refs.get() + 1;
    me.refs.set(n);
    n
}

unsafe extern "system" fn en_release(this: *mut EnumFormatEtc) -> u32 {
    // SAFETY: as above.
    let n = {
        let me = unsafe { &*this };
        let n = me.refs.get().saturating_sub(1);
        me.refs.set(n);
        n
    };
    if n == 0 {
        // SAFETY: the last reference; reclaim the `Box`.
        drop(unsafe { Box::from_raw(this) });
    }
    n
}

/// **`S_FALSE` when fewer than `celt` were produced, including zero**, and
/// `pceltFetched` may be null only when `celt` is 1. Both are easy to get
/// wrong and both present as a drop target that sees no formats at all rather
/// than as an error.
unsafe extern "system" fn en_next(
    this: *mut EnumFormatEtc,
    celt: u32,
    out: *mut FORMATETC,
    fetched: *mut u32,
) -> HRESULT {
    if out.is_null() && celt > 0 {
        return E_INVALIDARG;
    }
    if celt != 1 && fetched.is_null() {
        return E_INVALIDARG;
    }
    // SAFETY: `this` is ours.
    let me = unsafe { &*this };
    let mut n = 0u32;
    while n < celt && me.at.get() < me.formats.len() {
        let cf = me.formats[me.at.get()];
        // SAFETY: the caller promised `celt` writable `FORMATETC`s at `out`.
        unsafe {
            std::ptr::write(
                out.add(n as usize),
                FORMATETC {
                    cfFormat: cf as u16,
                    ptd: std::ptr::null_mut(),
                    dwAspect: DVASPECT_CONTENT,
                    lindex: -1,
                    tymed: TYMED_HGLOBAL,
                },
            );
        }
        me.at.set(me.at.get() + 1);
        n += 1;
    }
    if !fetched.is_null() {
        // SAFETY: checked non-null.
        unsafe { *fetched = n };
    }
    if n == celt {
        S_OK
    } else {
        S_FALSE
    }
}

unsafe extern "system" fn en_skip(this: *mut EnumFormatEtc, celt: u32) -> HRESULT {
    // SAFETY: `this` is ours.
    let me = unsafe { &*this };
    let want = me.at.get() + celt as usize;
    me.at.set(want.min(me.formats.len()));
    if want <= me.formats.len() {
        S_OK
    } else {
        S_FALSE
    }
}

unsafe extern "system" fn en_reset(this: *mut EnumFormatEtc) -> HRESULT {
    // SAFETY: `this` is ours.
    unsafe { (*this).at.set(0) };
    S_OK
}

unsafe extern "system" fn en_clone(
    this: *mut EnumFormatEtc,
    out: *mut *mut core::ffi::c_void,
) -> HRESULT {
    if out.is_null() {
        return E_INVALIDARG;
    }
    // SAFETY: `this` is ours, and `out` is writable.
    unsafe {
        let me = &*this;
        *out = new_enum(me.formats.clone(), me.at.get()).cast();
    }
    S_OK
}

fn new_enum(formats: Vec<u32>, at: usize) -> *mut EnumFormatEtc {
    Box::into_raw(Box::new(EnumFormatEtc {
        vtbl: &ENUM_VTBL,
        refs: Cell::new(1),
        formats,
        at: Cell::new(at),
    }))
}

// --- the drag itself --------------------------------------------------------

/// A drag that has been staged and is waiting for the pump to enter its loop.
pub(crate) struct Pending {
    formats: Vec<(u32, Vec<u8>)>,
    effects: DROPEFFECT,
}

thread_local! {
    /// The staged drag, and whether OLE has been initialised on this thread.
    ///
    /// Thread-local rather than a field on `Ui` because both are properties of
    /// the **UI thread** specifically — `OleInitialize` enters an apartment,
    /// and an apartment belongs to a thread — and because the drag is entered
    /// from `controller_proc`, which has only `ui()` to work with anyway.
    static PENDING: RefCell<Option<Pending>> = const { RefCell::new(None) };
    static OLE_READY: Cell<bool> = const { Cell::new(false) };

    // --- the drop target's side, §42 ---------------------------------------
    /// The bytes read out of the incoming `IDataObject` at the moment of the
    /// drop, by system format. See [`payload`] for why they are copied here
    /// rather than pulled when the client asks.
    static DROPPED: RefCell<Vec<(crate::clipboard::SystemFormat, Vec<u8>)>> =
        const { RefCell::new(Vec::new()) };
    /// What the client under the drag last told us, as a `DROPEFFECT`.
    ///
    /// This is the whole of the answer `IDropTarget::DragOver` gives OLE, and
    /// it is **one round trip stale by design** — see
    /// [`crate::platform::ToUi::DragFeedback`], which is where that decision is
    /// argued. `DROPEFFECT_NONE` until the client says otherwise, so a drag
    /// over a window whose client has not answered shows the no-entry cursor
    /// rather than promising a drop that would do nothing.
    static FEEDBACK: Cell<DROPEFFECT> = const { Cell::new(DROPEFFECT_NONE) };
    /// The window a drag is currently over, so `Drop` and `DragLeave` can name
    /// it without OLE telling them — neither method carries an `HWND`.
    static OVER: Cell<u64> = const { Cell::new(0) };
}

/// Stage a drag and ask the pump to run it.
///
/// Returns `None` when the drag has been handed off, and `Some(action)` when it
/// could not be started at all — in which case the caller answers immediately,
/// because a `start_drag` that produced no `drag_finished` is the hang this
/// whole feature exists not to build.
pub(crate) fn run(ui: &Rc<Ui>, payloads: &[(String, Vec<u8>)], actions: u32) -> Option<u32> {
    let formats = super::win32::system_formats(ui, payloads);
    if formats.is_empty() {
        println!(
            "wc-compositor: a client started a drag offering {} mime type(s), none of which \
             this compositor has a Windows format for; ending it now with no drop",
            payloads.len()
        );
        return Some(dnd_action::NONE);
    }
    let effects = to_dropeffect(actions);
    if effects == DROPEFFECT_NONE {
        println!(
            "wc-compositor: a client started a drag permitting {} ({actions:#x}); ending it now",
            dnd_action::describe(actions)
        );
        return Some(dnd_action::NONE);
    }
    PENDING.with(|p| *p.borrow_mut() = Some(Pending { formats, effects }));
    // **Posted, not called.** See this module's header: `DoDragDrop` is a modal
    // loop and this runs inside `drain`, which holds `ui.draining`.
    // SAFETY: a live window handle owned by this thread.
    unsafe { PostMessageW(ui.controller.get() as HWND, WM_WC_DRAG, 0, 0) };
    None
}

/// Run the staged drag. Called from `controller_proc`, on the pump, with no
/// drain in progress.
pub(crate) fn enter_modal_loop() -> Option<u32> {
    let pending = PENDING.with(|p| p.borrow_mut().take())?;
    if !ole_ready() {
        return Some(dnd_action::NONE);
    }
    let data = new_data_object(pending.formats);
    let source = new_drop_source();
    let mut effect: DROPEFFECT = DROPEFFECT_NONE;
    // SAFETY: `data` was just created here and is alive.
    let n = unsafe { &*data }.formats.len();
    println!(
        "wc-compositor: entering DoDragDrop with {n} format(s), allowed DROPEFFECT {:#x}",
        pending.effects
    );
    // SAFETY: two live COM objects this module allocated, each with one
    // reference held here, and a writable out-parameter. `DoDragDrop` does not
    // return until the drop or the cancellation.
    let hr = unsafe { DoDragDrop(data.cast(), source.cast(), pending.effects, &mut effect) };
    // SAFETY: releasing the one reference each was created with. OLE has
    // released whatever it took.
    unsafe {
        do_release(data);
        ds_release(source);
    }
    let action = match hr {
        DRAGDROP_S_DROP => from_dropeffect(effect),
        DRAGDROP_S_CANCEL => dnd_action::NONE,
        _ => {
            println!("wc-compositor: DoDragDrop failed with {hr:#010x}");
            dnd_action::NONE
        }
    };
    println!(
        "wc-compositor: DoDragDrop returned {hr:#010x}, effect {effect:#x} -> \
         wl_data_device_manager action {action:#x}"
    );
    Some(action)
}

/// `OleInitialize`, once per thread.
///
/// Separate from `CoInitializeEx` on purpose: OLE's drag and drop needs the
/// *OLE* apartment, not merely a COM one, and `OleInitialize` is what
/// registers the clipboard and drag-drop plumbing that `DoDragDrop` uses.
/// Called lazily rather than at startup because a compositor whose clients
/// never drag anything should not enter an apartment at all.
fn ole_ready() -> bool {
    OLE_READY.with(|r| {
        if r.get() {
            return true;
        }
        // SAFETY: a null reserved argument, on the UI thread.
        let hr = unsafe { OleInitialize(std::ptr::null()) };
        // `S_FALSE` means already initialised, which is a success.
        let ok = hr == S_OK || hr == S_FALSE;
        if !ok {
            eprintln!("wc-compositor: OleInitialize failed with {hr:#010x}; no drag can start");
        }
        r.set(ok);
        ok
    })
}

/// The Wayland source withdrew before the loop was entered.
///
/// Once `DoDragDrop` is running there is nothing to withdraw: OLE owns the
/// gesture until the user lets go, and the honest answer is to let it finish.
pub(crate) fn cancel(_ui: &Rc<Ui>) {
    if PENDING.with(|p| p.borrow_mut().take()).is_some() {
        println!("wc-compositor: the drag was withdrawn before it started");
    }
}

/// One mime type out of the drag currently over one of our windows.
///
/// The bytes of the drag that was last dropped on one of our windows, as the
/// mime type the client asked for.
///
/// **REWRITTEN FOR §42**, and until then this returned `None` unconditionally
/// because nothing could be dragged onto these windows at all.
///
/// Answered out of a copy taken at the drop rather than from the `IDataObject`
/// now, and that is forced rather than chosen: OLE's data object belongs to the
/// *source* application and is released when `Drop` returns, while the client's
/// `fetch` arrives later — after a hypervisor crossing, after the client has
/// decided it wants the bytes. Holding the interface pointer past the drop and
/// calling `GetData` on it then would be calling into another process's object
/// after telling that process the drag was over. §29.5's argument for the eager
/// pull on the clipboard is the same argument with a different deadline.
pub(crate) fn payload(_ui: &Rc<Ui>, mime: &str) -> Option<Vec<u8>> {
    DROPPED.with(|d| {
        d.borrow()
            .iter()
            .find_map(|(format, bytes)| crate::clipboard::from_system(mime, *format, bytes))
    })
}
// --- IDropTarget: the other direction (§42) ----------------------------------
//
// Everything above this line is the drag *source* — a Wayland client dragging
// onto the Windows desktop. This is the target: a Windows application dragging
// onto one of our windows, which §29.8 named as the missing half and which is
// the second sentence of the user's requirement.
//
// **The shape is inverted, and one consequence is easy to miss.** As a source
// we are asked for bytes and answer; as a target we are *told* about bytes and
// have to decide, synchronously, what would happen if the user let go — while
// the only party who can decide that is a Wayland client on the other side of a
// hypervisor. `FEEDBACK` is that decision arriving late, and
// `crate::platform::ToUi::DragFeedback` is where the cost is argued.

/// `IID_IDropTarget`, in the same OLE range as the three above.
const IID_IDROPTARGET: GUID = ole_iid(0x0000_0122);

/// `POINTL`, which OLE passes to three of the four methods **by value**.
///
/// Declared here rather than imported so that the layout this code compiles
/// against is visible at the call site: two `i32`s, eight bytes, which the
/// x86-64 calling convention passes in a register. A `*const POINTL` here —
/// the shape the other three arguments have — would read the *pointer* as
/// coordinates and place every drop at an address.
#[repr(C)]
#[derive(Clone, Copy)]
pub(crate) struct PointL {
    pub x: i32,
    pub y: i32,
}

#[repr(C)]
struct IDropTargetVtbl {
    base: IUnknownVtbl<DropTarget>,
    drag_enter: unsafe extern "system" fn(
        *mut DropTarget,
        *mut core::ffi::c_void,
        u32,
        PointL,
        *mut DROPEFFECT,
    ) -> HRESULT,
    drag_over: unsafe extern "system" fn(*mut DropTarget, u32, PointL, *mut DROPEFFECT) -> HRESULT,
    drag_leave: unsafe extern "system" fn(*mut DropTarget) -> HRESULT,
    drop: unsafe extern "system" fn(
        *mut DropTarget,
        *mut core::ffi::c_void,
        u32,
        PointL,
        *mut DROPEFFECT,
    ) -> HRESULT,
}

/// One per window, owned by OLE between `RegisterDragDrop` and
/// `RevokeDragDrop`.
///
/// It carries the `WindowId` rather than the `HWND` because that is what the
/// Wayland side speaks, and because the four methods it implements are given no
/// window at all — `DragLeave` and `Drop` have to know which of our windows
/// they belong to, and the only place that can come from is here.
#[repr(C)]
struct DropTarget {
    vtbl: *const IDropTargetVtbl,
    refs: Cell<u32>,
    window: u64,
}

static DROP_TARGET_VTBL: IDropTargetVtbl = IDropTargetVtbl {
    base: IUnknownVtbl {
        query_interface: dt_query_interface,
        add_ref: dt_add_ref,
        release: dt_release,
    },
    drag_enter: dt_drag_enter,
    drag_over: dt_drag_over,
    drag_leave: dt_drag_leave,
    drop: dt_drop,
};

unsafe extern "system" fn dt_query_interface(
    this: *mut DropTarget,
    iid: *const GUID,
    out: *mut *mut core::ffi::c_void,
) -> HRESULT {
    if out.is_null() {
        return E_INVALIDARG;
    }
    // SAFETY: OLE passes a writable out-pointer. Nulled even on refusal,
    // because OLE reads it either way — the same rule the source's three
    // objects follow.
    unsafe { *out = std::ptr::null_mut() };
    // SAFETY: `iid` is OLE's, and readable.
    if unsafe { guid_eq(iid, &IID_IUNKNOWN) || guid_eq(iid, &IID_IDROPTARGET) } {
        // SAFETY: `this` is one of ours, alive for the call.
        unsafe {
            dt_add_ref(this);
            *out = this.cast();
        }
        return S_OK;
    }
    E_NOINTERFACE
}

unsafe extern "system" fn dt_add_ref(this: *mut DropTarget) -> u32 {
    // SAFETY: `this` is a live `DropTarget` this module allocated.
    let me = unsafe { &*this };
    let n = me.refs.get() + 1;
    me.refs.set(n);
    n
}

unsafe extern "system" fn dt_release(this: *mut DropTarget) -> u32 {
    // SAFETY: as above.
    let n = {
        let me = unsafe { &*this };
        let n = me.refs.get().saturating_sub(1);
        me.refs.set(n);
        n
    };
    if n == 0 {
        // SAFETY: the last reference; reclaim the `Box` `register` made.
        drop(unsafe { Box::from_raw(this) });
    }
    n
}

/// A Windows drag arrived over one of our windows.
///
/// The formats are read **now** and the bytes are not: what the Wayland client
/// needs at this moment is the mime list, so it can decide whether it wants
/// any of them, and reading megabytes on every enter would put that read inside
/// OLE's modal loop for a gesture the user may be about to move elsewhere.
unsafe extern "system" fn dt_drag_enter(
    this: *mut DropTarget,
    data: *mut core::ffi::c_void,
    _key_state: u32,
    pt: PointL,
    effect: *mut DROPEFFECT,
) -> HRESULT {
    // SAFETY: `this` is live for the call.
    let window = unsafe { &*this }.window;
    let formats = unsafe { formats_of(data) };
    let mimes = crate::clipboard::mimes_for(&formats);
    // Nothing has answered yet, so the first `DragOver` says no. See `FEEDBACK`.
    FEEDBACK.with(|f| f.set(DROPEFFECT_NONE));
    OVER.with(|o| o.set(window));
    DROPPED.with(|d| d.borrow_mut().clear());
    // SAFETY: OLE passes a writable out-pointer.
    unsafe { write_effect(effect, DROPEFFECT_NONE) };
    if mimes.is_empty() {
        // A drag of something we cannot describe — a bitmap, a shell object.
        // Told to the Wayland side as nothing rather than as an empty drag: a
        // `wl_data_device.enter` with no mime types is a legal thing to send
        // and an unhelpful one to receive.
        println!("wc-compositor: a Windows drag entered window {window} offering nothing we speak");
        return S_OK;
    }
    if let Some(ui) = super::win32::ui() {
        let (x, y) = super::win32::screen_to_surface(&ui, window, pt.x, pt.y);
        let _ = ui.events.send(super::FromUi::DragEnter {
            id: window,
            x,
            y,
            mimes,
            // What Windows will let the user ask for. The keyboard modifiers
            // narrow it — Ctrl means copy, Shift means move — and OLE reports
            // that in `grfKeyState`; the client is told the whole set and
            // chooses, which is Wayland's model and the more informative one.
            actions: dnd_action::COPY | dnd_action::MOVE,
            time: super::win32::now_ms(),
        });
    }
    S_OK
}

/// The drag moved. This is the method that must answer synchronously.
unsafe extern "system" fn dt_drag_over(
    this: *mut DropTarget,
    _key_state: u32,
    pt: PointL,
    effect: *mut DROPEFFECT,
) -> HRESULT {
    // SAFETY: `this` is live for the call.
    let window = unsafe { &*this }.window;
    // SAFETY: OLE passes a writable out-pointer.
    unsafe { write_effect(effect, FEEDBACK.with(|f| f.get())) };
    if let Some(ui) = super::win32::ui() {
        let (x, y) = super::win32::screen_to_surface(&ui, window, pt.x, pt.y);
        let _ = ui.events.send(super::FromUi::DragMotion {
            id: window,
            x,
            y,
            time: super::win32::now_ms(),
        });
    }
    S_OK
}

unsafe extern "system" fn dt_drag_leave(this: *mut DropTarget) -> HRESULT {
    // SAFETY: `this` is live for the call.
    let window = unsafe { &*this }.window;
    FEEDBACK.with(|f| f.set(DROPEFFECT_NONE));
    OVER.with(|o| o.set(0));
    if let Some(ui) = super::win32::ui() {
        let _ = ui.events.send(super::FromUi::DragLeave {
            id: window,
            time: super::win32::now_ms(),
        });
    }
    S_OK
}

/// The user let go over our window.
///
/// **The bytes are read here, before this returns.** The `IDataObject` belongs
/// to the source application and is released when this method returns; the
/// Wayland client's `fetch` arrives later, across a hypervisor. See [`payload`].
unsafe extern "system" fn dt_drop(
    this: *mut DropTarget,
    data: *mut core::ffi::c_void,
    _key_state: u32,
    pt: PointL,
    effect: *mut DROPEFFECT,
) -> HRESULT {
    // SAFETY: `this` is live for the call.
    let window = unsafe { &*this }.window;
    let taken = FEEDBACK.with(|f| f.get());
    // SAFETY: OLE passes a writable out-pointer. What is reported here is what
    // the *client* accepted, which is the number the source application uses to
    // decide whether to delete its original on a move.
    unsafe { write_effect(effect, taken) };
    let bytes = unsafe { read_all(data) };
    println!(
        "wc-compositor: a Windows drag was dropped on window {window}: {} format(s) read, \
         effect {taken:#x}",
        bytes.len()
    );
    DROPPED.with(|d| *d.borrow_mut() = bytes);
    FEEDBACK.with(|f| f.set(DROPEFFECT_NONE));
    OVER.with(|o| o.set(0));
    if let Some(ui) = super::win32::ui() {
        let (x, y) = super::win32::screen_to_surface(&ui, window, pt.x, pt.y);
        let _ = ui.events.send(super::FromUi::DragDrop {
            id: window,
            x,
            y,
            time: super::win32::now_ms(),
        });
    }
    S_OK
}

/// SAFETY: `effect` must be a writable `DROPEFFECT` or null.
unsafe fn write_effect(effect: *mut DROPEFFECT, value: DROPEFFECT) {
    if !effect.is_null() {
        // SAFETY: the caller guarantees it is writable.
        unsafe { *effect = value };
    }
}

// --- reading somebody else's IDataObject ------------------------------------
//
// The source's object is a COM interface pointer this process did not create,
// so its vtable has to be described again with `*mut c_void` receivers. Only
// the three methods used are named; the rest of the layout is padding whose
// only job is to put those three at the right offsets, which is why they are
// declared and never called.

#[repr(C)]
struct RemoteDataObjectVtbl {
    query_interface: unsafe extern "system" fn(*mut core::ffi::c_void, *const GUID, *mut *mut core::ffi::c_void) -> HRESULT,
    add_ref: unsafe extern "system" fn(*mut core::ffi::c_void) -> u32,
    release: unsafe extern "system" fn(*mut core::ffi::c_void) -> u32,
    get_data: unsafe extern "system" fn(*mut core::ffi::c_void, *const FORMATETC, *mut STGMEDIUM) -> HRESULT,
    get_data_here: unsafe extern "system" fn(*mut core::ffi::c_void, *const FORMATETC, *mut STGMEDIUM) -> HRESULT,
    query_get_data: unsafe extern "system" fn(*mut core::ffi::c_void, *const FORMATETC) -> HRESULT,
    get_canonical_format_etc: unsafe extern "system" fn(*mut core::ffi::c_void, *const FORMATETC, *mut FORMATETC) -> HRESULT,
    set_data: unsafe extern "system" fn(*mut core::ffi::c_void, *const FORMATETC, *const STGMEDIUM, BOOL) -> HRESULT,
    enum_format_etc: unsafe extern "system" fn(*mut core::ffi::c_void, u32, *mut *mut core::ffi::c_void) -> HRESULT,
    d_advise: unsafe extern "system" fn(*mut core::ffi::c_void, *const FORMATETC, u32, *mut core::ffi::c_void, *mut u32) -> HRESULT,
    d_unadvise: unsafe extern "system" fn(*mut core::ffi::c_void, u32) -> HRESULT,
    enum_d_advise: unsafe extern "system" fn(*mut core::ffi::c_void, *mut *mut core::ffi::c_void) -> HRESULT,
}

/// A `FORMATETC` asking for one clipboard format as an `HGLOBAL`.
fn asking_for(format: u32) -> FORMATETC {
    FORMATETC {
        cfFormat: format as u16,
        ptd: std::ptr::null_mut(),
        dwAspect: DVASPECT_CONTENT,
        lindex: -1,
        tymed: TYMED_HGLOBAL,
    }
}

/// Which of the formats this compositor understands the drag is offering.
///
/// `QueryGetData` per format rather than walking `IEnumFORMATETC`, and it is
/// the more robust of the two: an enumerator is a second object with its own
/// contract — the same `S_FALSE`-on-short-read contract this module implements
/// for the *source* and had to write four tests about — while `QueryGetData` is
/// one call with one answer, asked only about the three formats we could do
/// anything with anyway.
///
/// SAFETY: `data` must be a live `IDataObject` or null.
unsafe fn formats_of(data: *mut core::ffi::c_void) -> Vec<crate::clipboard::SystemFormat> {
    let mut out = Vec::new();
    if data.is_null() {
        return out;
    }
    let Some(ui) = super::win32::ui() else { return out };
    // SAFETY: a live COM object: the first word is its vtable pointer.
    let vtbl = unsafe { *(data as *const *const RemoteDataObjectVtbl) };
    if vtbl.is_null() {
        return out;
    }
    for format in crate::clipboard::SystemFormat::ALL {
        let etc = asking_for(super::win32::format_id(&ui, format));
        // SAFETY: `vtbl` is the object's, `etc` is a well-formed FORMATETC.
        let hr = unsafe { ((*vtbl).query_get_data)(data, &etc) };
        if hr == S_OK {
            out.push(format);
        }
    }
    out
}

/// Read every format we understand out of the object, as bytes.
///
/// SAFETY: `data` must be a live `IDataObject` or null.
unsafe fn read_all(data: *mut core::ffi::c_void) -> Vec<(crate::clipboard::SystemFormat, Vec<u8>)> {
    let mut out = Vec::new();
    if data.is_null() {
        return out;
    }
    let Some(ui) = super::win32::ui() else { return out };
    // SAFETY: a live COM object.
    let vtbl = unsafe { *(data as *const *const RemoteDataObjectVtbl) };
    if vtbl.is_null() {
        return out;
    }
    // SAFETY: the formats are the ones `formats_of` would have accepted.
    for format in unsafe { formats_of(data) } {
        let etc = asking_for(super::win32::format_id(&ui, format));
        let mut medium: STGMEDIUM = unsafe { std::mem::zeroed() };
        // SAFETY: `etc` is well-formed and `medium` is writable.
        let hr = unsafe { ((*vtbl).get_data)(data, &etc, &mut medium) };
        if hr != S_OK {
            continue;
        }
        // SAFETY: `tymed` says the union holds an `HGLOBAL`, which is what the
        // `FORMATETC` asked for; a source that answers `S_OK` for a different
        // medium is out of contract and is skipped rather than trusted.
        let bytes = if medium.tymed == TYMED_HGLOBAL {
            unsafe { read_hglobal(medium.u.hGlobal) }
        } else {
            None
        };
        // **`ReleaseStgMedium`, always.** The medium is the source's memory and
        // the receiver frees it; skipping this leaks whatever was dragged, once
        // per drop, in the compositor rather than in the application that
        // dragged it.
        // SAFETY: `medium` was filled by `GetData` and is released exactly once.
        unsafe { ReleaseStgMedium(&mut medium) };
        if let Some(bytes) = bytes {
            out.push((format, bytes));
        }
    }
    out
}

/// SAFETY: `handle` must be a live `HGLOBAL` or null.
unsafe fn read_hglobal(handle: HGLOBAL) -> Option<Vec<u8>> {
    if handle.is_null() {
        return None;
    }
    // SAFETY: a live moveable global.
    let size = unsafe { GlobalSize(handle) };
    if size == 0 {
        return Some(Vec::new());
    }
    // SAFETY: locked for the copy and unlocked immediately after.
    let p = unsafe { GlobalLock(handle) };
    if p.is_null() {
        return None;
    }
    // SAFETY: `GlobalSize` bytes are readable from the locked pointer.
    let bytes = unsafe { std::slice::from_raw_parts(p as *const u8, size) }.to_vec();
    // SAFETY: paired with the lock above.
    unsafe { GlobalUnlock(handle) };
    Some(bytes)
}

// --- registration -----------------------------------------------------------

/// Let this window be dropped onto.
///
/// Called for every toplevel as it is created. A popup is deliberately not
/// registered: a menu is not a drop destination, and `RegisterDragDrop` on a
/// `WS_EX_NOACTIVATE` window whose lifetime is a gesture is a revoke waiting to
/// be forgotten.
pub(crate) fn register(hwnd: HWND, window: u64) {
    if !ole_ready() {
        return;
    }
    let target = Box::into_raw(Box::new(DropTarget {
        vtbl: &DROP_TARGET_VTBL,
        refs: Cell::new(1),
        window,
    }));
    // SAFETY: a live window handle on the thread that owns it, and a COM object
    // this module allocated. OLE takes its own reference.
    let hr = unsafe { RegisterDragDrop(hwnd, target.cast()) };
    if hr == S_OK {
        println!("wc-compositor: window {window} accepts drops from Windows applications");
    } else {
        // Not fatal, and said out loud rather than swallowed: a compositor
        // whose windows silently refuse drops looks exactly like one whose
        // clients refuse them, and the two have nothing in common.
        println!("wc-compositor: window {window} could not be made a drop target ({hr:#010x})");
    }
    // Ours is released either way: on success OLE holds its own reference, and
    // on failure there is nothing to hold it.
    // SAFETY: the reference `Box::into_raw` handed us.
    unsafe { dt_release(target) };
}

/// Stop accepting drops, before the window goes.
///
/// **`RevokeDragDrop` before `DestroyWindow`, not after.** OLE keys its
/// registration on the `HWND`; a handle that has been destroyed cannot be
/// revoked, and the entry stays in OLE's table until the process exits — which
/// on a compositor whose windows come and go with a client's menus is a leak
/// that grows with use.
pub(crate) fn revoke(hwnd: HWND) {
    if !OLE_READY.with(|r| r.get()) {
        return;
    }
    // SAFETY: a live window handle on the thread that owns it.
    unsafe { RevokeDragDrop(hwnd) };
}


// --- the two vocabularies ---------------------------------------------------

/// What to allow a Win32 drop target to do, given what the Wayland source
/// permits.
///
/// **`DROPEFFECT_COPY` and `DROPEFFECT_MOVE` are 1 and 2, exactly as Wayland's
/// `copy` and `move` are — and `DROPEFFECT_LINK` is 4 where Wayland's `ask` is
/// 4.** That coincidence is the trap: a conversion written as `actions as u32`
/// works for every drag anybody tests by hand and turns a shortcut into a
/// "let me choose" menu the first time somebody drags with the right button.
/// So the two are converted explicitly, with a test.
pub(crate) fn to_dropeffect(actions: u32) -> DROPEFFECT {
    let mut out = DROPEFFECT_NONE;
    if actions & dnd_action::COPY != 0 {
        out |= DROPEFFECT_COPY;
    }
    if actions & dnd_action::MOVE != 0 {
        out |= DROPEFFECT_MOVE;
    }
    // `ask` has no Win32 equivalent: Windows asks by offering a menu on a
    // right-button drag, which is the shell's decision and not a source's. A
    // source that would have asked is offering both, which is the same set of
    // outcomes with the question left to the user's modifier keys.
    if actions & dnd_action::ASK != 0 {
        out |= DROPEFFECT_COPY | DROPEFFECT_MOVE;
    }
    out
}

/// The inverse, for what a drop actually did.
pub(crate) fn from_dropeffect(effect: DROPEFFECT) -> u32 {
    // Move first: a shell that reports `COPY | MOVE` on a completed drop is
    // reporting what it *could* have done, and taking the destructive reading
    // of an ambiguous answer is wrong. In practice `DoDragDrop` returns exactly
    // one bit; this is the order to resolve it in if it ever does not.
    if effect & DROPEFFECT_MOVE != 0 {
        return dnd_action::MOVE;
    }
    if effect & (DROPEFFECT_COPY | DROPEFFECT_LINK) != 0 {
        // A link is a copy as far as Wayland can express it. Saying `none`
        // instead would tell the source its data was refused when it was taken.
        return dnd_action::COPY;
    }
    dnd_action::NONE
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_two_drag_vocabularies_do_not_share_a_numbering() {
        // `ask` is 4 on one side and `LINK` is 4 on the other, which is the
        // whole reason this conversion is a function.
        assert_eq!(
            to_dropeffect(dnd_action::ASK),
            DROPEFFECT_COPY | DROPEFFECT_MOVE
        );
        assert_eq!(from_dropeffect(DROPEFFECT_LINK), dnd_action::COPY);
        assert_eq!(to_dropeffect(dnd_action::COPY), DROPEFFECT_COPY);
        assert_eq!(
            to_dropeffect(dnd_action::COPY | dnd_action::MOVE),
            DROPEFFECT_COPY | DROPEFFECT_MOVE
        );
        assert_eq!(to_dropeffect(dnd_action::NONE), DROPEFFECT_NONE);
        assert_eq!(from_dropeffect(DROPEFFECT_NONE), dnd_action::NONE);
        assert_eq!(from_dropeffect(DROPEFFECT_MOVE), dnd_action::MOVE);
        // The destructive one wins when a shell reports both.
        assert_eq!(
            from_dropeffect(DROPEFFECT_COPY | DROPEFFECT_MOVE),
            dnd_action::MOVE
        );
    }

    /// The enumerator's contract, which is easy to get wrong in a way that
    /// presents as a drop target seeing no formats rather than as an error.
    #[test]
    fn the_format_enumerator_reports_s_false_when_it_runs_out() {
        let e = new_enum(vec![13, 49_161], 0);
        let mut buf = [FORMATETC::default(); 4];
        let mut got = 0u32;
        // SAFETY: `e` is ours and `buf` has room for four.
        let hr = unsafe { en_next(e, 4, buf.as_mut_ptr(), &mut got) };
        assert_eq!(hr, S_FALSE, "fewer than asked for is S_FALSE");
        assert_eq!(got, 2);
        assert_eq!(buf[0].cfFormat, 13);
        assert_eq!(buf[0].tymed, TYMED_HGLOBAL);
        assert_eq!(buf[1].cfFormat, 49_161u32 as u16);
        // SAFETY: as above.
        unsafe {
            assert_eq!(en_next(e, 1, buf.as_mut_ptr(), &mut got), S_FALSE);
            assert_eq!(got, 0, "exhausted");
            assert_eq!(en_reset(e), S_OK);
            assert_eq!(en_next(e, 2, buf.as_mut_ptr(), &mut got), S_OK);
            assert_eq!(got, 2);
            assert_eq!(en_release(e), 0);
        }
    }

    /// `QueryInterface` must answer for `IUnknown` and for its own interface
    /// and refuse everything else — and the refusal has to null the out
    /// pointer, because OLE reads it either way.
    #[test]
    fn query_interface_answers_for_iunknown_and_refuses_the_rest() {
        let d = new_data_object(vec![(13, b"hi".to_vec())]);
        let mut out: *mut core::ffi::c_void = std::ptr::dangling_mut();
        // SAFETY: `d` is ours and `out` is writable.
        unsafe {
            assert_eq!(do_query_interface(d, &IID_IDATAOBJECT, &mut out), S_OK);
            assert!(!out.is_null());
            assert_eq!(do_release(d), 1, "QueryInterface took a reference");
            assert_eq!(do_query_interface(d, &IID_IUNKNOWN, &mut out), S_OK);
            assert_eq!(do_release(d), 1);
            assert_eq!(do_query_interface(d, &IID_IDROPSOURCE, &mut out), E_NOINTERFACE);
            assert!(out.is_null(), "a refusal must still null the out pointer");
            assert_eq!(do_release(d), 0);
        }
    }

    /// The two questions OLE asks a drop source, and the two answers that make
    /// a drag end.
    #[test]
    fn the_drop_source_ends_the_drag_on_escape_and_on_the_button_coming_up() {
        let s = new_drop_source();
        // SAFETY: `s` is ours.
        unsafe {
            assert_eq!(ds_query_continue_drag(s, 1, MK_LBUTTON), DRAGDROP_S_CANCEL);
            assert_eq!(ds_query_continue_drag(s, 0, MK_LBUTTON), S_OK);
            assert_eq!(ds_query_continue_drag(s, 0, 0), DRAGDROP_S_DROP);
            assert_eq!(ds_give_feedback(s, DROPEFFECT_COPY), DRAGDROP_S_USEDEFAULTCURSORS);
            assert_eq!(ds_release(s), 0);
        }
    }

    /// A `GetData` hands out a **copy** each time, because the receiver frees
    /// what it is given and a drop target may ask twice.
    #[test]
    fn get_data_hands_out_a_fresh_handle_every_time() {
        let d = new_data_object(vec![(13, b"payload".to_vec())]);
        let fmt = FORMATETC {
            cfFormat: 13,
            ptd: std::ptr::null_mut(),
            dwAspect: DVASPECT_CONTENT,
            lindex: -1,
            tymed: TYMED_HGLOBAL,
        };
        let mut a = STGMEDIUM::default();
        let mut b = STGMEDIUM::default();
        // SAFETY: `d` is ours, and both mediums are writable.
        unsafe {
            assert_eq!(do_get_data(d, &fmt, &mut a), S_OK);
            assert_eq!(do_get_data(d, &fmt, &mut b), S_OK);
            assert_ne!(
                a.u.hGlobal, b.u.hGlobal,
                "the same handle twice would be a double free"
            );
            assert_eq!(a.tymed, TYMED_HGLOBAL);
            assert!(a.pUnkForRelease.is_null(), "the receiver owns it");
            // The bytes really are the payload.
            let p = GlobalLock(a.u.hGlobal);
            assert_eq!(std::slice::from_raw_parts(p.cast::<u8>(), 7), b"payload");
            GlobalUnlock(a.u.hGlobal);
            GlobalFree(a.u.hGlobal);
            GlobalFree(b.u.hGlobal);
            // A format it does not hold, and an aspect it does not serve.
            let mut c = STGMEDIUM::default();
            let other = FORMATETC { cfFormat: 99, ..fmt };
            assert_eq!(do_get_data(d, &other, &mut c), DV_E_FORMATETC);
            let bad_tymed = FORMATETC { tymed: 0, ..fmt };
            assert_eq!(do_query_get_data(d, &bad_tymed), DV_E_FORMATETC);
            assert_eq!(do_query_get_data(d, &fmt), S_OK);
            assert_eq!(do_release(d), 0);
        }
    }

    // --- the drop target, §42 ----------------------------------------------
    //
    // The same four kinds of silent wrongness the source's tests cover, from
    // the other side of the interface: the vtable order, the out-pointer
    // discipline, the ref count, and the one number OLE reads back.

    /// Call through the vtable **by position**, the way OLE does.
    ///
    /// This is the point of the test rather than an implementation detail: a
    /// vtable is a struct of function pointers whose order *is* the contract,
    /// and a Rust struct field named `drop_` that happens to sit where OLE
    /// expects `DragLeave` compiles perfectly and calls the wrong function at
    /// runtime. Indexing the raw layout is the only way to assert the order.
    unsafe fn dt_slot(t: *mut DropTarget, index: usize) -> *const () {
        // SAFETY: a live `DropTarget`; its first word is the vtable pointer,
        // and the vtable is `IDropTargetVtbl`'s seven function pointers.
        unsafe {
            let vtbl = *(t as *const *const ()) as *const *const ();
            *vtbl.add(index)
        }
    }

    #[test]
    fn the_drop_target_vtable_is_in_idl_order() {
        let t = Box::into_raw(Box::new(DropTarget {
            vtbl: &DROP_TARGET_VTBL,
            refs: Cell::new(1),
            window: 7,
        }));
        // SAFETY: the object is alive for the whole test.
        unsafe {
            // IUnknown's three, then IDropTarget's four, in IDL order.
            assert_eq!(dt_slot(t, 0), dt_query_interface as *const ());
            assert_eq!(dt_slot(t, 1), dt_add_ref as *const ());
            assert_eq!(dt_slot(t, 2), dt_release as *const ());
            assert_eq!(dt_slot(t, 3), dt_drag_enter as *const ());
            assert_eq!(dt_slot(t, 4), dt_drag_over as *const ());
            assert_eq!(dt_slot(t, 5), dt_drag_leave as *const ());
            assert_eq!(dt_slot(t, 6), dt_drop as *const ());
            assert_eq!(dt_release(t), 0);
        }
    }

    #[test]
    fn the_drop_target_answers_for_idroptarget_and_nulls_the_out_pointer_when_it_refuses() {
        let t = Box::into_raw(Box::new(DropTarget {
            vtbl: &DROP_TARGET_VTBL,
            refs: Cell::new(1),
            window: 1,
        }));
        // SAFETY: the object is alive for the whole test.
        unsafe {
            let mut out: *mut core::ffi::c_void = std::ptr::null_mut();
            assert_eq!(dt_query_interface(t, &IID_IDROPTARGET, &mut out), S_OK);
            assert!(!out.is_null(), "a successful QueryInterface must hand one back");
            // It took a reference; give it back.
            assert_eq!(dt_release(t), 1);

            // A refusal still writes the out-pointer, because OLE reads it
            // either way.
            out = 0x1234 as *mut core::ffi::c_void;
            assert_eq!(dt_query_interface(t, &IID_IDROPSOURCE, &mut out), E_NOINTERFACE);
            assert!(out.is_null(), "a refusal must null the out-pointer, not leave it");

            assert_eq!(dt_query_interface(t, &IID_IUNKNOWN, &mut out), S_OK);
            assert_eq!(dt_release(t), 1);
            assert_eq!(dt_release(t), 0);
        }
    }

    /// `DragOver` answers from the last feedback, and the default is "no".
    ///
    /// The default is the half worth asserting: a drop target that answered
    /// `DROPEFFECT_COPY` before any client had accepted would show a copy
    /// cursor over a window that is going to refuse the drop, which is a
    /// promise the compositor cannot keep.
    #[test]
    fn drag_over_answers_from_the_last_feedback_and_refuses_until_there_is_one() {
        let t = Box::into_raw(Box::new(DropTarget {
            vtbl: &DROP_TARGET_VTBL,
            refs: Cell::new(1),
            window: 3,
        }));
        let pt = PointL { x: 0, y: 0 };
        // SAFETY: the object is alive for the whole test.
        unsafe {
            FEEDBACK.with(|f| f.set(DROPEFFECT_NONE));
            let mut effect: DROPEFFECT = 0xdead_beef;
            assert_eq!(dt_drag_over(t, 0, pt, &mut effect), S_OK);
            assert_eq!(effect, DROPEFFECT_NONE, "nobody has accepted yet");

            feedback(true, dnd_action::COPY);
            assert_eq!(dt_drag_over(t, 0, pt, &mut effect), S_OK);
            assert_eq!(effect, DROPEFFECT_COPY);

            feedback(true, dnd_action::MOVE);
            assert_eq!(dt_drag_over(t, 0, pt, &mut effect), S_OK);
            assert_eq!(effect, DROPEFFECT_MOVE);

            // A client that stops accepting takes the effect with it.
            feedback(false, dnd_action::COPY);
            assert_eq!(dt_drag_over(t, 0, pt, &mut effect), S_OK);
            assert_eq!(effect, DROPEFFECT_NONE);

            // And a null out-pointer is survivable rather than a crash: OLE is
            // not supposed to pass one, and "not supposed to" is not a
            // guarantee this process gets to rely on.
            assert_eq!(dt_drag_over(t, 0, pt, std::ptr::null_mut()), S_OK);
            assert_eq!(dt_release(t), 0);
        }
    }

    /// A drag carrying nothing we understand is refused, and does not crash.
    ///
    /// A null `IDataObject` is not something OLE does; it is what a *harness*
    /// can pass, and the assertion is that the four methods are total
    /// functions of what they are given rather than of what they expect.
    #[test]
    fn a_drag_with_no_data_object_is_refused_rather_than_dereferenced() {
        let t = Box::into_raw(Box::new(DropTarget {
            vtbl: &DROP_TARGET_VTBL,
            refs: Cell::new(1),
            window: 5,
        }));
        let pt = PointL { x: 10, y: 20 };
        // SAFETY: the object is alive; the data object is deliberately null.
        unsafe {
            let mut effect: DROPEFFECT = DROPEFFECT_COPY;
            assert_eq!(dt_drag_enter(t, std::ptr::null_mut(), 0, pt, &mut effect), S_OK);
            assert_eq!(effect, DROPEFFECT_NONE);
            assert!(formats_of(std::ptr::null_mut()).is_empty());
            assert!(read_all(std::ptr::null_mut()).is_empty());
            assert_eq!(dt_drag_leave(t), S_OK);
            assert_eq!(dt_release(t), 0);
        }
    }

    /// What was dropped comes back as the mime type the client asks for.
    ///
    /// The conversion is `crate::clipboard::from_system`, which is the same
    /// function the clipboard's paste path uses — so this asserts the wiring
    /// rather than the encoding, which has its own tests. UTF-16 in, UTF-8
    /// out, and a mime type the drag never held answers `None` rather than the
    /// wrong bytes.
    #[test]
    fn the_dropped_bytes_come_back_as_the_mime_the_client_asked_for() {
        let utf16 = crate::clipboard::utf16_bytes(&crate::clipboard::utf16_from_utf8("hi\r\n"));
        DROPPED.with(|d| {
            *d.borrow_mut() = vec![(crate::clipboard::SystemFormat::UnicodeText, utf16)]
        });
        // `payload` takes a `Ui` it does not read; the drop's bytes are
        // thread-local because the drop is.
        let got = DROPPED.with(|d| {
            d.borrow()
                .iter()
                .find_map(|(f, b)| crate::clipboard::from_system("text/plain;charset=utf-8", *f, b))
        });
        assert_eq!(got.as_deref(), Some(&b"hi\n"[..]));
        let none = DROPPED.with(|d| {
            d.borrow()
                .iter()
                .find_map(|(f, b)| crate::clipboard::from_system("image/png", *f, b))
        });
        assert_eq!(none, None, "a mime the drag never held must not answer");
        DROPPED.with(|d| d.borrow_mut().clear());
    }
}

/// The client under an incoming drag has accepted or refused, and chosen an
/// action. Remember it for the next `IDropTarget::DragOver`.
///
/// ADDED FOR §42, and this function is the whole of the asynchronous half of
/// that answer. Its cost is argued at [`crate::platform::ToUi::DragFeedback`]:
/// `DragOver` returns whatever arrived *before* it was called, so the cursor is
/// up to one round trip behind the client. The drop is not, because the drop is
/// answered after the client has been told about it.
pub(crate) fn feedback(accepted: bool, action: u32) {
    let effect = if accepted { to_dropeffect(action) } else { DROPEFFECT_NONE };
    FEEDBACK.with(|f| f.set(effect));
}
