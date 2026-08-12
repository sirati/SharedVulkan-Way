//! The app side's view of the passthrough arena.
//!
//! # Scope, stated plainly
//!
//! This module is **the mapping and a placeholder allocator**, not the arena
//! design. `sv-arena` (design.md §12) is being written separately and owns the
//! slab directory, the segregated free lists, deferred free, the R26 canary and
//! the provider/locator/attacher split. What is here is the smallest thing that
//! lets `vkMapMemory` return a real pointer into real shared pages, which is the
//! one property the ICD cannot be written without.
//!
//! [`Backing`] is deliberately the shape of `sv_arena::ArenaAllocator` as that
//! crate currently defines it —
//!
//! ```text
//! Allocation { slab: u32, offset_in_slab: u64, size: u64 }
//! alloc(size) -> Result<Allocation>
//! arena_offset(Allocation) -> Result<u64>
//! free_deferred(Allocation, &[TimelineWatermark])
//! ```
//!
//! — so that swapping [`Placeholder`] for the real allocator is a change of one
//! type name and not a change of shape. **Nothing above this module knows how
//! many `mmap` calls produced the view** (design.md §5.3: "that must stay
//! invisible"), so a stitched multi-run attachment can be substituted with no
//! change above.
//!
//! # What the mapping is, and is not
//!
//! On Linux the arena is a `memfd`, sized once and mapped `MAP_SHARED`. That is
//! the same object `sv-hostd` hands out in the same-machine development loop
//! (design.md §3: "in the same-machine development loop it degenerates to
//! 'create a memfd, `ftruncate`, hand it out'"), so a GPU-side process can be
//! given the descriptor and map the identical pages. Until `sv-transport` lands,
//! this file creates the memfd itself rather than receiving it.
//!
//! It is **not** guest RAM located by content, which is `sv-hostd`'s job on the
//! real topology, and it is not attached per extent.

use std::io;

/// 4096, the allocator's granularity.
///
/// Dictated twice over: by the measured `minImportedHostPointerAlignment` of the
/// target GPU side (`measured-gpu-side.md`) and by the page granularity of the
/// shared mapping. Nothing has to be over-allocated to satisfy the driver.
pub const GRANULARITY: u64 = 4096;

/// The default slab size (design.md §5.2, D4).
pub const DEFAULT_SLAB_SIZE: u64 = 64 << 20;

/// An app-side `VkDeviceMemory` backed by the arena.
///
/// design.md §5.3: "a triple `(slab_id, offset_in_slab, size)` — a sub-range of
/// a slab, 4096-aligned, whose `offset_in_slab + size` never exceeds the slab".
/// Field-for-field `sv_arena::Allocation`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Allocation {
    pub slab: u32,
    pub offset_in_slab: u64,
    /// The rounded size, which is what `VkMemoryRequirements` reports.
    pub size: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArenaError {
    OutOfMemory,
    BadGeometry(&'static str),
    NotMapped,
}

pub type Result<T> = std::result::Result<T, ArenaError>;

/// What the ICD needs from an arena allocator.
///
/// Every method is one `sv_arena::ArenaAllocator` already has.
pub trait Backing: Send {
    fn slab_size(&self) -> u64;
    fn arena_len(&self) -> u64;
    fn alloc(&mut self, size: u64) -> Result<Allocation>;
    fn arena_offset(&self, a: Allocation) -> Result<u64>;
    /// Freed immediately here. The real allocator defers until the timelines an
    /// allocation could be referenced by have passed (design.md §5.5).
    fn free_now(&mut self, a: Allocation) -> Result<()>;
    fn free_bytes(&self) -> u64;
}

pub const fn round_up(size: u64) -> u64 {
    size.div_ceil(GRANULARITY) * GRANULARITY
}

/// A first-fit allocator over one contiguous arena.
///
/// **Placeholder.** It is correct — allocations never overlap, every range is
/// 4096-aligned, and a freed range is coalesced with its neighbours — and it is
/// not the allocator design.md §5.2 specifies: no size classes, no slab
/// directory, no deferred free, no canary. `sv_arena::ArenaAllocator` replaces
/// it whole.
#[derive(Debug)]
pub struct Placeholder {
    arena_len: u64,
    slab_size: u64,
    /// Free ranges as `(offset, len)`, sorted by offset and never adjacent.
    free: Vec<(u64, u64)>,
}

impl Placeholder {
    pub fn new(arena_len: u64, slab_size: u64) -> Result<Placeholder> {
        if arena_len == 0 || !arena_len.is_multiple_of(GRANULARITY) {
            return Err(ArenaError::BadGeometry("arena length must be a multiple of 4096"));
        }
        if slab_size == 0 || !slab_size.is_multiple_of(GRANULARITY) {
            return Err(ArenaError::BadGeometry("slab size must be a multiple of 4096"));
        }
        Ok(Placeholder { arena_len, slab_size, free: vec![(0, arena_len)] })
    }

    /// The arena offset an allocation begins at.
    fn offset_of(&self, a: Allocation) -> u64 {
        a.slab as u64 * self.slab_size + a.offset_in_slab
    }
}

impl Backing for Placeholder {
    fn slab_size(&self) -> u64 {
        self.slab_size
    }
    fn arena_len(&self) -> u64 {
        self.arena_len
    }

    fn alloc(&mut self, size: u64) -> Result<Allocation> {
        let size = round_up(size.max(1));
        // An allocation never straddles a slab boundary: a slab is the unit of
        // GPU-side import, so a range that crossed one would need two imports
        // and could not be expressed as `(slab, offset_in_slab, size)`
        // (design.md §5.2, §5.3).
        for i in 0..self.free.len() {
            let (off, len) = self.free[i];
            let slab = off / self.slab_size;
            let slab_end = (slab + 1) * self.slab_size;
            let start = off;
            if start + size > slab_end || start + size > off + len {
                continue;
            }
            if len == size {
                self.free.remove(i);
            } else {
                self.free[i] = (off + size, len - size);
            }
            return Ok(Allocation {
                slab: slab as u32,
                offset_in_slab: start - slab * self.slab_size,
                size,
            });
        }
        // Nothing fit inside a single slab at a run's head; try the head of the
        // next slab boundary inside each run.
        for i in 0..self.free.len() {
            let (off, len) = self.free[i];
            let aligned = off.div_ceil(self.slab_size) * self.slab_size;
            if aligned >= off + len || aligned + size > off + len {
                continue;
            }
            let slab = aligned / self.slab_size;
            if aligned + size > (slab + 1) * self.slab_size {
                continue;
            }
            let lead = aligned - off;
            let tail_off = aligned + size;
            let tail_len = off + len - tail_off;
            self.free[i] = (off, lead);
            if tail_len > 0 {
                self.free.insert(i + 1, (tail_off, tail_len));
            }
            return Ok(Allocation { slab: slab as u32, offset_in_slab: 0, size });
        }
        Err(ArenaError::OutOfMemory)
    }

    fn arena_offset(&self, a: Allocation) -> Result<u64> {
        let off = self.offset_of(a);
        if off + a.size > self.arena_len {
            return Err(ArenaError::BadGeometry("allocation is outside the arena"));
        }
        Ok(off)
    }

    fn free_now(&mut self, a: Allocation) -> Result<()> {
        let off = self.arena_offset(a)?;
        let pos = self.free.partition_point(|(o, _)| *o < off);
        self.free.insert(pos, (off, a.size));
        // Coalesce with both neighbours, which is what keeps a long-running
        // application from fragmenting the arena into 4 KiB crumbs.
        let mut i = 0;
        while i + 1 < self.free.len() {
            let (o, l) = self.free[i];
            let (o2, l2) = self.free[i + 1];
            if o + l == o2 {
                self.free[i] = (o, l + l2);
                self.free.remove(i + 1);
            } else {
                i += 1;
            }
        }
        Ok(())
    }

    fn free_bytes(&self) -> u64 {
        self.free.iter().map(|(_, l)| *l).sum()
    }
}

// ---------------------------------------------------------------------------
// the mapping
// ---------------------------------------------------------------------------

/// `sv_arena::ArenaAllocator` is what [`Backing`] was shaped after, so the
/// adaptor is four forwarding methods and one field-for-field conversion.
///
/// The conversion is written out rather than `transmute`d even though the two
/// `Allocation` types have identical fields, for the reason design.md §7.3 gives
/// about the generated code: two independently declared views of one layout
/// agreeing is a coincidence maintained by nobody.
impl Backing for sv_arena::alloc::ArenaAllocator {
    fn slab_size(&self) -> u64 {
        sv_arena::alloc::ArenaAllocator::slab_size(self)
    }
    fn arena_len(&self) -> u64 {
        sv_arena::alloc::ArenaAllocator::arena_len(self)
    }
    fn alloc(&mut self, size: u64) -> Result<Allocation> {
        sv_arena::alloc::ArenaAllocator::alloc(self, size)
            .map(|a| Allocation { slab: a.slab, offset_in_slab: a.offset_in_slab, size: a.size })
            .map_err(|_| ArenaError::OutOfMemory)
    }
    fn arena_offset(&self, a: Allocation) -> Result<u64> {
        sv_arena::alloc::ArenaAllocator::arena_offset(
            self,
            sv_arena::alloc::Allocation {
                slab: a.slab,
                offset_in_slab: a.offset_in_slab,
                size: a.size,
            },
        )
        .map_err(|_| ArenaError::BadGeometry("allocation is outside the arena"))
    }
    fn free_now(&mut self, a: Allocation) -> Result<()> {
        sv_arena::alloc::ArenaAllocator::free_now(
            self,
            sv_arena::alloc::Allocation {
                slab: a.slab,
                offset_in_slab: a.offset_in_slab,
                size: a.size,
            },
        )
        .map_err(|_| ArenaError::BadGeometry("freeing a range that is not allocated"))
    }
    fn free_bytes(&self) -> u64 {
        sv_arena::alloc::ArenaAllocator::free_bytes(self)
    }
}

/// A shared mapping of the arena.
///
/// One of the three modules design.md §7.3 permits hand-written `unsafe` in
/// ("the mapping, the ring, and the arena allocator"), and the invariants are:
///
/// - `base` is the result of one successful `mmap` of `len` bytes and is
///   unmapped exactly once, in `Drop`.
/// - Nothing in this process ever constructs a Rust reference into the mapping.
///   The peer can write any byte of it at any moment, so a `&T` over it would
///   violate the aliasing model no matter how careful we were (design.md §7.3).
///   The only thing that leaves this module is a raw pointer, which is what
///   `vkMapMemory` is specified to return.
#[derive(Debug)]
pub struct Mapping {
    base: *mut u8,
    len: usize,
    /// Never read on a platform whose `create` cannot succeed — the field is
    /// what decides whether `Drop` unmaps, and `Drop` only unmaps on Linux.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    backing: MappingBacking,
}

/// Who owns the pages under a [`Mapping`].
///
/// Two cases, and conflating them is a double `munmap`: a standalone driver made
/// the mapping itself and must unmap it, while a connected one is looking at
/// `sv-transport`'s mapping and must not. Holding the `Arc` in the second case
/// is what makes the pointer `vkMapMemory` returns outlive every use of it.
#[derive(Debug)]
enum MappingBacking {
    /// `memfd_create` + `mmap` here; unmapped in `Drop`.
    #[cfg(target_os = "linux")]
    Owned(std::os::fd::OwnedFd),
    /// `sv_transport::region::SharedRegion` owns the mapping and unmaps it.
    ///
    /// The `Arc` is never read and is the entire point of the variant: it is
    /// what keeps that mapping alive for as long as any pointer `vkMapMemory`
    /// handed out. Dropping it would unmap the arena under the application.
    Shared(#[allow(dead_code)] std::sync::Arc<sv_transport::region::SharedRegion>),
}

// SAFETY: a raw pointer to a shared mapping is as thread-safe as the mapping
// is, which is entirely: the kernel object is shared between processes already,
// and this type hands out no references.
unsafe impl Send for Mapping {}
unsafe impl Sync for Mapping {}

impl Mapping {
    /// Create a `memfd` of `len` bytes and map it `MAP_SHARED`.
    ///
    /// The development-loop arena of design.md §3. The descriptor is kept so
    /// that `sv-hostd`, or a GPU-side process on the same machine, can be handed
    /// it over the cold path once `sv-transport` lands.
    #[cfg(target_os = "linux")]
    pub fn create(len: u64) -> io::Result<Mapping> {
        use std::os::fd::FromRawFd;
        let len = usize::try_from(len)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "arena too large"))?;
        let name = c"sv-arena";
        // SAFETY: `name` is a NUL-terminated string that outlives the call.
        let fd = unsafe { libc::memfd_create(name.as_ptr(), libc::MFD_CLOEXEC) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `fd` is a fresh descriptor this call owns.
        let fd = unsafe { std::os::fd::OwnedFd::from_raw_fd(fd) };
        // SAFETY: `fd` is a memfd, which `ftruncate` accepts.
        if unsafe { libc::ftruncate(std::os::fd::AsRawFd::as_raw_fd(&fd), len as libc::off_t) } < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: a null hint, a positive length, and a valid descriptor.
        let base = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                std::os::fd::AsRawFd::as_raw_fd(&fd),
                0,
            )
        };
        if base == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }
        Ok(Mapping { base: base as *mut u8, len, backing: MappingBacking::Owned(fd) })
    }

    /// Adopt `sv-transport`'s mapping of the arena.
    ///
    /// The pointer is the region's own base, so `vkMapMemory` returns an address
    /// inside the *same* mapping the transport reads and the GPU side's
    /// `VkDeviceMemory` covers. That identity is design.md §1's invariant, and
    /// making it a borrow rather than a second `mmap` is what guarantees it
    /// rather than arranging it.
    pub fn attach(
        region: &std::sync::Arc<sv_transport::region::SharedRegion>,
        len: u64,
    ) -> Mapping {
        // SAFETY: the pointer is the base of a live mapping, and the `Arc` kept
        // in `backing` holds that mapping alive for as long as this `Mapping`
        // exists. Nothing here forms a reference into it — see the type's own
        // invariants above.
        let base = unsafe { region.base_ptr() };
        Mapping {
            base,
            len: (len as usize).min(region.len()),
            backing: MappingBacking::Shared(std::sync::Arc::clone(region)),
        }
    }

    /// No arena on a platform whose mapping has not been written.
    ///
    /// Returning an error rather than falling back to private memory is
    /// deliberate: private memory would make `vkMapMemory` succeed and the
    /// sharing silently absent, which is the one failure this project must never
    /// have. The app side on Windows needs `CreateFileMapping`, and that is
    /// `sv-hostd`'s counterpart rather than this module's.
    #[cfg(not(target_os = "linux"))]
    pub fn create(_len: u64) -> io::Result<Mapping> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "the app-side arena mapping is implemented for Linux only; a Windows \
             app side needs the CreateFileMapping path, which is not written",
        ))
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// The descriptor, for handing to the peer over the cold path.
    ///
    /// `None` on an attached mapping: the descriptor belongs to whichever side
    /// provided the arena, and it is not ours to hand on.
    #[cfg(target_os = "linux")]
    pub fn fd(&self) -> Option<std::os::fd::BorrowedFd<'_>> {
        match &self.backing {
            MappingBacking::Owned(fd) => Some(std::os::fd::AsFd::as_fd(fd)),
            MappingBacking::Shared(_) => None,
        }
    }

    /// A pointer to `offset` within the mapping, or `None` if the range does not
    /// fit.
    ///
    /// The bounds check is R2: "bounds-check every offset against the region
    /// length. Cheap, and catches our own generator bugs, which is the realistic
    /// failure."
    pub fn pointer_at(&self, offset: u64, size: u64) -> Option<*mut u8> {
        let end = offset.checked_add(size)?;
        if end > self.len as u64 {
            return None;
        }
        // SAFETY: `offset` is within `len`, so the result is within the same
        // allocated object.
        Some(unsafe { self.base.add(offset as usize) })
    }
}

impl Drop for Mapping {
    fn drop(&mut self) {
        #[cfg(target_os = "linux")]
        if matches!(self.backing, MappingBacking::Owned(_)) {
            // SAFETY: `base`/`len` are exactly what `mmap` returned and this
            // runs once. The `Shared` case is deliberately excluded: that
            // mapping belongs to `sv_transport::region::SharedRegion`, which
            // unmaps it in its own `Drop`, and unmapping it here would be the
            // second `munmap` of one range.
            unsafe {
                libc::munmap(self.base as *mut libc::c_void, self.len);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allocations_are_aligned_and_never_overlap() {
        let mut a = Placeholder::new(1 << 20, 1 << 18).unwrap();
        let mut spans = Vec::new();
        for size in [1u64, 4095, 4096, 4097, 100_000] {
            let al = a.alloc(size).unwrap();
            let off = a.arena_offset(al).unwrap();
            assert_eq!(off % GRANULARITY, 0, "every allocation is page-aligned");
            assert!(al.size >= size && al.size % GRANULARITY == 0);
            spans.push((off, al.size, al));
        }
        for i in 0..spans.len() {
            for j in i + 1..spans.len() {
                let (o1, l1, _) = spans[i];
                let (o2, l2, _) = spans[j];
                assert!(o1 + l1 <= o2 || o2 + l2 <= o1, "allocations overlap");
            }
        }
        // An allocation never straddles a slab boundary, because a slab is the
        // unit of GPU-side import.
        for (_, _, al) in &spans {
            assert!(al.offset_in_slab + al.size <= a.slab_size());
        }
        let before = a.free_bytes();
        for (_, _, al) in spans {
            a.free_now(al).unwrap();
        }
        assert!(a.free_bytes() > before);
        // Everything came back, coalesced into one run.
        assert_eq!(a.free_bytes(), 1 << 20);
        assert_eq!(a.free.len(), 1, "freed ranges coalesce");
    }

    #[test]
    fn the_arena_runs_out_rather_than_overlapping() {
        let mut a = Placeholder::new(8192, 8192).unwrap();
        assert!(a.alloc(4096).is_ok());
        assert!(a.alloc(4096).is_ok());
        assert_eq!(a.alloc(4096), Err(ArenaError::OutOfMemory));
    }

    /// Miri cannot run this one: `memfd_create` and `mmap` are not things it
    /// models, and pretending otherwise with a `Vec` would test a different
    /// thing. What Miri does cover is the allocator above, which is where the
    /// arithmetic that could be wrong lives.
    #[cfg(target_os = "linux")]
    #[cfg_attr(miri, ignore)]
    #[test]
    fn the_mapping_is_real_and_bounded() {
        let m = Mapping::create(2 * GRANULARITY).expect("memfd + mmap");
        assert_eq!(m.len(), 2 * GRANULARITY as usize);
        let p = m.pointer_at(0, 8).unwrap();
        // SAFETY: within the mapping, which is writable.
        unsafe {
            p.cast::<u64>().write_unaligned(0x0123_4567_89ab_cdef);
            assert_eq!(p.cast::<u64>().read_unaligned(), 0x0123_4567_89ab_cdef);
        }
        // R2: an offset past the end is refused rather than returned.
        assert!(m.pointer_at(2 * GRANULARITY, 1).is_none());
        assert!(m.pointer_at(u64::MAX, 1).is_none());
        assert!(m.pointer_at(GRANULARITY, GRANULARITY).is_some());
    }
}
