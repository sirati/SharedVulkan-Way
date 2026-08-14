//! The passthrough arena: extents, slabs, the allocator, and the R26 canary.
//!
//! docs/design.md §5. The arena is the reason this project exists — §1's second
//! invariant is that "the command stream carries offsets, never payloads" — so
//! this crate's whole job is to turn a request for `n` bytes into an offset that
//! means the same thing on both sides.
//!
//! | module | design.md |
//! |---|---|
//! | [`extent`] | §5.1 provider/locator/attacher, §5.7 runs and what attaching costs |
//! | [`alloc`] | §5.2 the 4096-granularity allocator, §5.3 what a `VkDeviceMemory` is, §5.5 deferred free |
//! | [`canary`] | §5.2's canary, `security-model.md` R26 |
//! | [`handle`] | `firefox-accel.md` §7's shared handle, `one-binary.md` §4 |
//! | [`shared`] | `one-binary.md` §3: one allocator, several consumers, disjoint windows |
//!
//! # Three facts this crate is built on, all measured
//!
//! - **Granularity is 4096 bytes**, because that is the measured
//!   `minImportedHostPointerAlignment` on the target *and* the page granularity
//!   of the shared mapping. Nothing is over-allocated to satisfy the driver.
//! - **A slab is an import unit and a failure unit, and nothing else** (§5.2).
//!   It is not the attach unit; the extent is (§5.7, D22).
//! - **The run list never crosses the wire.** On the locating paths the guest
//!   does not know it, and on the AWE path the runs are derived on the far side
//!   from the frame list. It is a side-local derived artefact, rebuilt per
//!   attachment. [`extent::RunList`] therefore has no encoder, deliberately.
//!
//! # `unsafe`
//!
//! None, anywhere in this crate. The allocator lives entirely in the app side's
//! private memory (§5.2: "keeping the free lists in private memory rather than
//! in the shared region means the GPU side cannot be corrupted by an allocator
//! bug and the allocator can be Miri-tested against a plain `Vec<u8>`"), and
//! every touch of the shared mapping goes through `sv_transport::region`, which
//! bounds-checks and owns the one `unsafe` block.

#![forbid(unsafe_code)]

pub mod alloc;
pub mod canary;
pub mod extent;
pub mod handle;
pub mod shared;
/// The Linux guest-side provider. Gated because it is `mlock` and `smaps`, and
/// because the *other* guest-side provider is `awe-probe`'s Win32 one.
#[cfg(target_os = "linux")]
pub mod guestram;
pub mod import;

use core::fmt;

/// The allocator's granularity, and the alignment of every offset it returns.
///
/// docs/design.md §5.2: "dictated by the measured `minImportedHostPointerAlignment`
/// and by the page granularity of the shared mapping".
pub const GRANULARITY: u64 = 4096;

/// Round a request up to the granularity.
///
/// `VUID-VkMemoryAllocateInfo-allocationSize-01745` requires the *size* to be a
/// multiple of the alignment too, so this is not an internal convenience: the
/// rounded figure is what `VkMemoryRequirements` must report, honestly (§5.2).
pub const fn round_up(size: u64) -> u64 {
    size.div_ceil(GRANULARITY) * GRANULARITY
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// The arena is a fixed pre-negotiated window (R11) and it is full. This is
    /// `VK_ERROR_OUT_OF_DEVICE_MEMORY` to the application, and it is a normal
    /// condition rather than a fault.
    OutOfArena { requested: u64, largest_free: u64 },
    /// A single allocation larger than one slab. Not servable: a
    /// `VkDeviceMemory` is a sub-range of exactly one slab (§5.3), because the
    /// GPU side holds one real import per slab.
    LargerThanSlab { requested: u64, slab_size: u64 },
    /// A zero-length allocation. Vulkan forbids it and so do we, by name, so
    /// that it cannot become a zero-page run in the free list.
    ZeroLength,
    /// An offset or length outside the arena (R2).
    OutOfBounds { offset: u64, len: u64, arena_len: u64 },
    /// Freeing something that was never allocated, or freeing it twice. Always a
    /// bug in the caller; loud because the alternative is a free list that
    /// quietly contains the same page twice.
    NotAllocated { slab: u32, offset_in_slab: u64 },
    /// The R26 detector fired. Fatal to the session, not repairable: by the time
    /// it is noticed the two sides have already disagreed about memory.
    CanaryMismatch { slab: u32, expected: [u8; 16], found: [u8; 16] },
    /// A geometry that cannot describe an arena.
    BadGeometry(&'static str),
    /// The run list needs more mappings than the host will give one process
    /// (§5.7). Refused up front with the actual numbers rather than discovered
    /// at the 1048577th `mmap`.
    TooManyRuns { runs: usize, max_map_count: usize },
    Transport(sv_transport::Error),
}

impl From<sv_transport::Error> for Error {
    fn from(e: sv_transport::Error) -> Self {
        Error::Transport(e)
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::OutOfArena { requested, largest_free } => write!(
                f,
                "the arena cannot serve {requested} bytes; the largest free run is {largest_free}"
            ),
            Error::LargerThanSlab { requested, slab_size } => write!(
                f,
                "{requested} bytes is larger than the {slab_size}-byte slab a VkDeviceMemory lives in"
            ),
            Error::ZeroLength => write!(f, "a zero-length allocation"),
            Error::OutOfBounds { offset, len, arena_len } => {
                write!(f, "offset {offset}+{len} is outside the {arena_len}-byte arena")
            }
            Error::NotAllocated { slab, offset_in_slab } => {
                write!(f, "slab {slab} offset {offset_in_slab} was not allocated")
            }
            Error::CanaryMismatch { slab, expected, found } => write!(
                f,
                "slab {slab}: canary is {found:02x?}, expected {expected:02x?} — the two sides no longer agree about memory (R26)"
            ),
            Error::BadGeometry(why) => write!(f, "bad arena geometry: {why}"),
            Error::TooManyRuns { runs, max_map_count } => write!(
                f,
                "attaching needs {runs} mappings and vm.max_map_count is {max_map_count}"
            ),
            Error::Transport(e) => write!(f, "transport: {e}"),
        }
    }
}

impl core::error::Error for Error {}

pub type Result<T> = core::result::Result<T, Error>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rounding_is_to_the_measured_alignment_and_is_reported_honestly() {
        assert_eq!(GRANULARITY, 4096, "the measured minImportedHostPointerAlignment");
        assert_eq!(round_up(0), 0);
        assert_eq!(round_up(1), 4096);
        assert_eq!(round_up(4096), 4096);
        assert_eq!(round_up(4097), 8192);
        // The rounding is the number the application is told, not a private
        // detail: a 1-byte buffer really does consume a page of a fixed window.
        assert_eq!(round_up(100) - 100, 3996);
    }
}
