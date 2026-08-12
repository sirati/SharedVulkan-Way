//! The shared mapping, and the only `unsafe` in the transport's core.
//!
//! # What this type is
//!
//! A [`SharedRegion`] is a byte range that **two processes map at different base
//! addresses**. design.md §1 states the invariant the whole project rests on:
//!
//! > the shared regions are mapped at a different base address on each side, but
//! > every byte has the same offset from the region base on both sides.
//!
//! So the only currency this type deals in is `(offset, length)`, and the base
//! address never leaves it.
//!
//! # Safety invariants
//!
//! R7 asks for stated invariants rather than care. These are they.
//!
//! **I1. `base` is valid for reads and writes of `len` bytes, for the lifetime
//! of the value.** Established by the constructors — [`SharedRegion::heap`]
//! allocates `len` bytes and [`SharedRegion::map_fd`] maps `len` bytes — and
//! preserved because nothing but `Drop` releases it.
//!
//! **I2. `base` is at least 4096-byte aligned.** Both constructors guarantee it:
//! the heap one asks the allocator for that alignment, and `mmap` returns
//! page-aligned addresses. Every accessor's alignment check is therefore a check
//! on the *offset* alone, which is what makes it a pure layout question and
//! testable without a mapping.
//!
//! **I3. No `&[u8]`, `&mut [u8]` or `&T` over region bytes is ever handed out,
//! for any `T` other than [`AtomicU32`] and [`AtomicU64`].** This is the
//! important one and it is not negotiable. The peer is another process; it may
//! write any byte at any instant; a `&u32` over such a byte is undefined
//! behaviour no matter how careful the surrounding code is, and struct padding
//! makes a `&VkFoo` unsound even single-threaded. Concurrent mutation of an
//! `AtomicU32` is *defined*, which is exactly why the atomics are the one
//! exception. Everything else is a copy in or a copy out, and the copy is the
//! act that snapshots the value — design.md §5.5's distinction between a value
//! read *out* of shared memory (checkable) and one the GPU reads later by DMA
//! (not).
//!
//! **I4. `read_bytes` and `write_bytes` are called only on a byte range the
//! caller owns exclusively under the ring protocol.** A producer owns
//! `[tail, tail+len)` until it publishes `tail` with a release store; a consumer
//! owns `[head, tail)` after an acquire load of `tail`. Both are ordinary
//! happens-before, established through the atomics of I3. A peer that writes
//! outside its half of that protocol is not a data race we tolerate — it is a
//! protocol violation, and `security-model.md`'s stance is explicit that the two
//! sides are one trust domain. Miri cannot model a second process, so this
//! invariant is carried by review and by the single-process ring tests, not by a
//! tool; saying so is more useful than implying otherwise.
//!
//! **I5. `Send` and `Sync` are sound** because every accessor takes `&self` and
//! is either an atomic operation or a copy governed by I4.
//!
//! # What is deliberately absent
//!
//! There is no `as_slice()`. There was one for about ten minutes and it is the
//! single easiest way to lose I3, because every caller that has one stops
//! thinking about the peer. The arena's `vkMapMemory` pointer is the one place
//! that genuinely needs a raw address, and it goes through
//! [`SharedRegion::base_ptr`], which is `unsafe` and documents what the caller
//! is taking on.

use crate::{Error, Result};
use core::sync::atomic::{AtomicU32, AtomicU64};

/// The page granularity of every mapping in this project.
///
/// It is also `minImportedHostPointerAlignment` as measured on the target
/// (`measured-gpu-side.md`), and the arena allocator's granularity. That the
/// same number answers three questions is a convenience, not a coincidence: all
/// three are the hardware page.
pub const PAGE: usize = 4096;

/// Round `n` up to a whole number of pages.
pub const fn page_up(n: u64) -> u64 {
    n.div_ceil(PAGE as u64) * PAGE as u64
}

enum Backing {
    /// An ordinary heap allocation. Portable, works under Miri, and is what the
    /// single-process tests and the `Vec<u8>`-backed arena of design.md §13
    /// step 2 use.
    Heap(std::alloc::Layout),
    /// `mmap(MAP_SHARED)` of a file descriptor. The real one.
    #[cfg(target_os = "linux")]
    Mapped,
}

/// A region of shared memory, addressed only by offset.
pub struct SharedRegion {
    base: *mut u8,
    len: usize,
    backing: Backing,
}

// I5.
unsafe impl Send for SharedRegion {}
unsafe impl Sync for SharedRegion {}

impl SharedRegion {
    /// A private, zeroed, page-aligned region.
    ///
    /// Not shared with anybody, which makes it exactly right for tests and for
    /// the parts of the development loop that run in one process. Everything
    /// above this type is written as though the peer were real, so a test over a
    /// heap region exercises the same code paths — it just cannot exercise the
    /// cross-process race, which is outside the language anyway (I4).
    pub fn heap(len: usize) -> Result<Self> {
        assert!(len > 0, "a zero-length region is not a region");
        let layout = std::alloc::Layout::from_size_align(len, PAGE)
            .map_err(|_| Error::BadGeometry("region length overflows a layout"))?;
        // SAFETY: `layout` has a non-zero size, checked above.
        let base = unsafe { std::alloc::alloc_zeroed(layout) };
        if base.is_null() {
            return Err(Error::Os { call: "alloc_zeroed", errno: 0 });
        }
        // I1: `len` bytes owned. I2: aligned to PAGE by the layout.
        Ok(SharedRegion { base, len, backing: Backing::Heap(layout) })
    }

    /// Map `len` bytes of `fd` starting at `offset`, shared.
    ///
    /// `offset` must be page-aligned, which `mmap` requires and which the
    /// caller's extent geometry gives for free.
    #[cfg(target_os = "linux")]
    pub fn map_fd(fd: std::os::fd::BorrowedFd<'_>, offset: u64, len: usize) -> Result<Self> {
        use std::os::fd::AsRawFd;
        if !offset.is_multiple_of(PAGE as u64) {
            return Err(Error::Misaligned { off: offset, align: PAGE as u64 });
        }
        assert!(len > 0);
        // SAFETY: a null hint lets the kernel choose the address; `len` and
        // `offset` are the caller's and are checked by the kernel; the fd is
        // borrowed for the call. The return is checked against MAP_FAILED
        // before anything is done with it.
        let addr = unsafe {
            sys::mmap(
                core::ptr::null_mut(),
                len,
                sys::PROT_READ | sys::PROT_WRITE,
                sys::MAP_SHARED,
                fd.as_raw_fd(),
                offset as i64,
            )
        };
        if addr as isize == -1 {
            return Err(Error::Os { call: "mmap", errno: sys::errno() });
        }
        // I1: the kernel established the mapping. I2: mmap is page-aligned.
        Ok(SharedRegion { base: addr as *mut u8, len, backing: Backing::Mapped })
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// The R2 bounds check, in one place.
    ///
    /// Every accessor in this crate and in `sv-arena` reaches the mapping
    /// through this function. That is the whole mitigation: not "check offsets",
    /// which is a thing to remember, but "there is one way in and it checks".
    #[inline]
    pub fn check(&self, off: u64, len: u64) -> Result<usize> {
        let end = off
            .checked_add(len)
            .ok_or(Error::OutOfBounds { off, len, region_len: self.len })?;
        if end > self.len as u64 {
            return Err(Error::OutOfBounds { off, len, region_len: self.len });
        }
        // Cannot fail: `end <= self.len`, and `self.len` is a `usize`.
        Ok(off as usize)
    }

    /// A 32-bit atomic at `off`, which must be 4-byte aligned.
    pub fn atomic_u32(&self, off: u64) -> Result<&AtomicU32> {
        if !off.is_multiple_of(4) {
            return Err(Error::Misaligned { off, align: 4 });
        }
        let at = self.check(off, 4)?;
        // SAFETY: I1 gives a valid pointer; I2 plus the check above gives
        // 4-byte alignment; I3 says nothing else ever holds a non-atomic
        // reference to these bytes. The lifetime is tied to `&self`, so the
        // mapping outlives the reference.
        Ok(unsafe { &*(self.base.add(at) as *const AtomicU32) })
    }

    /// A 64-bit atomic at `off`, which must be 8-byte aligned.
    pub fn atomic_u64(&self, off: u64) -> Result<&AtomicU64> {
        if !off.is_multiple_of(8) {
            return Err(Error::Misaligned { off, align: 8 });
        }
        let at = self.check(off, 8)?;
        // SAFETY: as `atomic_u32`, with 8-byte alignment.
        Ok(unsafe { &*(self.base.add(at) as *const AtomicU64) })
    }

    /// Copy `dst.len()` bytes out of the region.
    pub fn read_bytes(&self, off: u64, dst: &mut [u8]) -> Result<()> {
        let at = self.check(off, dst.len() as u64)?;
        if dst.is_empty() {
            return Ok(());
        }
        // SAFETY: I1 and the bounds check give a source valid for `dst.len()`
        // bytes; `dst` is a live exclusive borrow, so the two cannot overlap
        // (region bytes are never borrowed as a slice — I3). I4 gives the
        // caller exclusive use of the source range for the duration.
        unsafe {
            core::ptr::copy_nonoverlapping(self.base.add(at), dst.as_mut_ptr(), dst.len());
        }
        Ok(())
    }

    /// Copy `src` into the region.
    pub fn write_bytes(&self, off: u64, src: &[u8]) -> Result<()> {
        let at = self.check(off, src.len() as u64)?;
        if src.is_empty() {
            return Ok(());
        }
        // SAFETY: mirror of `read_bytes`. `src` is a shared borrow of memory
        // that is not part of the region (I3), so the ranges are disjoint.
        unsafe {
            core::ptr::copy_nonoverlapping(src.as_ptr(), self.base.add(at), src.len());
        }
        Ok(())
    }

    /// Set `len` bytes to `byte`. Used to zero the region before publication
    /// (design.md §4.3 step 1) and to lay a canary down (§5.2).
    pub fn fill(&self, off: u64, len: u64, byte: u8) -> Result<()> {
        let at = self.check(off, len)?;
        if len == 0 {
            return Ok(());
        }
        // SAFETY: I1 and the bounds check. Writing bytes the peer may be
        // reading is I4's business, and the only caller is initialisation,
        // which happens before `magic` is published.
        unsafe {
            core::ptr::write_bytes(self.base.add(at), byte, len as usize);
        }
        Ok(())
    }

    /// Compare `len` bytes at `off` against `expect`, without copying them out.
    ///
    /// Exists for the R26 canary, which is checked on a timer over every slab
    /// and has no reason to allocate.
    pub fn equals(&self, off: u64, expect: &[u8]) -> Result<bool> {
        let mut buf = [0u8; 64];
        if expect.len() <= buf.len() {
            let dst = &mut buf[..expect.len()];
            self.read_bytes(off, dst)?;
            return Ok(dst == expect);
        }
        let mut got = vec![0u8; expect.len()];
        self.read_bytes(off, &mut got)?;
        Ok(got == expect)
    }

    /// The raw base address.
    ///
    /// # Safety
    ///
    /// The caller takes on I3 in full: the returned pointer addresses memory
    /// another process writes, so forming a reference to anything in it — other
    /// than an atomic — is undefined behaviour. The one legitimate caller is the
    /// ICD's `vkMapMemory`, which hands `base + offset` to the *application* as
    /// a `void*`; from there it is the application's memory and the aliasing
    /// question is the application's (design.md §5.3).
    pub unsafe fn base_ptr(&self) -> *mut u8 {
        self.base
    }
}

impl Drop for SharedRegion {
    fn drop(&mut self) {
        match self.backing {
            // SAFETY: `layout` is the one `heap` allocated with, and I1 says
            // nothing else has freed it.
            Backing::Heap(layout) => unsafe { std::alloc::dealloc(self.base, layout) },
            #[cfg(target_os = "linux")]
            // SAFETY: `base`/`len` are the mapping `map_fd` established. One
            // `munmap` releases every VMA inside the range, which is also why
            // detaching a stitched arena is cheap however many runs built it
            // (design.md §5.7).
            Backing::Mapped => unsafe {
                sys::munmap(self.base as *mut core::ffi::c_void, self.len);
            },
        }
    }
}

impl core::fmt::Debug for SharedRegion {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        // Deliberately does not print `base`. A base address in a log is the one
        // number design.md §1 says never crosses, and a log is a place it could.
        f.debug_struct("SharedRegion").field("len", &self.len).finish_non_exhaustive()
    }
}

/// The handful of libc entry points this crate needs.
///
/// Declared rather than pulled in as a dependency, following `arena-host`,
/// which established the convention for this workspace: the whole project has
/// four external crates and none of them is a syscall wrapper.
#[cfg(target_os = "linux")]
pub(crate) mod sys {
    use core::ffi::{c_char, c_int, c_void};

    pub const PROT_READ: i32 = 1;
    pub const PROT_WRITE: i32 = 2;
    pub const MAP_SHARED: i32 = 1;
    /// `MFD_CLOEXEC | MFD_ALLOW_SEALING`.
    pub const MFD_CLOEXEC_ALLOW_SEALING: u32 = 0x0001 | 0x0002;

    unsafe extern "C" {
        pub fn mmap(
            addr: *mut c_void,
            len: usize,
            prot: i32,
            flags: i32,
            fd: i32,
            off: i64,
        ) -> *mut c_void;
        pub fn munmap(addr: *mut c_void, len: usize) -> i32;
        pub fn memfd_create(name: *const c_char, flags: u32) -> c_int;
        pub fn ftruncate(fd: c_int, len: i64) -> c_int;
        #[link_name = "__errno_location"]
        fn errno_location() -> *mut c_int;
    }

    pub fn errno() -> i32 {
        // SAFETY: `__errno_location` returns a pointer to this thread's errno,
        // valid for the life of the thread.
        unsafe { *errno_location() }
    }
}

/// Create a memfd of `len` bytes.
///
/// This is design.md §3's "in the same-machine development loop it degenerates
/// to *create a memfd, `ftruncate`, hand it out*", and it is the whole of
/// `sv-hostd`'s dev mode's memory half.
#[cfg(target_os = "linux")]
pub fn memfd(name: &str, len: u64) -> Result<std::os::fd::OwnedFd> {
    use std::os::fd::FromRawFd;
    let cname = std::ffi::CString::new(name)
        .map_err(|_| Error::BadGeometry("memfd name contains a NUL"))?;
    // SAFETY: `cname` is NUL-terminated and outlives the call.
    let fd = unsafe { sys::memfd_create(cname.as_ptr(), sys::MFD_CLOEXEC_ALLOW_SEALING) };
    if fd < 0 {
        return Err(Error::Os { call: "memfd_create", errno: sys::errno() });
    }
    // SAFETY: `fd` is a fresh descriptor this function owns.
    let owned = unsafe { std::os::fd::OwnedFd::from_raw_fd(fd) };
    // SAFETY: `owned` is a live descriptor for the duration of the call.
    if unsafe { sys::ftruncate(fd, len as i64) } != 0 {
        return Err(Error::Os { call: "ftruncate", errno: sys::errno() });
    }
    Ok(owned)
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::sync::atomic::Ordering;

    #[test]
    fn a_fresh_region_is_zero() {
        let r = SharedRegion::heap(2 * PAGE).unwrap();
        let mut buf = [0xAAu8; 64];
        r.read_bytes(0, &mut buf).unwrap();
        assert_eq!(buf, [0u8; 64]);
        // A count of zero deserves suspicion, so check the far end too: a
        // constructor that zeroed only the first page would pass the line above.
        r.read_bytes((2 * PAGE - 64) as u64, &mut buf).unwrap();
        assert_eq!(buf, [0u8; 64]);
    }

    #[test]
    fn every_offset_is_checked_against_the_region_length() {
        let r = SharedRegion::heap(PAGE).unwrap();
        assert!(r.check(0, PAGE as u64).is_ok());
        assert_eq!(
            r.check(1, PAGE as u64),
            Err(Error::OutOfBounds { off: 1, len: PAGE as u64, region_len: PAGE })
        );
        // The overflow case, which a naive `off + len > self.len` misses
        // entirely by wrapping to a small number.
        assert!(matches!(r.check(u64::MAX, 8), Err(Error::OutOfBounds { .. })));
        assert!(r.atomic_u64(PAGE as u64 - 8).is_ok());
        assert!(r.atomic_u64(PAGE as u64 - 4).is_err());
    }

    #[test]
    fn atomics_must_be_naturally_aligned() {
        let r = SharedRegion::heap(PAGE).unwrap();
        assert_eq!(r.atomic_u32(2).unwrap_err(), Error::Misaligned { off: 2, align: 4 });
        assert_eq!(r.atomic_u64(4).unwrap_err(), Error::Misaligned { off: 4, align: 8 });
        assert!(r.atomic_u32(4).is_ok());
        assert!(r.atomic_u64(8).is_ok());
    }

    #[test]
    fn an_atomic_and_a_byte_copy_see_the_same_bytes() {
        // The property everything else depends on: the two access paths are two
        // views of one buffer, not two buffers. If this were ever false the
        // handshake would publish into one and the peer would read the other.
        let r = SharedRegion::heap(PAGE).unwrap();
        r.atomic_u64(64).unwrap().store(0x0102_0304_0506_0708, Ordering::Release);
        let mut buf = [0u8; 8];
        r.read_bytes(64, &mut buf).unwrap();
        assert_eq!(buf, 0x0102_0304_0506_0708u64.to_le_bytes());

        r.write_bytes(64, &0xdead_beefu64.to_le_bytes()).unwrap();
        assert_eq!(r.atomic_u64(64).unwrap().load(Ordering::Acquire), 0xdead_beef);
    }

    #[test]
    fn fill_and_equals_agree() {
        let r = SharedRegion::heap(PAGE).unwrap();
        r.fill(128, 64, 0x5A).unwrap();
        assert!(r.equals(128, &[0x5A; 64]).unwrap());
        assert!(!r.equals(128, &[0x5B; 64]).unwrap());
        // …and did not spill either side of the range.
        assert!(r.equals(127, &[0x00]).unwrap());
        assert!(r.equals(192, &[0x00]).unwrap());
        // The long path, past the 64-byte stack buffer.
        r.fill(256, 100, 0x11).unwrap();
        assert!(r.equals(256, &[0x11; 100]).unwrap());
    }

    #[test]
    fn a_zero_length_access_is_a_no_op_and_not_an_error() {
        let r = SharedRegion::heap(PAGE).unwrap();
        r.read_bytes(PAGE as u64, &mut []).unwrap();
        r.write_bytes(PAGE as u64, &[]).unwrap();
        r.fill(PAGE as u64, 0, 0).unwrap();
    }

    #[test]
    fn page_up_rounds_the_way_the_allocator_needs() {
        assert_eq!(page_up(0), 0);
        assert_eq!(page_up(1), PAGE as u64);
        assert_eq!(page_up(PAGE as u64), PAGE as u64);
        assert_eq!(page_up(PAGE as u64 + 1), 2 * PAGE as u64);
    }

    /// Miri's Stacked Borrows is the point of this one: two live `&AtomicU32`
    /// into the same region, used alternately, must not invalidate each other.
    /// A `&mut [u8]` view would, which is why there is not one (I3).
    #[test]
    fn two_atomic_views_coexist() {
        let r = SharedRegion::heap(PAGE).unwrap();
        let a = r.atomic_u32(0).unwrap();
        let b = r.atomic_u32(4).unwrap();
        a.store(1, Ordering::Release);
        b.store(2, Ordering::Release);
        assert_eq!(a.load(Ordering::Acquire), 1);
        assert_eq!(b.load(Ordering::Acquire), 2);
    }

    #[cfg(target_os = "linux")]
    #[test]
    #[cfg_attr(miri, ignore = "memfd_create and mmap are syscalls Miri does not implement")]
    fn a_memfd_maps_twice_and_the_two_views_are_the_same_memory() {
        use std::os::fd::AsFd;
        // The dev loop in miniature, and the one property design.md §1 needs:
        // different base addresses, identical offsets.
        let fd = memfd("sv-test", (4 * PAGE) as u64).unwrap();
        let a = SharedRegion::map_fd(fd.as_fd(), 0, 4 * PAGE).unwrap();
        let b = SharedRegion::map_fd(fd.as_fd(), 0, 4 * PAGE).unwrap();
        // SAFETY: read only, for the assertion that the bases really do differ.
        let (pa, pb) = unsafe { (a.base_ptr(), b.base_ptr()) };
        assert_ne!(pa, pb, "two mappings of one memfd landed at one address");

        a.write_bytes(3 * PAGE as u64 + 16, b"HELLO").unwrap();
        let mut buf = [0u8; 5];
        b.read_bytes(3 * PAGE as u64 + 16, &mut buf).unwrap();
        assert_eq!(&buf, b"HELLO");

        b.atomic_u64(8).unwrap().store(0x1234_5678_9abc_def0, core::sync::atomic::Ordering::Release);
        assert_eq!(
            a.atomic_u64(8).unwrap().load(core::sync::atomic::Ordering::Acquire),
            0x1234_5678_9abc_def0
        );
    }
}
