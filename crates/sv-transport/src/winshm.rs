//! Shared memory on Windows: the named section object, which is the memfd's
//! counterpart.
//!
//! design.md is written so that either side may be either OS, and until now
//! every shared mapping in the project has been `memfd_create` + `mmap`. That
//! is Linux, and the topology the project exists for has **the GPU side inside
//! a Windows guest** — so the first thing a Windows GPU side needs is a way to
//! own a region the app side can map.
//!
//! # The mechanism, and why it is a *named* section
//!
//! `CreateFileMapping(INVALID_HANDLE_VALUE, …)` creates a section object backed
//! by the paging file rather than by a file on disk. It is the direct analogue
//! of a memfd: anonymous, sized at creation, and reference-counted by the
//! kernel. `MapViewOfFile` is `mmap`.
//!
//! The interesting choice is **naming** it. Windows has an exact `SCM_RIGHTS`
//! equivalent — `DuplicateHandle` into the peer's process — and it is the wrong
//! tool here for a reason worth writing down: it needs a handle to the peer
//! *process*, which needs `PROCESS_DUP_HANDLE`, which means the owner needs the
//! peer's pid and the right to open it. That is a privilege relationship
//! between two processes that otherwise have none, and it inverts on this
//! topology — the GPU side would need rights over every application that
//! attaches.
//!
//! A named section needs neither. The name is a rendezvous string in the kernel
//! namespace; the owner creates, the peer opens, and the only thing that has to
//! cross the cold path is the name — which is text, and which therefore also
//! crosses a virtio-serial port unchanged. So the same handoff works for the
//! Windows-to-Windows pair *and* would work across a VM boundary if the sides
//! ever needed it, whereas `DuplicateHandle` works for neither.
//!
//! **The name is not a secret and is not treated as one.** Anything on the
//! machine that can guess it can open the section, exactly as anything that can
//! reach the Unix socket can attach on Linux. `security-model.md`'s answer on
//! Linux is filesystem permissions on the socket; the answer here is the
//! session-local namespace — a name with no prefix is created in the caller's
//! *session*, so another logged-in user's processes cannot see it at all. A
//! `Global\` prefix would be visible machine-wide and is deliberately not used.
//!
//! # What this is not
//!
//! It is **not** the arena provider for the testhost topology. There the
//! arena is guest RAM allocated with `VirtualAlloc(MEM_LARGE_PAGES)` and found
//! by the host with a content scan (§5.1); a section object is invisible to
//! `sv-hostd` because nothing tells the host which guest-physical pages it
//! landed on, and it is pageable, which the scan's stability argument forbids.
//!
//! This is the provider for **two processes on one Windows machine** — the
//! analogue of the memfd development loop, and the configuration that proves
//! the Windows platform layer without a hypervisor in the way.

#[cfg(windows)]
use crate::{Error, Result};

/// A named section object and the view of it this process holds.
///
/// Both the handle and the view are owned: dropping unmaps and closes, and the
/// kernel frees the section when the last handle to it goes.
#[cfg(windows)]
pub struct Section {
    handle: isize,
    base: *mut u8,
    len: usize,
}

#[cfg(windows)]
unsafe impl Send for Section {}
#[cfg(windows)]
unsafe impl Sync for Section {}

#[cfg(windows)]
mod sys {
    use core::ffi::c_void;

    pub const INVALID_HANDLE_VALUE: isize = -1;
    pub const PAGE_READWRITE: u32 = 0x04;
    pub const FILE_MAP_ALL_ACCESS: u32 = 0x000F_001F;
    /// `SEC_COMMIT`: charge the pages against the commit limit now rather than
    /// on first touch. The arena is a fixed pre-negotiated window (R11), so
    /// discovering half way through that the machine cannot back it is the
    /// wrong time to find out.
    pub const SEC_COMMIT: u32 = 0x0800_0000;
    pub const ERROR_ALREADY_EXISTS: u32 = 183;

    unsafe extern "system" {
        pub fn CreateFileMappingW(
            file: isize,
            attrs: *mut c_void,
            protect: u32,
            size_high: u32,
            size_low: u32,
            name: *const u16,
        ) -> isize;
        pub fn OpenFileMappingW(access: u32, inherit: i32, name: *const u16) -> isize;
        pub fn MapViewOfFile(
            mapping: isize,
            access: u32,
            offset_high: u32,
            offset_low: u32,
            len: usize,
        ) -> *mut c_void;
        pub fn UnmapViewOfFile(base: *const c_void) -> i32;
        pub fn CloseHandle(h: isize) -> i32;
        pub fn GetLastError() -> u32;
    }
}

#[cfg(windows)]
fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(core::iter::once(0)).collect()
}

#[cfg(windows)]
impl Section {
    /// Create a section of `len` bytes under `name`, and map the whole of it.
    ///
    /// Fails if a section of that name already exists. That is deliberate and it
    /// is the same decision `Control::create` makes about `magic`: silently
    /// adopting somebody else's region is how two sessions end up sharing rings
    /// and disagreeing about the generation. A stale section from a crashed
    /// process is a real case — Windows frees it when the last handle closes,
    /// so a crash *does* clean up, and a collision therefore means a live peer.
    pub fn create(name: &str, len: u64) -> Result<Section> {
        if len == 0 || !len.is_multiple_of(crate::region::PAGE as u64) {
            return Err(Error::BadGeometry("a section is a whole number of pages"));
        }
        let wname = wide(name);
        // SAFETY: `wname` is NUL-terminated and outlives the call; a null
        // attribute pointer asks for the default descriptor.
        let handle = unsafe {
            sys::CreateFileMappingW(
                sys::INVALID_HANDLE_VALUE,
                core::ptr::null_mut(),
                sys::PAGE_READWRITE | sys::SEC_COMMIT,
                (len >> 32) as u32,
                len as u32,
                wname.as_ptr(),
            )
        };
        if handle == 0 {
            return Err(Error::Os {
                call: "CreateFileMappingW",
                errno: unsafe { sys::GetLastError() } as i32,
            });
        }
        // CreateFileMapping *succeeds* and returns a handle to the existing
        // object when the name is taken, with GetLastError set. Ignoring that is
        // how a second server silently adopts the first one's memory.
        let existing = unsafe { sys::GetLastError() } == sys::ERROR_ALREADY_EXISTS;
        if existing {
            unsafe { sys::CloseHandle(handle) };
            return Err(Error::Os {
                call: "CreateFileMappingW (name already exists)",
                errno: sys::ERROR_ALREADY_EXISTS as i32,
            });
        }
        Section::map(handle, len)
    }

    /// Open an existing section by name and map the whole of it.
    pub fn open(name: &str, len: u64) -> Result<Section> {
        let wname = wide(name);
        // SAFETY: as above; `inherit = 0` because the handle is not passed to
        // children — the peer opens by name, which is the whole point.
        let handle =
            unsafe { sys::OpenFileMappingW(sys::FILE_MAP_ALL_ACCESS, 0, wname.as_ptr()) };
        if handle == 0 {
            return Err(Error::Os {
                call: "OpenFileMappingW",
                errno: unsafe { sys::GetLastError() } as i32,
            });
        }
        Section::map(handle, len)
    }

    fn map(handle: isize, len: u64) -> Result<Section> {
        let len_usize = usize::try_from(len)
            .map_err(|_| Error::BadGeometry("the section does not fit in a usize"))?;
        // SAFETY: `handle` is a live section handle this function owns; the
        // return is checked for null before use.
        let base = unsafe {
            sys::MapViewOfFile(handle, sys::FILE_MAP_ALL_ACCESS, 0, 0, len_usize)
        };
        if base.is_null() {
            let e = unsafe { sys::GetLastError() } as i32;
            unsafe { sys::CloseHandle(handle) };
            return Err(Error::Os { call: "MapViewOfFile", errno: e });
        }
        Ok(Section { handle, base: base as *mut u8, len: len_usize })
    }

    /// The mapped base. Used only to build a [`crate::region::SharedRegion`]
    /// over it, which is where every bounds check lives.
    ///
    /// # Safety
    /// The caller must not outlive the `Section`, and must not alias the region
    /// as anything other than the shared bytes it is.
    pub unsafe fn base_ptr(&self) -> *mut u8 {
        self.base
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
}

#[cfg(windows)]
impl Drop for Section {
    fn drop(&mut self) {
        // SAFETY: both were established by `map` and neither has been released.
        unsafe {
            sys::UnmapViewOfFile(self.base as *const core::ffi::c_void);
            sys::CloseHandle(self.handle);
        }
    }
}

#[cfg(windows)]
impl core::fmt::Debug for Section {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        // No base address, for the reason `SharedRegion`'s Debug gives.
        f.debug_struct("Section").field("len", &self.len).finish_non_exhaustive()
    }
}

/// The name of the section carrying one session's extent.
///
/// One string function so the two sides cannot spell it differently, which is
/// the same reasoning `arena-record` uses for the page record. `Local\` is
/// explicit rather than implied: the default namespace for an unprefixed name
/// *is* the session, but a reader should not have to know that to see that this
/// is not machine-wide.
pub fn section_name(session: &str) -> String {
    format!("Local\\shared_vulkan.{session}")
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    #[test]
    fn a_section_round_trips_between_two_handles_in_one_process() {
        // Two independent mappings of one section, which is what the two
        // processes have — the same code path, minus the process boundary.
        let name = section_name("test-roundtrip");
        let owner = Section::create(&name, 64 * 1024).unwrap();
        let peer = Section::open(&name, 64 * 1024).unwrap();
        unsafe {
            owner.base_ptr().add(4096).write_volatile(0x5c);
            assert_eq!(peer.base_ptr().add(4096).read_volatile(), 0x5c);
            peer.base_ptr().add(8192).write_volatile(0xab);
            assert_eq!(owner.base_ptr().add(8192).read_volatile(), 0xab);
        }
    }

    #[test]
    fn creating_a_name_that_exists_is_refused_rather_than_adopted() {
        let name = section_name("test-collision");
        let _owner = Section::create(&name, 64 * 1024).unwrap();
        assert!(
            Section::create(&name, 64 * 1024).is_err(),
            "CreateFileMapping returns the existing object with ERROR_ALREADY_EXISTS; \
             adopting it would silently share another session's rings"
        );
    }

    #[test]
    fn opening_a_name_that_does_not_exist_fails() {
        assert!(Section::open(&section_name("test-absent"), 4096).is_err());
    }

    #[test]
    fn a_section_is_a_whole_number_of_pages() {
        assert!(Section::create(&section_name("test-ragged"), 100).is_err());
        assert!(Section::create(&section_name("test-zero"), 0).is_err());
    }
}

#[cfg(all(test, not(windows)))]
mod tests {
    /// The name is computed on both sides and on both operating systems — the
    /// Linux app side of a Windows-guest pair has to produce the same string —
    /// so this much is testable everywhere.
    #[test]
    fn the_section_name_is_session_local_and_stable() {
        assert_eq!(super::section_name("abc"), "Local\\shared_vulkan.abc");
    }
}
