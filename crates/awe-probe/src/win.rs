//! The Win32 surface this probe needs, declared by hand.
//!
//! Hand-written rather than pulled from `windows-sys` on purpose: the whole
//! probe is about a dozen calls, the dev shell builds offline, and a reader
//! checking whether we called `AllocateUserPhysicalPages` correctly should be
//! able to see the exact signature we used without chasing a crate version.

#![allow(non_camel_case_types, non_snake_case)]
// The type names are the SDK's, deliberately: a reader checking a signature
// against MSDN should see the same spelling.
#![allow(clippy::upper_case_acronyms)]

use std::ffi::c_void;

pub type BOOL = i32;
pub type HANDLE = *mut c_void;
pub type ULONG_PTR = usize;

pub const MEM_COMMIT: u32 = 0x0000_1000;
pub const MEM_RESERVE: u32 = 0x0000_2000;
pub const MEM_RELEASE: u32 = 0x0000_8000;
pub const MEM_PHYSICAL: u32 = 0x0040_0000;
pub const MEM_LARGE_PAGES: u32 = 0x2000_0000;
pub const PAGE_READWRITE: u32 = 0x04;

/// `MEM_EXTENDED_PARAMETER_TYPE::MemExtendedParameterAttributeFlags`.
pub const MEM_EXTENDED_PARAMETER_ATTRIBUTE_FLAGS: u64 = 5;
/// Allocate the physical pages as large pages — the one flag that would give
/// AWE both frame numbers *and* 2 MiB contiguity.
pub const MEM_EXTENDED_PARAMETER_NONPAGED_LARGE: u64 = 0x0000_0008;

/// `MEM_EXTENDED_PARAMETER`: an 8-bit type in the low bits of the first word,
/// 56 reserved bits above it, then a union of pointer-sized values. Declared as
/// two `u64`s because Rust has no bitfields and the layout is what matters.
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct MEM_EXTENDED_PARAMETER {
    pub type_and_reserved: u64,
    pub value: u64,
}

impl MEM_EXTENDED_PARAMETER {
    pub fn attribute_flags(flags: u64) -> Self {
        MEM_EXTENDED_PARAMETER {
            type_and_reserved: MEM_EXTENDED_PARAMETER_ATTRIBUTE_FLAGS & 0xff,
            value: flags,
        }
    }
}

pub const TOKEN_ADJUST_PRIVILEGES: u32 = 0x0020;
pub const TOKEN_QUERY: u32 = 0x0008;
pub const SE_PRIVILEGE_ENABLED: u32 = 0x0000_0002;

pub const ERROR_NOT_ALL_ASSIGNED: u32 = 1300;
pub const ERROR_PRIVILEGE_NOT_HELD: u32 = 1314;

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct LUID {
    pub LowPart: u32,
    pub HighPart: i32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct LUID_AND_ATTRIBUTES {
    pub Luid: LUID,
    pub Attributes: u32,
}

#[repr(C)]
pub struct TOKEN_PRIVILEGES {
    pub PrivilegeCount: u32,
    pub Privileges: [LUID_AND_ATTRIBUTES; 1],
}

unsafe extern "system" {
    pub fn GetLastError() -> u32;
    pub fn GetCurrentProcess() -> HANDLE;

    pub fn VirtualAlloc(
        lpAddress: *mut c_void,
        dwSize: usize,
        flAllocationType: u32,
        flProtect: u32,
    ) -> *mut c_void;
    pub fn VirtualFree(lpAddress: *mut c_void, dwSize: usize, dwFreeType: u32) -> BOOL;
    pub fn VirtualLock(lpAddress: *mut c_void, dwSize: usize) -> BOOL;
    pub fn VirtualUnlock(lpAddress: *mut c_void, dwSize: usize) -> BOOL;
    pub fn GetProcessWorkingSetSize(
        hProcess: HANDLE,
        lpMinimumWorkingSetSize: *mut usize,
        lpMaximumWorkingSetSize: *mut usize,
    ) -> BOOL;
    pub fn SetProcessWorkingSetSize(
        hProcess: HANDLE,
        dwMinimumWorkingSetSize: usize,
        dwMaximumWorkingSetSize: usize,
    ) -> BOOL;

    // Address Windowing Extensions. `PageArray` is filled with physical frame
    // numbers — inside a VM, guest-physical ones, which is the entire point.
    pub fn AllocateUserPhysicalPages(
        hProcess: HANDLE,
        NumberOfPages: *mut ULONG_PTR,
        PageArray: *mut ULONG_PTR,
    ) -> BOOL;
    pub fn FreeUserPhysicalPages(
        hProcess: HANDLE,
        NumberOfPages: *mut ULONG_PTR,
        PageArray: *const ULONG_PTR,
    ) -> BOOL;
    pub fn MapUserPhysicalPages(
        VirtualAddress: *mut c_void,
        NumberOfPages: ULONG_PTR,
        PageArray: *const ULONG_PTR,
    ) -> BOOL;
    /// 2 MiB on x64, and the granularity every large-page allocation must be a
    /// multiple of.
    pub fn GetLargePageMinimum() -> usize;

    pub fn LoadLibraryA(lpLibFileName: *const u8) -> HANDLE;
    pub fn GetProcAddress(hModule: HANDLE, lpProcName: *const u8) -> *const c_void;

    pub fn CloseHandle(hObject: HANDLE) -> BOOL;
}

// The token calls live in advapi32, which mingw does not link by default.
#[link(name = "advapi32")]
unsafe extern "system" {
    pub fn OpenProcessToken(
        ProcessHandle: HANDLE,
        DesiredAccess: u32,
        TokenHandle: *mut HANDLE,
    ) -> BOOL;
    pub fn LookupPrivilegeValueW(
        lpSystemName: *const u16,
        lpName: *const u16,
        lpLuid: *mut LUID,
    ) -> BOOL;
    pub fn AdjustTokenPrivileges(
        TokenHandle: HANDLE,
        DisableAllPrivileges: BOOL,
        NewState: *const TOKEN_PRIVILEGES,
        BufferLength: u32,
        PreviousState: *mut TOKEN_PRIVILEGES,
        ReturnLength: *mut u32,
    ) -> BOOL;
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Try to enable a privilege in this process's token.
///
/// Returns `Ok(true)` if the privilege was enabled, `Ok(false)` if the token
/// does not hold it at all (`ERROR_NOT_ALL_ASSIGNED` — the answer when the
/// account has no "Lock pages in memory" user-right assignment), and `Err` if
/// the call machinery itself failed.
pub fn enable_privilege(name: &str) -> Result<bool, String> {
    unsafe {
        let mut token: HANDLE = std::ptr::null_mut();
        if OpenProcessToken(
            GetCurrentProcess(),
            TOKEN_ADJUST_PRIVILEGES | TOKEN_QUERY,
            &mut token,
        ) == 0
        {
            return Err(format!("OpenProcessToken failed: {}", GetLastError()));
        }
        let mut luid = LUID::default();
        let n = wide(name);
        if LookupPrivilegeValueW(std::ptr::null(), n.as_ptr(), &mut luid) == 0 {
            let e = GetLastError();
            CloseHandle(token);
            return Err(format!("LookupPrivilegeValue({name}) failed: {e}"));
        }
        let tp = TOKEN_PRIVILEGES {
            PrivilegeCount: 1,
            Privileges: [LUID_AND_ATTRIBUTES {
                Luid: luid,
                Attributes: SE_PRIVILEGE_ENABLED,
            }],
        };
        let ok = AdjustTokenPrivileges(
            token,
            0,
            &tp,
            std::mem::size_of::<TOKEN_PRIVILEGES>() as u32,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        );
        // AdjustTokenPrivileges succeeds even when it changed nothing; the
        // only honest test is GetLastError.
        let err = GetLastError();
        CloseHandle(token);
        if ok == 0 {
            return Err(format!("AdjustTokenPrivileges failed: {err}"));
        }
        Ok(err != ERROR_NOT_ALL_ASSIGNED)
    }
}

/// `AllocateUserPhysicalPages2`, resolved at run time.
///
/// It arrived in Windows 10 1803 and mingw's import library does not carry it,
/// so linking against it directly would fail to build rather than fail to run —
/// and "this Windows is too old" is an answer the probe should be able to
/// report rather than a link error. Signature:
///
/// ```c
/// BOOL AllocateUserPhysicalPages2(HANDLE, PULONG_PTR NumberOfPages,
///     PULONG_PTR PageArray, PMEM_EXTENDED_PARAMETER, ULONG ParameterCount);
/// ```
pub type PfnAllocateUserPhysicalPages2 = unsafe extern "system" fn(
    HANDLE,
    *mut ULONG_PTR,
    *mut ULONG_PTR,
    *mut MEM_EXTENDED_PARAMETER,
    u32,
) -> BOOL;

/// Where to look. Newer memory APIs are often exported by `kernelbase.dll` (or
/// only by the API set) and merely *forwarded* from `kernel32.dll` — so asking
/// kernel32 alone and concluding "not available" would be a wrong negative.
pub const ALLOC2_DLLS: [&std::ffi::CStr; 3] = [
    c"kernel32.dll",
    c"kernelbase.dll",
    c"api-ms-win-core-memory-l1-1-8.dll",
];

pub fn allocate_user_physical_pages2() -> Option<(PfnAllocateUserPhysicalPages2, &'static str)> {
    for (i, dll) in ALLOC2_DLLS.iter().enumerate() {
        unsafe {
            let lib = LoadLibraryA(dll.as_ptr() as *const u8);
            if lib.is_null() {
                continue;
            }
            let p = GetProcAddress(lib, c"AllocateUserPhysicalPages2".as_ptr() as *const u8);
            if !p.is_null() {
                let name = ["kernel32", "kernelbase", "api-ms-win-core-memory-l1-1-8"][i];
                return Some((
                    std::mem::transmute::<*const c_void, PfnAllocateUserPhysicalPages2>(p),
                    name,
                ));
            }
        }
    }
    None
}

// ---------------------------------------------------------------------------
// QueryWorkingSetEx
// ---------------------------------------------------------------------------

/// `PSAPI_WORKING_SET_EX_INFORMATION`: a virtual address in, a bitfield out.
///
/// The bitfield is a union of two layouts selected by `Valid`; we keep it as a
/// raw word and decode it ourselves, because the interesting question is
/// exactly what is and is not in those 64 bits.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct PSAPI_WORKING_SET_EX_INFORMATION {
    pub VirtualAddress: *mut c_void,
    pub VirtualAttributes: usize,
}

#[link(name = "psapi")]
unsafe extern "system" {
    pub fn QueryWorkingSetEx(hProcess: HANDLE, pv: *mut c_void, cb: u32) -> BOOL;
}

/// The `Valid == 1` layout of `PSAPI_WORKING_SET_EX_BLOCK`, decoded.
#[derive(Clone, Copy)]
pub struct WsBits(pub usize);

impl WsBits {
    pub fn valid(self) -> bool {
        self.0 & 1 != 0
    }
    pub fn share_count(self) -> usize {
        (self.0 >> 1) & 0x7
    }
    pub fn win32_protection(self) -> usize {
        (self.0 >> 4) & 0x7ff
    }
    pub fn shared(self) -> bool {
        (self.0 >> 15) & 1 != 0
    }
    pub fn node(self) -> usize {
        (self.0 >> 16) & 0x3f
    }
    pub fn locked(self) -> bool {
        (self.0 >> 22) & 1 != 0
    }
    pub fn large_page(self) -> bool {
        (self.0 >> 23) & 1 != 0
    }
    pub fn bad(self) -> bool {
        (self.0 >> 31) & 1 != 0
    }
    /// Bits 24..30 — documented as `Reserved`.
    pub fn reserved_mid(self) -> usize {
        (self.0 >> 24) & 0x7f
    }
    /// Bits 32..63 — documented as `ReservedUlong`, and the only place in this
    /// structure a page frame number could possibly hide.
    pub fn reserved_high(self) -> u64 {
        (self.0 as u64) >> 32
    }
}
