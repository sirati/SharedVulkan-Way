//! §92. The Windows shell's half of a pinnable Linux application: shortcuts
//! that carry the application's identity, and its jump list.
//!
//! # Hand-written COM, as in the compositor
//!
//! `windows-sys` generates functions and structs and no COM interfaces, so the
//! five interfaces used here are declared from their headers, each vtable in
//! the header's own order: `IShellLinkW` (shobjidl_core.h), `IPersistFile`
//! (objidl.h), `IPropertyStore` (propsys.h), `ICustomDestinationList`, and
//! `IObjectArray` / `IObjectCollection` (objectarray.h). The compositor's
//! `platform/win32.rs` and `platform/drag.rs` do the same for theirs. A slot in
//! the wrong place is a call through the wrong function pointer, which is why
//! every declaration below lists every method, used or not.
//!
//! # Why not PowerShell, as the desktop shortcut used to be
//!
//! `WScript.Shell`'s shortcut object cannot set `System.AppUserModel.ID`, and
//! without that property a pinned shortcut and the application's windows are
//! two buttons (§92.2). And a jump list has no scripting interface at all.

use std::ffi::c_void;
use std::path::{Path, PathBuf};

use windows_sys::core::{GUID, HRESULT};

// ------------------------------------------------------------------ GUIDs ---

const CLSID_SHELL_LINK: GUID = GUID::from_u128(0x00021401_0000_0000_c000_000000000046);
const IID_SHELL_LINK_W: GUID = GUID::from_u128(0x000214f9_0000_0000_c000_000000000046);
const IID_PERSIST_FILE: GUID = GUID::from_u128(0x0000010b_0000_0000_c000_000000000046);
const IID_PROPERTY_STORE: GUID = GUID::from_u128(0x886d8eeb_8cf2_4446_8d02_cdba1dbdcf99);
const CLSID_DESTINATION_LIST: GUID = GUID::from_u128(0x77f10cf0_3db5_4966_b520_b7c54fd35ed6);
const IID_CUSTOM_DESTINATION_LIST: GUID = GUID::from_u128(0x6332debf_87b5_4670_90c0_5e57b408a49e);
const CLSID_ENUMERABLE_OBJECT_COLLECTION: GUID =
    GUID::from_u128(0x2d3468c1_36a7_43b6_ac24_d3f02fd9607a);
const IID_OBJECT_COLLECTION: GUID = GUID::from_u128(0x5632b1a4_e38a_400a_928a_d4cd63230295);
const IID_OBJECT_ARRAY: GUID = GUID::from_u128(0x92ca9dcd_5622_4bba_a805_5e9f541bd8c9);

/// `FOLDERID_Programs`: the user's Start menu programs folder.
pub const FOLDER_PROGRAMS: GUID = GUID::from_u128(0xa77f5d77_2e2b_44c3_a6a2_aba601054a51);
/// `FOLDERID_Desktop`, which on this guest is redirected into OneDrive.
pub const FOLDER_DESKTOP: GUID = GUID::from_u128(0xb4bfcc3a_db2c_424c_b029_7fe99a87c641);

#[repr(C)]
struct PropertyKey {
    fmtid: GUID,
    pid: u32,
}

/// `PKEY_Title`: a jump-list link's text. Windows shows no link without one.
const PKEY_TITLE: PropertyKey =
    PropertyKey { fmtid: GUID::from_u128(0xf29f85e0_4ff9_1068_ab91_08002b27b3d9), pid: 2 };
/// `PKEY_AppUserModel_ID`, the same key the compositor sets on its windows.
const PKEY_APP_USER_MODEL_ID: PropertyKey =
    PropertyKey { fmtid: GUID::from_u128(0x9f4c2855_9f79_4b39_a8d0_e1d42de1d5f3), pid: 5 };

/// `PROPVARIANT` for one `VT_LPWSTR`, borrowing the string: `SetValue`
/// copies, and this is never handed to `PropVariantClear`.
#[repr(C)]
struct PropVariant {
    vt: u16,
    reserved: [u16; 3],
    value: [u64; 2],
}
const _: () = assert!(std::mem::size_of::<PropVariant>() == 24);

// ------------------------------------------------------------- vtables ---

type Fp = unsafe extern "system" fn();

#[repr(C)]
struct UnknownVtbl {
    query_interface: unsafe extern "system" fn(*mut c_void, *const GUID, *mut *mut c_void) -> HRESULT,
    add_ref: unsafe extern "system" fn(*mut c_void) -> u32,
    release: unsafe extern "system" fn(*mut c_void) -> u32,
}

/// `IShellLinkW`, in shobjidl_core.h's order.
#[repr(C)]
struct ShellLinkVtbl {
    base: UnknownVtbl,
    get_path: unsafe extern "system" fn(*mut c_void, *mut u16, i32, *mut c_void, u32) -> HRESULT,
    get_id_list: Fp,
    set_id_list: Fp,
    get_description: Fp,
    set_description: unsafe extern "system" fn(*mut c_void, *const u16) -> HRESULT,
    get_working_directory: Fp,
    set_working_directory: unsafe extern "system" fn(*mut c_void, *const u16) -> HRESULT,
    get_arguments: unsafe extern "system" fn(*mut c_void, *mut u16, i32) -> HRESULT,
    set_arguments: unsafe extern "system" fn(*mut c_void, *const u16) -> HRESULT,
    get_hotkey: Fp,
    set_hotkey: Fp,
    get_show_cmd: Fp,
    set_show_cmd: Fp,
    get_icon_location: Fp,
    set_icon_location: unsafe extern "system" fn(*mut c_void, *const u16, i32) -> HRESULT,
    set_relative_path: Fp,
    resolve: Fp,
    set_path: unsafe extern "system" fn(*mut c_void, *const u16) -> HRESULT,
}

/// `IPersistFile`, objidl.h (after `IPersist::GetClassID`).
#[repr(C)]
struct PersistFileVtbl {
    base: UnknownVtbl,
    get_class_id: Fp,
    is_dirty: Fp,
    load: unsafe extern "system" fn(*mut c_void, *const u16, u32) -> HRESULT,
    save: unsafe extern "system" fn(*mut c_void, *const u16, i32) -> HRESULT,
    save_completed: Fp,
    get_cur_file: Fp,
}

/// `IPropertyStore`, propsys.h.
#[repr(C)]
struct PropertyStoreVtbl {
    base: UnknownVtbl,
    get_count: Fp,
    get_at: Fp,
    get_value: unsafe extern "system" fn(*mut c_void, *const PropertyKey, *mut PropVariant) -> HRESULT,
    set_value: unsafe extern "system" fn(*mut c_void, *const PropertyKey, *const PropVariant) -> HRESULT,
    commit: unsafe extern "system" fn(*mut c_void) -> HRESULT,
}

/// `ICustomDestinationList`, shobjidl_core.h.
#[repr(C)]
struct DestinationListVtbl {
    base: UnknownVtbl,
    set_app_id: unsafe extern "system" fn(*mut c_void, *const u16) -> HRESULT,
    begin_list: unsafe extern "system" fn(*mut c_void, *mut u32, *const GUID, *mut *mut c_void) -> HRESULT,
    append_category: unsafe extern "system" fn(*mut c_void, *const u16, *mut c_void) -> HRESULT,
    append_known_category: Fp,
    add_user_tasks: unsafe extern "system" fn(*mut c_void, *mut c_void) -> HRESULT,
    commit_list: unsafe extern "system" fn(*mut c_void) -> HRESULT,
    get_removed_destinations: Fp,
    delete_list: unsafe extern "system" fn(*mut c_void, *const u16) -> HRESULT,
    abort_list: unsafe extern "system" fn(*mut c_void) -> HRESULT,
}

/// `IObjectArray`, and `IObjectCollection` which extends it; objectarray.h.
#[repr(C)]
struct ObjectCollectionVtbl {
    base: UnknownVtbl,
    get_count: unsafe extern "system" fn(*mut c_void, *mut u32) -> HRESULT,
    get_at: unsafe extern "system" fn(*mut c_void, u32, *const GUID, *mut *mut c_void) -> HRESULT,
    add_object: unsafe extern "system" fn(*mut c_void, *mut c_void) -> HRESULT,
    add_from_array: Fp,
    remove_object_at: Fp,
    clear: Fp,
}

/// An owned interface pointer, released on drop.
struct Com(*mut c_void);

impl Com {
    /// # Safety
    /// `T` must be the vtable of the interface this pointer is.
    unsafe fn vtbl<T>(&self) -> &T {
        // SAFETY: a COM object's first field is its vtable pointer; the
        // caller names the right vtable type.
        unsafe { &**(self.0 as *const *const T) }
    }

    fn create(clsid: &GUID, iid: &GUID) -> Result<Com, String> {
        use windows_sys::Win32::System::Com::{CLSCTX_INPROC_SERVER, CoCreateInstance};
        let mut p: *mut c_void = std::ptr::null_mut();
        // SAFETY: valid GUIDs and an out pointer checked below.
        let hr = unsafe { CoCreateInstance(clsid, std::ptr::null_mut(), CLSCTX_INPROC_SERVER, iid, &mut p) };
        if hr < 0 || p.is_null() {
            return Err(format!("CoCreateInstance({}): 0x{:08x}", guid_str(clsid), hr as u32));
        }
        Ok(Com(p))
    }

    fn query(&self, iid: &GUID) -> Result<Com, String> {
        let mut p: *mut c_void = std::ptr::null_mut();
        // SAFETY: every interface begins with IUnknown.
        let hr = unsafe { (self.vtbl::<UnknownVtbl>().query_interface)(self.0, iid, &mut p) };
        if hr < 0 || p.is_null() {
            return Err(format!("QueryInterface({}): 0x{:08x}", guid_str(iid), hr as u32));
        }
        Ok(Com(p))
    }
}

impl Drop for Com {
    fn drop(&mut self) {
        // SAFETY: a pointer this program owns one reference to.
        unsafe { (self.vtbl::<UnknownVtbl>().release)(self.0) };
    }
}

fn guid_str(g: &GUID) -> String {
    format!("{:08x}-{:04x}-{:04x}", g.data1, g.data2, g.data3)
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn check(hr: HRESULT, what: &str) -> Result<(), String> {
    if hr < 0 { Err(format!("{what}: 0x{:08x}", hr as u32)) } else { Ok(()) }
}

/// Put this thread in a single-threaded apartment. Shell objects want one, and
/// this program has no other COM to disagree with.
pub fn init() {
    use windows_sys::Win32::System::Com::{COINIT_APARTMENTTHREADED, CoInitializeEx};
    // SAFETY: a plain call; S_FALSE (already initialised) is fine.
    unsafe { CoInitializeEx(std::ptr::null(), COINIT_APARTMENTTHREADED as u32) };
}

/// A known folder's path: the Start menu's programs, or the desktop.
pub fn known_folder(id: &GUID) -> Result<PathBuf, String> {
    use windows_sys::Win32::System::Com::CoTaskMemFree;
    use windows_sys::Win32::UI::Shell::SHGetKnownFolderPath;
    let mut p: *mut u16 = std::ptr::null_mut();
    // SAFETY: a valid folder id, no token (the current user), an out pointer.
    let hr = unsafe { SHGetKnownFolderPath(id, 0, std::ptr::null_mut(), &mut p) };
    if hr < 0 || p.is_null() {
        return Err(format!("SHGetKnownFolderPath({}): 0x{:08x}", guid_str(id), hr as u32));
    }
    // SAFETY: a NUL-terminated string the shell allocated, freed here.
    let s = unsafe {
        let mut n = 0;
        while *p.add(n) != 0 {
            n += 1;
        }
        let s = String::from_utf16_lossy(std::slice::from_raw_parts(p, n));
        CoTaskMemFree(p.cast());
        s
    };
    Ok(PathBuf::from(s))
}

/// One shortcut, or one jump-list entry: `target args`, wearing `icon`.
#[derive(Debug, Clone, Default)]
pub struct LinkSpec {
    pub target: String,
    pub args: String,
    pub workdir: String,
    pub description: String,
    pub icon: Option<String>,
    /// `System.AppUserModel.ID`: set on shortcuts, so a pin made from one
    /// lands on the button the application's windows use.
    pub aumid: Option<String>,
    /// `PKEY_Title`: set on jump-list entries.
    pub title: Option<String>,
}

fn set_props(link: &Com, props: &[(&PropertyKey, &str)]) -> Result<(), String> {
    if props.is_empty() {
        return Ok(());
    }
    let store = link.query(&IID_PROPERTY_STORE)?;
    // SAFETY: an IPropertyStore; every string outlives the call that copies it.
    unsafe {
        let v = store.vtbl::<PropertyStoreVtbl>();
        for (key, value) in props {
            let w = wide(value);
            let pv = PropVariant { vt: 31, reserved: [0; 3], value: [w.as_ptr() as u64, 0] };
            check((v.set_value)(store.0, *key, &pv), "IPropertyStore::SetValue")?;
        }
        check((v.commit)(store.0), "IPropertyStore::Commit")
    }
}

fn make_link(s: &LinkSpec) -> Result<Com, String> {
    let link = Com::create(&CLSID_SHELL_LINK, &IID_SHELL_LINK_W)?;
    // SAFETY: an IShellLinkW; every string outlives its call.
    unsafe {
        let v = link.vtbl::<ShellLinkVtbl>();
        check((v.set_path)(link.0, wide(&s.target).as_ptr()), "IShellLinkW::SetPath")?;
        check((v.set_arguments)(link.0, wide(&s.args).as_ptr()), "IShellLinkW::SetArguments")?;
        if !s.workdir.is_empty() {
            check((v.set_working_directory)(link.0, wide(&s.workdir).as_ptr()), "SetWorkingDirectory")?;
        }
        if !s.description.is_empty() {
            check((v.set_description)(link.0, wide(&s.description).as_ptr()), "SetDescription")?;
        }
        if let Some(i) = &s.icon {
            check((v.set_icon_location)(link.0, wide(i).as_ptr(), 0), "SetIconLocation")?;
        }
    }
    let mut props: Vec<(&PropertyKey, &str)> = Vec::new();
    if let Some(a) = &s.aumid {
        props.push((&PKEY_APP_USER_MODEL_ID, a));
    }
    if let Some(t) = &s.title {
        props.push((&PKEY_TITLE, t));
    }
    set_props(&link, &props)?;
    Ok(link)
}

/// Write a `.lnk`, replacing one of the same name.
pub fn save_link(s: &LinkSpec, path: &Path) -> Result<(), String> {
    let link = make_link(s)?;
    let file = link.query(&IID_PERSIST_FILE)?;
    let p = wide(&path.to_string_lossy());
    // SAFETY: an IPersistFile; `p` outlives the call.
    unsafe { check((file.vtbl::<PersistFileVtbl>().save)(file.0, p.as_ptr(), 1), "IPersistFile::Save") }
}

/// §92.12. An existing `.lnk`, loaded: for reading it, or for changing one
/// property and saving it back with everything else as it was.
fn load_link(path: &Path, write: bool) -> Result<(Com, Com), String> {
    // STGM_READ, STGM_READWRITE.
    let mode = if write { 2 } else { 0 };
    let link = Com::create(&CLSID_SHELL_LINK, &IID_SHELL_LINK_W)?;
    let file = link.query(&IID_PERSIST_FILE)?;
    let p = wide(&path.to_string_lossy());
    // SAFETY: an IPersistFile; `p` outlives the call.
    unsafe { check((file.vtbl::<PersistFileVtbl>().load)(file.0, p.as_ptr(), mode), "IPersistFile::Load")? };
    Ok((link, file))
}

/// §92.12. What a shortcut runs and the identity it carries (empty if none),
/// as Explorer would read them. The identity is `None` when the property
/// store cannot be read (Wine's returns `E_NOTIMPL`).
pub fn read_link(path: &Path) -> Result<(String, Option<String>), String> {
    use windows_sys::Win32::System::Com::CoTaskMemFree;
    let (link, _file) = load_link(path, false)?;
    let mut buf = vec![0u16; 1024];
    // SAFETY: an IShellLinkW; a buffer of the length given; no find data
    // (the parameter is optional) and SLGP_RAWPATH (4), the path as stored.
    let target = unsafe {
        let hr = (link.vtbl::<ShellLinkVtbl>().get_path)(link.0, buf.as_mut_ptr(), buf.len() as i32, std::ptr::null_mut(), 4);
        if hr < 0 {
            String::new()
        } else {
            let n = buf.iter().position(|c| *c == 0).unwrap_or(buf.len());
            String::from_utf16_lossy(&buf[..n])
        }
    };
    let Ok(store) = link.query(&IID_PROPERTY_STORE) else { return Ok((target, None)) };
    let mut pv = PropVariant { vt: 0, reserved: [0; 3], value: [0; 2] };
    // SAFETY: an IPropertyStore and a PROPVARIANT it fills. A `VT_LPWSTR`
    // (31) is a string the store allocated with the COM allocator, which is
    // what `PropVariantClear` would free it with; anything else is not an
    // identity and carries no allocation of a type this reads.
    let aumid = unsafe {
        if (store.vtbl::<PropertyStoreVtbl>().get_value)(store.0, &PKEY_APP_USER_MODEL_ID, &mut pv) < 0 {
            return Ok((target, None));
        }
        let p = pv.value[0] as *mut u16;
        if pv.vt == 31 && !p.is_null() {
            let mut n = 0;
            while *p.add(n) != 0 {
                n += 1;
            }
            let s = String::from_utf16_lossy(std::slice::from_raw_parts(p, n));
            CoTaskMemFree(p.cast());
            s
        } else {
            String::new()
        }
    };
    Ok((target, Some(aumid)))
}

/// §92.12. Give an existing shortcut another identity, keeping everything
/// else it has: the user may have renamed it, moved it or changed its icon.
pub fn set_link_aumid(path: &Path, aumid: &str) -> Result<(), String> {
    let (link, file) = load_link(path, true)?;
    set_props(&link, &[(&PKEY_APP_USER_MODEL_ID, aumid)])?;
    // SAFETY: an IPersistFile; a null name saves to the file it loaded.
    unsafe { check((file.vtbl::<PersistFileVtbl>().save)(file.0, std::ptr::null(), 1), "IPersistFile::Save") }
}

/// §92.12. Delete `aumid`'s jump list: an installation's removal, for its
/// own identity only.
pub fn delete_jump_list(aumid: &str) -> Result<(), String> {
    let list = Com::create(&CLSID_DESTINATION_LIST, &IID_CUSTOM_DESTINATION_LIST)?;
    // SAFETY: an ICustomDestinationList; the string outlives the call.
    unsafe { check((list.vtbl::<DestinationListVtbl>().delete_list)(list.0, wide(aumid).as_ptr()), "ICustomDestinationList::DeleteList") }
}

fn collection(links: &[LinkSpec]) -> Result<Com, String> {
    let c = Com::create(&CLSID_ENUMERABLE_OBJECT_COLLECTION, &IID_OBJECT_COLLECTION)?;
    for s in links {
        let l = make_link(s)?;
        // SAFETY: an IObjectCollection, adding an object it AddRefs.
        unsafe { check((c.vtbl::<ObjectCollectionVtbl>().add_object)(c.0, l.0), "IObjectCollection::AddObject")? };
    }
    c.query(&IID_OBJECT_ARRAY)
}

/// The arguments of every link the user removed from this identity's jump
/// list, as `BeginList` reports them.
fn removed_args(array: &Com) -> Vec<String> {
    let mut out = Vec::new();
    // SAFETY: an IObjectArray from BeginList.
    unsafe {
        let v = array.vtbl::<ObjectCollectionVtbl>();
        let mut n = 0u32;
        if (v.get_count)(array.0, &mut n) < 0 {
            return out;
        }
        for i in 0..n {
            let mut p: *mut c_void = std::ptr::null_mut();
            if (v.get_at)(array.0, i, &IID_SHELL_LINK_W, &mut p) < 0 || p.is_null() {
                continue;
            }
            let link = Com(p);
            let mut buf = vec![0u16; 2048];
            if (link.vtbl::<ShellLinkVtbl>().get_arguments)(link.0, buf.as_mut_ptr(), buf.len() as i32) >= 0 {
                let n = buf.iter().position(|c| *c == 0).unwrap_or(buf.len());
                out.push(String::from_utf16_lossy(&buf[..n]));
            }
        }
    }
    out
}

/// What a commit did, for the log. Nothing in it is a URL or a title.
#[derive(Debug, Default)]
pub struct Committed {
    pub categories: Vec<(String, usize)>,
    pub tasks: usize,
    pub slots: u32,
    pub warnings: Vec<String>,
}

/// Replace `aumid`'s jump list.
///
/// `build` is called with the arguments of every link the user removed from
/// the list, after `BeginList` and before anything is appended: Windows
/// refuses a category that puts a removed item back, and the user asked for it
/// to be gone. A category that is refused anyway — `E_ACCESSDENIED` when the
/// user has turned off "Show recently opened items in Jump Lists" — is
/// reported and skipped; the tasks are still committed.
pub fn commit_jump_list(
    aumid: &str,
    build: impl FnOnce(&[String]) -> (Vec<(String, Vec<LinkSpec>)>, Vec<LinkSpec>),
) -> Result<Committed, String> {
    let list = Com::create(&CLSID_DESTINATION_LIST, &IID_CUSTOM_DESTINATION_LIST)?;
    let mut done = Committed::default();
    // SAFETY: an ICustomDestinationList; strings outlive their calls; the
    // removed array is owned by `Com`.
    unsafe {
        let v = list.vtbl::<DestinationListVtbl>();
        check((v.set_app_id)(list.0, wide(aumid).as_ptr()), "ICustomDestinationList::SetAppID")?;
        let mut removed: *mut c_void = std::ptr::null_mut();
        check(
            (v.begin_list)(list.0, &mut done.slots, &IID_OBJECT_ARRAY, &mut removed),
            "ICustomDestinationList::BeginList",
        )?;
        let removed = if removed.is_null() { Vec::new() } else { removed_args(&Com(removed)) };
        let (categories, tasks) = build(&removed);
        let result = (|| {
            for (name, links) in &categories {
                if links.is_empty() {
                    continue;
                }
                let arr = collection(links)?;
                let hr = (v.append_category)(list.0, wide(name).as_ptr(), arr.0);
                if hr < 0 {
                    done.warnings.push(format!("AppendCategory: 0x{:08x}", hr as u32));
                } else {
                    done.categories.push((name.clone(), links.len()));
                }
            }
            if !tasks.is_empty() {
                let arr = collection(&tasks)?;
                check((v.add_user_tasks)(list.0, arr.0), "ICustomDestinationList::AddUserTasks")?;
                done.tasks = tasks.len();
            }
            check((v.commit_list)(list.0), "ICustomDestinationList::CommitList")
        })();
        if let Err(e) = result {
            (v.abort_list)(list.0);
            return Err(e);
        }
    }
    Ok(done)
}

/// The UI languages, most preferred first, `\0`-separated: `de-DE\0en-US`.
pub fn ui_languages() -> String {
    use windows_sys::Win32::Globalization::{GetUserPreferredUILanguages, MUI_LANGUAGE_NAME};
    let (mut count, mut len) = (0u32, 0u32);
    // SAFETY: the documented two-call pattern: size, then fill.
    unsafe {
        if GetUserPreferredUILanguages(MUI_LANGUAGE_NAME, &mut count, std::ptr::null_mut(), &mut len) == 0 {
            return String::new();
        }
        let mut buf = vec![0u16; len as usize];
        if GetUserPreferredUILanguages(MUI_LANGUAGE_NAME, &mut count, buf.as_mut_ptr(), &mut len) == 0 {
            return String::new();
        }
        String::from_utf16_lossy(&buf).trim_end_matches('\0').to_string()
    }
}

/// Whether the taskbar, and so the jump list, is dark: `SystemUsesLightTheme`
/// is 0. Light when the value is missing, which is Windows' own default.
pub fn taskbar_is_dark() -> bool {
    use windows_sys::Win32::System::Registry::{HKEY_CURRENT_USER, RRF_RT_REG_DWORD, RegGetValueW};
    let key = wide(r"Software\Microsoft\Windows\CurrentVersion\Themes\Personalize");
    let value = wide("SystemUsesLightTheme");
    let mut data = 1u32;
    let mut size = 4u32;
    // SAFETY: a DWORD-sized buffer and its size.
    let rc = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            key.as_ptr(),
            value.as_ptr(),
            RRF_RT_REG_DWORD,
            std::ptr::null_mut(),
            (&mut data as *mut u32).cast(),
            &mut size,
        )
    };
    rc == 0 && data == 0
}

/// Run `f` while holding a named mutex, so the owner's periodic refresh and a
/// click's refresh do not rebuild one jump list at the same time.
pub fn serialised<T>(name: &str, f: impl FnOnce() -> T) -> T {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{CreateMutexW, ReleaseMutex, WaitForSingleObject};
    let n = wide(name);
    // SAFETY: a null security descriptor and a NUL-terminated name.
    let h = unsafe { CreateMutexW(std::ptr::null(), 0, n.as_ptr()) };
    if h.is_null() {
        return f();
    }
    // SAFETY: our handle; 30 s is longer than any refresh, and a refresh that
    // waits that long goes ahead rather than never.
    // WAIT_ABANDONED (0x80) is ownership too: the last holder exited mid-refresh.
    let owned = matches!(unsafe { WaitForSingleObject(h, 30_000) }, 0 | 0x80);
    let out = f();
    // SAFETY: release only what was acquired; close our handle either way.
    unsafe {
        if owned {
            ReleaseMutex(h);
        }
        CloseHandle(h);
    }
    out
}

/// §102.14. The `PKEY_AppUserModel_ID` on another process's window: the
/// identity the compositor stamped on it (§92). `None` for no property, an
/// empty one, or a shell without window property stores (Wine's).
pub fn window_aumid(hwnd: windows_sys::Win32::Foundation::HWND) -> Option<String> {
    use windows_sys::Win32::System::Com::CoTaskMemFree;
    use windows_sys::Win32::UI::Shell::PropertiesSystem::SHGetPropertyStoreForWindow;
    const VT_LPWSTR: u16 = 31;
    let mut p: *mut c_void = std::ptr::null_mut();
    // SAFETY: a window handle from `EnumWindows` (a stale one fails the call);
    // the out pointer is checked.
    let hr = unsafe { SHGetPropertyStoreForWindow(hwnd, &IID_PROPERTY_STORE, &mut p) };
    if hr < 0 || p.is_null() {
        return None;
    }
    let store = Com(p);
    let mut pv = PropVariant { vt: 0, reserved: [0; 3], value: [0; 2] };
    // SAFETY: an IPropertyStore. The one type read is a `CoTaskMemAlloc`ed
    // string, freed here, which is what `PropVariantClear` would do for it.
    unsafe {
        let v = store.vtbl::<PropertyStoreVtbl>();
        if (v.get_value)(store.0, &PKEY_APP_USER_MODEL_ID, &mut pv) < 0 || pv.vt != VT_LPWSTR || pv.value[0] == 0 {
            return None;
        }
        let s = pv.value[0] as *mut u16;
        let mut n = 0usize;
        while *s.add(n) != 0 {
            n += 1;
        }
        let out = String::from_utf16_lossy(std::slice::from_raw_parts(s, n));
        CoTaskMemFree(s.cast());
        (!out.is_empty()).then_some(out)
    }
}
