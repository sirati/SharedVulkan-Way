//! The shared mapping, and the only `unsafe` in the transport's core.
//!
//! # What this type is
//!
//! A [`SharedRegion`] is a byte range that **two processes map at different base
//! addresses**. docs/design.md §1 states the invariant the whole project rests on:
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
//! act that snapshots the value — docs/design.md §5.5's distinction between a value
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

/// One run, as [`SharedRegion::stitch`] needs it.
///
/// The same three numbers as `sv_arena::extent::Run` and
/// `crate::cold::AttachRun`, and deliberately a third type: this one is the
/// argument to a syscall loop, that one is the arena's side-local derivation,
/// and the third is a wire form. Collapsing them would make the arena's internal
/// representation into protocol by accident.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StitchRun {
    pub file_offset: u64,
    pub extent_offset: u64,
    pub len: u64,
}

enum Backing {
    /// An ordinary heap allocation. Portable, works under Miri, and is what the
    /// single-process tests and the `Vec<u8>`-backed arena of docs/design.md §13
    /// step 2 use.
    Heap(std::alloc::Layout),
    /// `mmap(MAP_SHARED)` of a file descriptor. The real one.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    Mapped,
    /// A slice of memory somebody else owns and will free — the guest-side
    /// provider's one pinned window, sliced into the control region and the
    /// arena. Dropping this must do nothing.
    Borrowed,
    /// `MapViewOfFile` of a named section object — the Windows counterpart, and
    /// what a Windows-to-Windows pair shares. Owned, so that unmapping and
    /// closing the handle happen together and in the right order.
    ///
    /// Held only for its `Drop`, which is why the compiler cannot see it being
    /// read: releasing the view and the handle *is* what owning it does.
    #[cfg(windows)]
    Section(#[allow(dead_code)] crate::winshm::Section),
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
    #[cfg(any(target_os = "linux", target_os = "macos"))]
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

    /// Build one virtually contiguous region out of scattered runs of `fd`.
    ///
    /// > `sv-hostd` … reserves one `PROT_NONE` range per region and
    /// > `mmap(MAP_FIXED)`s each run into it. The result is virtually contiguous
    /// > with identical internal offsets — the §1 invariant — and nothing above
    /// > the attacher ever learns that it was stitched. — docs/design.md §4.3
    ///
    /// **Both the daemon and every ICD call this**, on the same descriptor with
    /// the same run list, which is the point: two implementations of scatter
    /// stitching would be two chances to get the offsets subtly different, and
    /// the symptom of that is an arena where one side's writes land somewhere
    /// the other is not reading. There is no error for that.
    ///
    /// The reservation is one `mmap` of the whole extent and each run then
    /// replaces its slice of it. Reserving first is not an optimisation:
    /// mapping the runs one at a time and hoping the kernel places them
    /// adjacently does not work, and the failure would be a region whose offsets
    /// are wrong rather than a call that fails.
    #[cfg(target_os = "linux")]
    pub fn stitch(fd: std::os::fd::BorrowedFd<'_>, runs: &[StitchRun]) -> Result<Self> {
        use std::os::fd::AsRawFd;
        let total: u64 = runs.iter().map(|r| r.len).sum();
        let len = usize::try_from(total)
            .map_err(|_| Error::BadGeometry("the extent does not fit in a usize"))?;
        if len == 0 {
            return Err(Error::BadGeometry("a zero-length region is not a region"));
        }
        // Check the runs cover [0, total) exactly once, in order, before any
        // mapping happens. A gap is a hole in the offset space, which is
        // precisely what §1's invariant forbids, and finding out half way
        // through leaves a partly-built arena.
        let mut at = 0u64;
        for r in runs {
            if r.extent_offset != at || r.len == 0 {
                return Err(Error::BadGeometry("the run list has a gap, an overlap or a hole"));
            }
            at = at
                .checked_add(r.len)
                .ok_or(Error::BadGeometry("the run list overflows"))?;
        }

        // SAFETY: a null hint lets the kernel choose; PROT_NONE so that a run
        // we failed to map faults immediately rather than reading zeroes — the
        // same reasoning as R21's, applied to our own mistakes.
        let base = unsafe {
            sys::mmap(
                core::ptr::null_mut(),
                len,
                0, // PROT_NONE
                sys::MAP_PRIVATE | sys::MAP_ANONYMOUS | sys::MAP_NORESERVE,
                -1,
                0,
            )
        };
        if base as isize == -1 {
            return Err(Error::Os { call: "mmap(reserve)", errno: sys::errno() });
        }
        // Owned from here, so an early return unmaps the whole reservation
        // including whatever was already stitched into it.
        let region = SharedRegion { base: base as *mut u8, len, backing: Backing::Mapped };

        for r in runs {
            // SAFETY: `extent_offset + len <= total` by the check above.
            let at = unsafe { (base as *mut u8).add(r.extent_offset as usize) }
                as *mut core::ffi::c_void;
            let got = unsafe {
                sys::mmap(
                    at,
                    r.len as usize,
                    sys::PROT_READ | sys::PROT_WRITE,
                    sys::MAP_SHARED | sys::MAP_FIXED,
                    fd.as_raw_fd(),
                    r.file_offset as i64,
                )
            };
            if got as isize == -1 {
                return Err(Error::Os { call: "mmap(MAP_FIXED run)", errno: sys::errno() });
            }
            if got != at {
                return Err(Error::BadGeometry(
                    "MAP_FIXED did not honour the address — the reservation was not respected",
                ));
            }
        }
        Ok(region)
    }

    /// Reserve `len` bytes of address space, mapped to nothing: the app side's
    /// view of a pool whose regions arrive one at a time
    /// (waycompose-design.md §97).
    ///
    /// Every byte is `PROT_NONE` until [`SharedRegion::map_fixed`] puts a
    /// region's file there, so a pool offset in a region this process has not
    /// mapped faults at once rather than reading zeroes. Nothing is committed:
    /// eight 4 GiB strides cost 32 GiB of address space and no memory.
    #[cfg(target_os = "linux")]
    pub fn reserve(len: usize) -> Result<Self> {
        if len == 0 || !len.is_multiple_of(PAGE) {
            return Err(Error::BadGeometry("a reservation must be whole, non-zero pages"));
        }
        // SAFETY: a null hint lets the kernel choose; an anonymous PROT_NONE
        // mapping reads and writes nothing.
        let base = unsafe {
            sys::mmap(
                core::ptr::null_mut(),
                len,
                0,
                sys::MAP_PRIVATE | sys::MAP_ANONYMOUS | sys::MAP_NORESERVE,
                -1,
                0,
            )
        };
        if base as isize == -1 {
            return Err(Error::Os { call: "mmap(reserve)", errno: sys::errno() });
        }
        Ok(SharedRegion { base: base as *mut u8, len, backing: Backing::Mapped })
    }

    /// Map `len` bytes of `fd`, from `file_offset`, at byte `at` of this
    /// region, shared. The region must be a [`SharedRegion::reserve`]d one (or
    /// any `Mapped` one), and the range must lie inside it.
    ///
    /// Replacing part of a reservation is what `MAP_FIXED` is for, and the
    /// one way it goes wrong is a range outside the reservation, which would
    /// replace some other mapping of this process. That is checked here.
    #[cfg(target_os = "linux")]
    pub fn map_fixed(&self, at: u64, fd: std::os::fd::BorrowedFd<'_>, file_offset: u64, len: u64) -> Result<()> {
        use std::os::fd::AsRawFd;
        if !matches!(self.backing, Backing::Mapped) {
            return Err(Error::BadGeometry("map_fixed into a region this process did not map"));
        }
        if !at.is_multiple_of(PAGE as u64) || !file_offset.is_multiple_of(PAGE as u64) || !len.is_multiple_of(PAGE as u64) {
            return Err(Error::Misaligned { off: at | file_offset | len, align: PAGE as u64 });
        }
        let start = self.check(at, len)?;
        // SAFETY: `[at, at+len)` is inside this region's mapping by the check
        // above, so MAP_FIXED replaces only pages this region owns.
        let want = unsafe { self.base.add(start) } as *mut core::ffi::c_void;
        let got = unsafe {
            sys::mmap(
                want,
                len as usize,
                sys::PROT_READ | sys::PROT_WRITE,
                sys::MAP_SHARED | sys::MAP_FIXED,
                fd.as_raw_fd(),
                file_offset as i64,
            )
        };
        if got as isize == -1 {
            return Err(Error::Os { call: "mmap(MAP_FIXED)", errno: sys::errno() });
        }
        if got != want {
            return Err(Error::BadGeometry("MAP_FIXED did not honour the address"));
        }
        Ok(())
    }

    /// Put `[at, at+len)` back to reserved, unmapped address space.
    #[cfg(target_os = "linux")]
    pub fn unmap_fixed(&self, at: u64, len: u64) -> Result<()> {
        if !matches!(self.backing, Backing::Mapped) {
            return Err(Error::BadGeometry("unmap_fixed in a region this process did not map"));
        }
        let start = self.check(at, len)?;
        // SAFETY: as `map_fixed`: inside this region's own mapping.
        let want = unsafe { self.base.add(start) } as *mut core::ffi::c_void;
        let got = unsafe {
            sys::mmap(
                want,
                len as usize,
                0,
                sys::MAP_PRIVATE | sys::MAP_ANONYMOUS | sys::MAP_NORESERVE | sys::MAP_FIXED,
                -1,
                0,
            )
        };
        if got as isize == -1 {
            return Err(Error::Os { call: "mmap(PROT_NONE)", errno: sys::errno() });
        }
        Ok(())
    }

    /// A region over memory somebody else owns.
    ///
    /// The one caller is the guest-side arena provider, which commits **one**
    /// pinned window covering both regions — because the host locates and
    /// attaches per *extent* (D22), so the guest must lay the two regions out as
    /// sub-ranges of one allocation rather than as two objects. Slicing that
    /// window into a control region and an arena is what this constructor is
    /// for.
    ///
    /// # Safety
    /// `base` must be valid for `len` bytes for as long as the region lives, and
    /// nothing else may free it. In practice that means the caller keeps the
    /// window alive, and the region's `Drop` must not touch it — which is why
    /// this is its own backing variant rather than a `Mapped` with a null owner.
    pub unsafe fn from_raw(base: *mut u8, len: usize) -> Self {
        assert!(len > 0, "a zero-length region is not a region");
        SharedRegion { base, len, backing: Backing::Borrowed }
    }

    /// Take a mapped Windows section object as a region.
    ///
    /// The counterpart of [`SharedRegion::map_fd`], and deliberately the *whole*
    /// section rather than a sub-range: Windows view offsets have to be
    /// multiples of the allocation granularity, which is 64 KiB and not 4 KiB,
    /// so a caller that wanted "the arena starts after the control region" would
    /// be constrained by a number that has nothing to do with this design. Two
    /// sections — one per region — is both simpler and closer to §4.1's "two
    /// independent mappings with independent bases", and it is what the
    /// GPU-allocated Linux path already does with two descriptors.
    #[cfg(windows)]
    pub fn map_section(section: crate::winshm::Section) -> Result<Self> {
        let len = section.len();
        assert!(len > 0);
        // SAFETY: the region takes ownership of the section, so the pointer is
        // valid for as long as the region is. I2: MapViewOfFile returns an
        // allocation-granularity-aligned address, which is 64 KiB here.
        let base = unsafe { section.base_ptr() };
        Ok(SharedRegion { base, len, backing: Backing::Section(section) })
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
    /// (docs/design.md §4.3 step 1) and to lay a canary down (§5.2).
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
    /// question is the application's (docs/design.md §5.3).
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
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            // SAFETY: `base`/`len` are the mapping `map_fd` established. One
            // `munmap` releases every VMA inside the range, which is also why
            // detaching a stitched arena is cheap however many runs built it
            // (docs/design.md §5.7).
            Backing::Mapped => unsafe {
                sys::munmap(self.base as *mut core::ffi::c_void, self.len);
            },
            // Somebody else's memory. Freeing it here would be a double free of
            // the guest provider's window.
            Backing::Borrowed => {}
            // The section's own `Drop` unmaps the view and closes the handle,
            // in that order. Nothing to do here but let it run.
            #[cfg(windows)]
            Backing::Section(_) => {}
        }
    }
}

impl core::fmt::Debug for SharedRegion {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        // Deliberately does not print `base`. A base address in a log is the one
        // number docs/design.md §1 says never crosses, and a log is a place it could.
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
    pub const MAP_PRIVATE: i32 = 0x02;
    pub const MAP_ANONYMOUS: i32 = 0x20;
    pub const MAP_POPULATE: i32 = 0x8000;
    pub const MAP_HUGETLB: i32 = 0x40000;
    pub const MAP_FIXED: i32 = 0x10;
    pub const MAP_NORESERVE: i32 = 0x4000;
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
        pub fn mlock(addr: *const c_void, len: usize) -> i32;
        pub fn munlock(addr: *const c_void, len: usize) -> i32;
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

/// A pinned window of this process's own memory, for a **guest-side** arena
/// provider.
///
/// This is the Linux counterpart of `awe-probe`'s `VirtualAlloc` + `VirtualLock`
/// window, and it lives here rather than in `sv-arena` for one reason:
/// `sv-arena` is `forbid(unsafe_code)`, deliberately, and its crate docs say
/// "every touch of the shared mapping goes through `sv_transport::region`, which
/// bounds-checks and owns the one `unsafe` block". Committing and pinning a
/// mapping is exactly that kind of touch, so the syscalls are here and
/// `sv_arena::guestram` is safe code over them.
///
/// # Why `mlock` and not merely `MAP_POPULATE`
///
/// `MAP_POPULATE` faults the pages in, which makes them exist and does not make
/// them stay. The kernel may reclaim an anonymous page and refault it on a
/// different frame, and the arena's model is that the host located a page once
/// and keeps reading that offset — so a moved frame is a silent divergence, not
/// an error. That is R26. On Windows it rests on `VirtualLock`, whose documented
/// contract promises residency and not frame stability; on Linux anonymous pages
/// are precisely what reclaim is for, so `mlock` is not optional here.
#[cfg(target_os = "linux")]
pub struct LockedWindow {
    base: *mut u8,
    len: usize,
    locked: bool,
}

// The window is this process's own memory, handed to a driver as a pointer and
// otherwise touched only through the slice accessors below.
#[cfg(target_os = "linux")]
unsafe impl Send for LockedWindow {}
#[cfg(target_os = "linux")]
unsafe impl Sync for LockedWindow {}

#[cfg(target_os = "linux")]
impl LockedWindow {
    /// Commit `len` bytes and pin them.
    ///
    /// `huge` asks for `MAP_HUGETLB`, which needs a pre-reserved pool
    /// (`vm.nr_hugepages`) and fails cleanly without one. It is offered rather
    /// than assumed, and **transparent** huge pages are deliberately not relied
    /// on: they are advisory and can be split under pressure, so a locator that
    /// assumed 2 MiB alignment because THP was likely would find nothing when it
    /// was not.
    pub fn commit(len: u64, huge: bool) -> Result<LockedWindow> {
        if len == 0 || !len.is_multiple_of(PAGE as u64) {
            return Err(Error::BadGeometry("a window is a whole number of pages"));
        }
        let len = usize::try_from(len)
            .map_err(|_| Error::BadGeometry("the window does not fit in a usize"))?;
        // MAP_POPULATE = 0x8000, MAP_HUGETLB = 0x40000 on Linux/x86-64.
        let mut flags = sys::MAP_PRIVATE | sys::MAP_ANONYMOUS | sys::MAP_POPULATE;
        if huge {
            flags |= sys::MAP_HUGETLB;
        }
        // SAFETY: a null hint lets the kernel choose the address; `len` is
        // checked above; the return is checked against MAP_FAILED before use.
        let base = unsafe {
            sys::mmap(core::ptr::null_mut(), len, sys::PROT_READ | sys::PROT_WRITE, flags, -1, 0)
        };
        if base as isize == -1 {
            return Err(Error::Os { call: "mmap(anonymous window)", errno: sys::errno() });
        }
        // SAFETY: the mapping was just established and is `len` bytes.
        let locked = unsafe { sys::mlock(base, len) } == 0;
        if !locked {
            let errno = sys::errno();
            // SAFETY: releasing the mapping we just made, exactly once.
            unsafe { sys::munmap(base, len) };
            // ENOMEM here is almost always RLIMIT_MEMLOCK rather than memory,
            // and the two need different fixes, so say which.
            return Err(Error::Os { call: "mlock (RLIMIT_MEMLOCK?)", errno });
        }
        Ok(LockedWindow { base: base as *mut u8, len, locked })
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// The window's bytes.
    pub fn as_mut_slice(&mut self) -> &mut [u8] {
        // SAFETY: I1 — the mapping is `len` bytes this type owns, and `&mut
        // self` excludes any other reference to it.
        unsafe { core::slice::from_raw_parts_mut(self.base, self.len) }
    }

    pub fn as_slice(&self) -> &[u8] {
        // SAFETY: as above, shared.
        unsafe { core::slice::from_raw_parts(self.base, self.len) }
    }

    /// The base address, for handing to a driver as a host pointer.
    ///
    /// # Safety
    /// The caller must not outlive the window, and must treat the memory as
    /// shared with whatever it hands the pointer to.
    pub unsafe fn base_ptr(&self) -> *mut u8 {
        self.base
    }

    /// The page size the kernel actually used, read from `/proc/self/smaps`.
    ///
    /// The counterpart of §4.3's "**`granularity` is asserted, never
    /// assumed**": the Windows side publishes the `LargePage` bit
    /// `QueryWorkingSetEx` reports rather than the flag it passed to
    /// `VirtualAlloc`, and this is the same discipline. `KernelPageSize` is used
    /// rather than `AnonHugePages` because the latter is transparent huge pages,
    /// which may be split later; `KernelPageSize` is 2048 kB only for a real
    /// hugetlb mapping.
    pub fn observed_page_size(&self) -> Option<u64> {
        let base = self.base as usize as u64;
        let smaps = std::fs::read_to_string("/proc/self/smaps").ok()?;
        let mut in_ours = false;
        for line in smaps.lines() {
            if let Some((range, _)) = line.split_once(' ')
                && let Some((start, _)) = range.split_once('-')
                && let Ok(start) = u64::from_str_radix(start, 16)
            {
                in_ours = start == base;
                continue;
            }
            if in_ours && let Some(rest) = line.strip_prefix("KernelPageSize:") {
                return rest.trim().trim_end_matches(" kB").parse::<u64>().ok().map(|kb| kb * 1024);
            }
        }
        None
    }
}

#[cfg(target_os = "linux")]
impl Drop for LockedWindow {
    fn drop(&mut self) {
        // SAFETY: `base`/`len` are the mapping `commit` established, released
        // exactly once. `munlock` first so the accounting is returned even if
        // the unmap were to fail — the pages are locked, and leaking them takes
        // them away from the guest until the process dies.
        unsafe {
            if self.locked {
                sys::munlock(self.base as *const core::ffi::c_void, self.len);
            }
            sys::munmap(self.base as *mut core::ffi::c_void, self.len);
        }
    }
}

#[cfg(target_os = "linux")]
impl core::fmt::Debug for LockedWindow {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("LockedWindow").field("len", &self.len).finish_non_exhaustive()
    }
}

/// Where a named shared-memory object lives on Linux.
///
/// `/dev/shm` is the directory `shm_open(3)` itself uses, so a path under it is
/// a POSIX shared-memory object by any other name — and a path is the more
/// useful spelling here, because the *other* named object this project opens is
/// QEMU's memory backend, which is named by a filesystem path and not by an
/// `shm_open` name. One resolver for both is one code path (see [`open_named`]).
#[cfg(target_os = "linux")]
pub const SHM_DIR: &str = "/dev/shm";

/// The name of the shared object for a session, as both sides spell it.
///
/// The Linux counterpart of [`crate::winshm::section_name`], and deliberately
/// the same shape: a name derived from a session string, which each side opens
/// for itself. Nothing about it crosses a process boundary except as text.
#[cfg(target_os = "linux")]
pub fn shm_path(session: &str) -> String {
    format!("{SHM_DIR}/sv.{session}")
}

/// Create a **named** shared-memory object of `len` bytes, and return it.
///
/// docs/design.md §1a: the arena and the control region are reached by *name*, and
/// the name is what crosses. This is the Linux half of that rule; the Windows
/// half is [`crate::winshm::Section::create`], which has worked this way since
/// the Windows platform layer was written.
///
/// # Why this is not a memfd, and what that costs
///
/// A memfd has no name, so the only ways to give one to another process are
/// `SCM_RIGHTS` or `/proc/<pid>/fd/<n>` — the first is a descriptor crossing the
/// boundary and is forbidden, and the second is a same-machine name that a VM
/// boundary cannot carry. A file under `/dev/shm` is the same `shmem` memory
/// with a name attached.
///
/// **The cost is sealing, and it is measured rather than assumed.** A file under
/// `/dev/shm` is created with `F_SEAL_SEAL` already set — `F_GET_SEALS` returns
/// 1 on this kernel — so `F_ADD_SEALS(F_SEAL_SHRINK)` fails with `EPERM`, and
/// `/dev/udmabuf` refuses any object that does not carry `F_SEAL_SHRINK`. Only
/// `memfd_create(MFD_ALLOW_SEALING)` produces a sealable object, and such an
/// object has no name. So **a named shared-memory object can never back a
/// udmabuf**, which is what decides that the local-dma-buf answer for RADV
/// exists in the cross-VM topology (where the object is QEMU's own sealed memfd,
/// opened by path) and nowhere else. docs/design.md §0.7.
#[cfg(target_os = "linux")]
pub fn shm_create(path: &str, len: u64) -> Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    // Unlink a leftover from a crashed run first: `O_EXCL` would otherwise turn
    // "the last run died" into "this run cannot start", and the pages of the
    // stale object are only freed when the last mapping of it goes away anyway.
    let _ = std::fs::remove_file(path);
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .map_err(|e| Error::Os { call: "open(shm, O_CREAT|O_EXCL)", errno: e.raw_os_error().unwrap_or(0) })?;
    file.set_len(len)
        .map_err(|e| Error::Os { call: "ftruncate", errno: e.raw_os_error().unwrap_or(0) })?;
    Ok(file)
}

/// Open a named object the peer created, by the name it sent.
///
/// The whole of the attaching side's half of the handoff: no descriptor arrives,
/// a name does, and this process opens it itself. The name is a path so that the
/// same function serves both objects this project ever opens — a `/dev/shm`
/// object made by [`shm_create`], and QEMU's memory backend, which `sv-hostd`
/// names either by its `memory-backend-file` path or by `/proc/<pid>/fd/<n>`.
#[cfg(target_os = "linux")]
pub fn open_named(path: &str) -> Result<std::fs::File> {
    std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .map_err(|e| Error::Os { call: "open(named object)", errno: e.raw_os_error().unwrap_or(0) })
}

/// Remove the name. Mappings and open descriptors survive it, as they do for
/// any unlinked file, so this is release of the *name* and not of the memory.
#[cfg(target_os = "linux")]
pub fn shm_unlink(path: &str) {
    let _ = std::fs::remove_file(path);
}

/// Create a memfd of `len` bytes.
///
/// Still here because the *guest-side* provider and the tests want an anonymous
/// object, and because it is the only sealable one (see [`shm_create`]). It is
/// no longer what `sv-hostd` hands out: an object nobody can name is an object
/// that can only be shared by passing a descriptor, and docs/design.md §1a forbids
/// that across the boundary.
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
        // The dev loop in miniature, and the one property docs/design.md §1 needs:
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

#[cfg(all(test, target_os = "linux", not(miri)))]
mod pool_view_tests {
    use super::*;
    use std::os::fd::AsFd;

    /// §97's app-side view: a reservation with two "regions" mapped into it at
    /// their strides, each seeing its own file's bytes, and a region taken
    /// out again reads as nothing mapped (here: the write fails to be seen).
    #[test]
    fn regions_map_into_a_reservation_at_their_offsets() {
        let stride = 1u64 << 24;
        let view = SharedRegion::reserve(4 * stride as usize).unwrap();
        let a = memfd("pool-a", 2 << 20).unwrap();
        let b = memfd("pool-b", 1 << 20).unwrap();
        let ra = SharedRegion::map_fd(a.as_fd(), 0, 2 << 20).unwrap();
        let rb = SharedRegion::map_fd(b.as_fd(), 0, 1 << 20).unwrap();
        ra.write_bytes(4096, b"region-a").unwrap();
        rb.write_bytes(8192, b"region-b").unwrap();
        view.map_fixed(0, a.as_fd(), 0, 2 << 20).unwrap();
        view.map_fixed(2 * stride, b.as_fd(), 0, 1 << 20).unwrap();
        let mut got = [0u8; 8];
        view.read_bytes(4096, &mut got).unwrap();
        assert_eq!(&got, b"region-a");
        view.read_bytes(2 * stride + 8192, &mut got).unwrap();
        assert_eq!(&got, b"region-b");
        // Writes through the view reach the file.
        view.write_bytes(2 * stride + 16, b"from-icd").unwrap();
        rb.read_bytes(16, &mut got).unwrap();
        assert_eq!(&got, b"from-icd");
        // A range outside the reservation is refused, not mapped over
        // somebody else's memory.
        assert!(view.map_fixed(4 * stride, b.as_fd(), 0, 1 << 20).is_err());
        assert!(view.map_fixed(4 * stride - 4096, b.as_fd(), 0, 1 << 20).is_err());
        view.unmap_fixed(2 * stride, 1 << 20).unwrap();
        // Mapping it again after the unmap sees the same bytes.
        view.map_fixed(2 * stride, b.as_fd(), 0, 1 << 20).unwrap();
        view.read_bytes(2 * stride + 16, &mut got).unwrap();
        assert_eq!(&got, b"from-icd");
    }
}

/// §103: the macOS twin of [`sys`], holding only what [`SharedRegion::map_fd`]
/// and its `Drop` need. Separate rather than shared because the constants are
/// not Linux's (`MAP_ANON` is `0x1000`, errno is `__error()`), and a shared
/// module with per-OS numbers is how a wrong flag ships silently.
#[cfg(target_os = "macos")]
pub(crate) mod sys {
    use core::ffi::{c_int, c_void};

    pub const PROT_READ: i32 = 1;
    pub const PROT_WRITE: i32 = 2;
    pub const MAP_SHARED: i32 = 1;

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
        #[link_name = "__error"]
        fn errno_location() -> *mut c_int;
    }

    pub fn errno() -> i32 {
        // SAFETY: `__error` returns a pointer to this thread's errno, valid for
        // the life of the thread.
        unsafe { *errno_location() }
    }
}
