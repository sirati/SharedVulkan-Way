//! Windows guest RAM lent to the host: large-page section objects
//! (waycompose-design.md §108.7).
//!
//! A region is one pagefile-backed section created with `SEC_LARGE_PAGES`:
//! every page of it is a 2 MiB large page, non-pageable, physically
//! contiguous and 2 MiB aligned. It is never trimmed and never moved (§84.5
//! measured 0 of 16384 pages changing frame on large pages, where
//! `VirtualLock`ed 4 KiB pages moved in bursts). Unlike AWE frames or a private
//! `VirtualAlloc(MEM_LARGE_PAGES)`, a section lives as long as **any** handle
//! or view of it, in any process. That is what lets a keeper hold the memory
//! after its user has crashed (§108.10), which is rule 3 of §108.4.
//!
//! Creating one needs `SeLockMemoryPrivilege` in the creating process. LocalSystem
//! holds it, so the keeper is a SYSTEM service and the user's account needs
//! no new right (`security-model.md` R43).

use crate::{Error, Result};
use core::ffi::c_void;

mod sys {
    use core::ffi::c_void;
    pub const INVALID_HANDLE_VALUE: isize = -1;
    pub const PAGE_READWRITE: u32 = 0x04;
    pub const SEC_COMMIT: u32 = 0x0800_0000;
    pub const SEC_LARGE_PAGES: u32 = 0x8000_0000;
    pub const FILE_MAP_ALL_ACCESS: u32 = 0x000F_001F;
    pub const FILE_MAP_LARGE_PAGES: u32 = 0x2000_0000;
    pub const ERROR_ALREADY_EXISTS: u32 = 183;
    pub const TOKEN_ADJUST_PRIVILEGES: u32 = 0x0020;
    pub const TOKEN_QUERY: u32 = 0x0008;
    pub const SE_PRIVILEGE_ENABLED: u32 = 0x0000_0002;
    pub const SDDL_REVISION_1: u32 = 1;

    #[repr(C)]
    pub struct LUID {
        pub low: u32,
        pub high: i32,
    }
    #[repr(C)]
    pub struct TOKEN_PRIVILEGES {
        pub count: u32,
        pub luid: LUID,
        pub attrs: u32,
    }
    #[repr(C)]
    pub struct SECURITY_ATTRIBUTES {
        pub len: u32,
        pub sd: *mut c_void,
        pub inherit: i32,
    }
    #[repr(C)]
    #[derive(Default)]
    pub struct MEMORYSTATUSEX {
        pub len: u32,
        pub load: u32,
        pub total_phys: u64,
        pub avail_phys: u64,
        pub total_pagefile: u64,
        pub avail_pagefile: u64,
        pub total_virtual: u64,
        pub avail_virtual: u64,
        pub avail_ext_virtual: u64,
    }

    #[link(name = "kernel32")]
    unsafe extern "system" {
        pub fn CreateFileMappingW(file: isize, attrs: *mut c_void, protect: u32, hi: u32, lo: u32, name: *const u16) -> isize;
        pub fn OpenFileMappingW(access: u32, inherit: i32, name: *const u16) -> isize;
        pub fn MapViewOfFile(mapping: isize, access: u32, hi: u32, lo: u32, len: usize) -> *mut c_void;
        pub fn UnmapViewOfFile(base: *const c_void) -> i32;
        pub fn CloseHandle(h: isize) -> i32;
        pub fn GetLastError() -> u32;
        pub fn GetCurrentProcess() -> isize;
        pub fn GetLargePageMinimum() -> usize;
        pub fn GlobalMemoryStatusEx(st: *mut MEMORYSTATUSEX) -> i32;
        pub fn LocalFree(p: *mut c_void) -> *mut c_void;
    }
    #[link(name = "advapi32")]
    unsafe extern "system" {
        pub fn OpenProcessToken(p: isize, access: u32, tok: *mut isize) -> i32;
        pub fn LookupPrivilegeValueW(sys: *const u16, name: *const u16, luid: *mut LUID) -> i32;
        pub fn AdjustTokenPrivileges(tok: isize, disable: i32, new: *const TOKEN_PRIVILEGES, len: u32, prev: *mut c_void, ret: *mut u32) -> i32;
        pub fn ConvertStringSecurityDescriptorToSecurityDescriptorW(s: *const u16, rev: u32, sd: *mut *mut c_void, len: *mut u32) -> i32;
    }
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(core::iter::once(0)).collect()
}

fn os(call: &'static str) -> Error {
    Error::Os { call, errno: unsafe { sys::GetLastError() } as i32 }
}

/// Enable `SeLockMemoryPrivilege` in this process's token. `Ok(false)`: the
/// token does not hold it (`ERROR_NOT_ALL_ASSIGNED`).
pub fn enable_lock_memory() -> Result<bool> {
    unsafe {
        let mut tok = 0isize;
        if sys::OpenProcessToken(sys::GetCurrentProcess(), sys::TOKEN_ADJUST_PRIVILEGES | sys::TOKEN_QUERY, &mut tok) == 0 {
            return Err(os("OpenProcessToken"));
        }
        let mut luid = sys::LUID { low: 0, high: 0 };
        let name = wide("SeLockMemoryPrivilege");
        if sys::LookupPrivilegeValueW(core::ptr::null(), name.as_ptr(), &mut luid) == 0 {
            sys::CloseHandle(tok);
            return Err(os("LookupPrivilegeValueW"));
        }
        let tp = sys::TOKEN_PRIVILEGES { count: 1, luid, attrs: sys::SE_PRIVILEGE_ENABLED };
        let ok = sys::AdjustTokenPrivileges(tok, 0, &tp, 0, core::ptr::null_mut(), core::ptr::null_mut());
        let err = sys::GetLastError();
        sys::CloseHandle(tok);
        if ok == 0 {
            return Err(Error::Os { call: "AdjustTokenPrivileges", errno: err as i32 });
        }
        Ok(err != 1300)
    }
}

/// `GetLargePageMinimum()`: 2 MiB on x86-64, 0 if large pages are unsupported.
pub fn large_page_minimum() -> usize {
    unsafe { sys::GetLargePageMinimum() }
}

/// Physical memory as Windows reports it: `(total, available)` bytes.
pub fn memory_status() -> (u64, u64) {
    let mut st = sys::MEMORYSTATUSEX { len: core::mem::size_of::<sys::MEMORYSTATUSEX>() as u32, ..Default::default() };
    unsafe { sys::GlobalMemoryStatusEx(&mut st) };
    (st.total_phys, st.avail_phys)
}

/// A large-page section: the handle only. Holding one keeps every page of the
/// section allocated, whether or not anything maps it.
pub struct LargeSection {
    handle: isize,
    len: u64,
}

unsafe impl Send for LargeSection {}
unsafe impl Sync for LargeSection {}

impl LargeSection {
    /// Create a section of `len` bytes (a multiple of 2 MiB). `name`, if any,
    /// is the kernel object name (`Global\…` for a keeper in session 0);
    /// `sddl`, if any, its security descriptor.
    pub fn create(name: Option<&str>, len: u64, sddl: Option<&str>) -> Result<LargeSection> {
        let unit = large_page_minimum() as u64;
        if unit == 0 {
            return Err(Error::BadGeometry("this machine has no large pages"));
        }
        if len == 0 || !len.is_multiple_of(unit) {
            return Err(Error::BadGeometry("a large-page section is a whole number of large pages"));
        }
        let wname = name.map(wide);
        let mut sd: *mut c_void = core::ptr::null_mut();
        if let Some(s) = sddl {
            let ws = wide(s);
            if unsafe { sys::ConvertStringSecurityDescriptorToSecurityDescriptorW(ws.as_ptr(), sys::SDDL_REVISION_1, &mut sd, core::ptr::null_mut()) } == 0 {
                return Err(os("ConvertStringSecurityDescriptorToSecurityDescriptorW"));
            }
        }
        let mut sa = sys::SECURITY_ATTRIBUTES { len: core::mem::size_of::<sys::SECURITY_ATTRIBUTES>() as u32, sd, inherit: 0 };
        let handle = unsafe {
            sys::CreateFileMappingW(
                sys::INVALID_HANDLE_VALUE,
                if sd.is_null() { core::ptr::null_mut() } else { (&mut sa as *mut sys::SECURITY_ATTRIBUTES).cast() },
                sys::PAGE_READWRITE | sys::SEC_COMMIT | sys::SEC_LARGE_PAGES,
                (len >> 32) as u32,
                len as u32,
                wname.as_ref().map_or(core::ptr::null(), |w| w.as_ptr()),
            )
        };
        let err = unsafe { sys::GetLastError() };
        if !sd.is_null() {
            unsafe { sys::LocalFree(sd) };
        }
        if handle == 0 {
            return Err(Error::Os { call: "CreateFileMappingW(SEC_LARGE_PAGES)", errno: err as i32 });
        }
        if name.is_some() && err == sys::ERROR_ALREADY_EXISTS {
            unsafe { sys::CloseHandle(handle) };
            return Err(Error::Os { call: "CreateFileMappingW (name already exists)", errno: err as i32 });
        }
        Ok(LargeSection { handle, len })
    }

    /// Open a section another process created, by name.
    pub fn open(name: &str, len: u64) -> Result<LargeSection> {
        let w = wide(name);
        let handle = unsafe { sys::OpenFileMappingW(sys::FILE_MAP_ALL_ACCESS, 0, w.as_ptr()) };
        if handle == 0 {
            return Err(os("OpenFileMappingW"));
        }
        Ok(LargeSection { handle, len })
    }

    pub fn len(&self) -> u64 {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Map the whole section. Tries `FILE_MAP_LARGE_PAGES` first (2 MiB
    /// translations in this process) and falls back to a plain view, which
    /// maps the same large physical pages with 4 KiB translations.
    pub fn map(&self) -> Result<LargeView> {
        let len = usize::try_from(self.len).map_err(|_| Error::BadGeometry("too large for usize"))?;
        let mut large = true;
        let mut base = unsafe { sys::MapViewOfFile(self.handle, sys::FILE_MAP_ALL_ACCESS | sys::FILE_MAP_LARGE_PAGES, 0, 0, len) };
        if base.is_null() {
            large = false;
            base = unsafe { sys::MapViewOfFile(self.handle, sys::FILE_MAP_ALL_ACCESS, 0, 0, len) };
        }
        if base.is_null() {
            return Err(os("MapViewOfFile"));
        }
        Ok(LargeView { base: base as *mut u8, len, large_translations: large })
    }
}

impl Drop for LargeSection {
    fn drop(&mut self) {
        unsafe { sys::CloseHandle(self.handle) };
    }
}

/// One mapped view of a [`LargeSection`].
pub struct LargeView {
    base: *mut u8,
    len: usize,
    /// Whether `FILE_MAP_LARGE_PAGES` was granted for this view.
    pub large_translations: bool,
}

unsafe impl Send for LargeView {}
unsafe impl Sync for LargeView {}

impl LargeView {
    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// # Safety
    /// The pointer is valid while the view lives; its bytes are shared with
    /// the host, so only raw or atomic access is sound.
    pub unsafe fn base_ptr(&self) -> *mut u8 {
        self.base
    }

    /// Write the §108.8 stamp at the start of every 2 MiB unit.
    pub fn stamp_units(&self, nonce: &[u8; 16]) {
        let units = (self.len as u64 / crate::lpstamp::UNIT) as u32;
        for i in 0..units {
            let s = crate::lpstamp::stamp(nonce, i, units);
            unsafe {
                core::ptr::copy_nonoverlapping(s.as_ptr(), self.base.add(i as usize * crate::lpstamp::UNIT as usize), s.len());
            }
        }
        core::sync::atomic::fence(core::sync::atomic::Ordering::Release);
    }
}

impl Drop for LargeView {
    fn drop(&mut self) {
        unsafe { sys::UnmapViewOfFile(self.base as *const c_void) };
    }
}
