//! Host to Linux, by type: write and remove a Linux application's
//! registration (`wc_hostint::assoc::windows_plan`) under HKEY_CURRENT_USER
//! only. What is written is listed in a manifest (`assoc::manifest`) that
//! uninstall reads back through `assoc::parse_manifest`, which accepts only
//! keys the plan could have written. After either, the shell is told
//! (`SHChangeNotify(SHCNE_ASSOCCHANGED)`) so Explorer's menus follow at once.

use windows_sys::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_SUCCESS};
use windows_sys::Win32::System::Registry::{
    HKEY, HKEY_CLASSES_ROOT, HKEY_CURRENT_USER, KEY_READ, KEY_SET_VALUE, KEY_WRITE, REG_NONE, REG_OPTION_NON_VOLATILE,
    REG_SZ, RegCloseKey, RegCreateKeyExW, RegDeleteKeyValueW, RegDeleteKeyW, RegDeleteTreeW, RegOpenKeyExW,
    RegQueryInfoKeyW, RegQueryValueExW,
    RegSetValueExW,
};
use windows_sys::Win32::UI::Shell::{SHCNE_ASSOCCHANGED, SHCNF_IDLIST, SHChangeNotify};

use wc_hostint::assoc::{RegData, RegPlan, RegUndo};

use super::com::wide;

struct Key(HKEY);
impl Drop for Key {
    fn drop(&mut self) {
        // SAFETY: a key this module opened.
        unsafe { RegCloseKey(self.0) };
    }
}

fn create(path: &str) -> Result<Key, String> {
    let w = wide(path);
    let mut k: HKEY = std::ptr::null_mut();
    // SAFETY: NUL-terminated path, out pointer.
    let rc = unsafe {
        RegCreateKeyExW(
            HKEY_CURRENT_USER,
            w.as_ptr(),
            0,
            std::ptr::null(),
            REG_OPTION_NON_VOLATILE,
            KEY_WRITE | KEY_SET_VALUE,
            std::ptr::null(),
            &mut k,
            std::ptr::null_mut(),
        )
    };
    if rc != ERROR_SUCCESS {
        return Err(format!("RegCreateKeyExW(HKCU\\{path}): error {rc}"));
    }
    Ok(Key(k))
}

fn set(k: &Key, name: &str, data: &RegData) -> Result<(), String> {
    let n = wide(name);
    let (ty, bytes): (u32, Vec<u8>) = match data {
        RegData::Str(s) => (REG_SZ, wide(s).iter().flat_map(|c| c.to_le_bytes()).collect()),
        // Presence-only values. `URL Protocol` is conventionally an empty
        // string; an `OpenWithProgids` entry is a zero-length REG_NONE, as
        // the shell's own registrations write them.
        RegData::Empty if name == "URL Protocol" => (REG_SZ, vec![0, 0]),
        RegData::Empty => (REG_NONE, Vec::new()),
    };
    // SAFETY: a live key, NUL-terminated name, `bytes` of the stated type.
    let rc = unsafe { RegSetValueExW(k.0, n.as_ptr(), 0, ty, bytes.as_ptr(), bytes.len() as u32) };
    if rc != ERROR_SUCCESS {
        return Err(format!("RegSetValueExW({name:?}): error {rc}"));
    }
    Ok(())
}

/// Tell the shell that associations changed.
pub fn notify_shell() {
    // SAFETY: no items for SHCNE_ASSOCCHANGED.
    unsafe { SHChangeNotify(SHCNE_ASSOCCHANGED as i32, SHCNF_IDLIST, std::ptr::null(), std::ptr::null()) };
}

/// Write `plan` under HKCU. Stops at the first failure and returns it; the
/// caller writes the manifest first, so a half-written registration is
/// still removed by uninstall.
pub fn apply(plan: &RegPlan) -> Result<usize, String> {
    let mut n = 0;
    for v in &plan.values {
        let k = create(&v.key)?;
        set(&k, &v.name, &v.data)?;
        n += 1;
    }
    notify_shell();
    Ok(n)
}

/// Remove what a manifest lists. Missing keys and values are fine (removed
/// already); other failures are collected and returned.
pub fn remove(undo: &[RegUndo]) -> Result<usize, Vec<String>> {
    let mut errs = Vec::new();
    let mut n = 0;
    for u in undo {
        let rc = match u {
            RegUndo::Tree(k) => {
                let w = wide(k);
                // SAFETY: NUL-terminated subkey of HKCU; the key goes with
                // everything under it.
                unsafe { RegDeleteTreeW(HKEY_CURRENT_USER, w.as_ptr()) }
            }
            RegUndo::Value { key, name } => {
                let k = wide(key);
                let v = wide(name);
                // SAFETY: NUL-terminated key and value names.
                unsafe { RegDeleteKeyValueW(HKEY_CURRENT_USER, k.as_ptr(), v.as_ptr()) }
            }
        };
        if rc == ERROR_SUCCESS {
            n += 1;
        } else if rc != ERROR_FILE_NOT_FOUND {
            errs.push(format!("{u:?}: error {rc}"));
        }
    }
    // `Software\Waycompose\<install>` and `Software\Waycompose` are left
    // empty by the last application's uninstall: remove them then, and
    // only then (a key with any subkey or value is kept).
    for u in undo {
        if let RegUndo::Tree(k) = u
            && let Some(rest) = k.strip_prefix("Software\\Waycompose\\")
            && let Some((install, _)) = rest.split_once('\\')
        {
            prune_empty(&format!("Software\\Waycompose\\{install}"));
            prune_empty("Software\\Waycompose");
        }
    }
    notify_shell();
    if errs.is_empty() { Ok(n) } else { Err(errs) }
}

/// Delete HKCU\`path` if it has neither subkeys nor values.
fn prune_empty(path: &str) {
    let w = wide(path);
    let mut k: HKEY = std::ptr::null_mut();
    // SAFETY: NUL-terminated subkey, out pointer.
    if unsafe { RegOpenKeyExW(HKEY_CURRENT_USER, w.as_ptr(), 0, KEY_READ, &mut k) } != ERROR_SUCCESS {
        return;
    }
    let (mut subkeys, mut values) = (1u32, 1u32);
    {
        let k = Key(k);
        let n = std::ptr::null_mut();
        // SAFETY: counts only; every other out pointer is null.
        unsafe {
            RegQueryInfoKeyW(k.0, n, n.cast(), std::ptr::null(), &mut subkeys, n.cast(), n.cast(), &mut values, n.cast(), n.cast(), n.cast(), n.cast())
        };
    }
    if subkeys == 0 && values == 0 {
        // SAFETY: an empty key of ours.
        unsafe { RegDeleteKeyW(HKEY_CURRENT_USER, w.as_ptr()) };
    }
}

/// Whether the system already defines URL scheme `s`: `HKCR\<s>` with a
/// `URL Protocol` value (HKCR merges the machine's and the user's).
pub fn scheme_exists(s: &str) -> bool {
    let w = wide(s);
    let mut k: HKEY = std::ptr::null_mut();
    // SAFETY: NUL-terminated subkey, out pointer.
    if unsafe { RegOpenKeyExW(HKEY_CLASSES_ROOT, w.as_ptr(), 0, KEY_READ, &mut k) } != ERROR_SUCCESS {
        return false;
    }
    let k = Key(k);
    let v = wide("URL Protocol");
    // SAFETY: a presence query; no data buffer.
    let rc = unsafe {
        RegQueryValueExW(k.0, v.as_ptr(), std::ptr::null(), std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null_mut())
    };
    rc == ERROR_SUCCESS
}

/// Read one HKCU string value back, for checks. `None` if absent.
pub fn read(key: &str, name: &str) -> Option<String> {
    let w = wide(key);
    let mut k: HKEY = std::ptr::null_mut();
    // SAFETY: NUL-terminated subkey, out pointer.
    if unsafe { RegOpenKeyExW(HKEY_CURRENT_USER, w.as_ptr(), 0, KEY_READ, &mut k) } != ERROR_SUCCESS {
        return None;
    }
    let k = Key(k);
    let n = wide(name);
    let mut buf = vec![0u16; 2048];
    let mut bytes = (buf.len() * 2) as u32;
    let mut ty = 0u32;
    // SAFETY: a buffer of `bytes` bytes.
    let rc = unsafe { RegQueryValueExW(k.0, n.as_ptr(), std::ptr::null(), &mut ty, buf.as_mut_ptr().cast(), &mut bytes) };
    (rc == ERROR_SUCCESS).then(|| super::com::from_wide(&buf[..(bytes as usize / 2).min(buf.len())]))
}

/// Whether HKCU\`key` exists.
pub fn key_exists(key: &str) -> bool {
    let w = wide(key);
    let mut k: HKEY = std::ptr::null_mut();
    // SAFETY: NUL-terminated subkey, out pointer.
    let ok = unsafe { RegOpenKeyExW(HKEY_CURRENT_USER, w.as_ptr(), 0, KEY_READ, &mut k) } == ERROR_SUCCESS;
    if ok {
        drop(Key(k));
    }
    ok
}
