//! The shared handle: how one side names a frame the other side must present,
//! without copying it and without a descriptor.
//!
//! `docs/firefox-accel.md` §7 asked what a shared handle between the Vulkan side
//! and the Wayland side would have to be, and answered it in prose: **an arena
//! slab id plus an offset and a length, plus a layout — and never a
//! descriptor.** This module is that answer as a type, so that the property is
//! checked by the compiler rather than by whoever reads the prose next.
//!
//! # Why this lives in `sv-arena` and not beside the compositor
//!
//! Because a handle is a statement about the arena, and there is one arena. The
//! whole cost this type exists to remove comes from having had two: the frame
//! was rendered into shared_vulkan's arena in the guest, read out to the host,
//! copied into the client's `wl_shm` pool, copied back into waycompose's arena
//! in the guest, and only then presented — **two full-frame crossings of the VM
//! boundary, in opposite directions, to move bytes that never had to leave guest
//! RAM.** A handle is what replaces both crossings, and it can only be defined
//! once, in the crate that owns the region it names.
//!
//! # The boundary rule, argued here rather than asserted elsewhere
//!
//! `design.md` §1a: no file descriptor and no handle ever crosses between the
//! two sides. waycompose's §14.7 sets the standard for how that claim is made —
//! *"a count that a reader cannot judge from the document is not an assertion,
//! it is a number"* — so the argument is made here, at the type, where a reader
//! meets it.
//!
//! **[`FrameHandle`] cannot represent a descriptor.** Not "does not"; cannot.
//! Every field is an integer, there is no variant, no `Box<dyn …>`, no pointer,
//! no `RawFd` and no `HANDLE`. Its wire form ([`FrameHandle::to_words`]) is a
//! fixed-length array of `u32`, which is precisely the argument list of a
//! Wayland request built from `uint` arguments — a protocol message with no `h`
//! in its signature has nowhere to put a descriptor, exactly as §14.7's upstream
//! writer has no fd argument to pass one through. The construction is the
//! guarantee; [`fixtures`]-backed tests assert the shape, but the reason to
//! trust it is that there is nothing else the type could hold.
//!
//! This is the property that gets *harder* to keep once the two sides are one
//! process, because then a `VkImage`'s real memory and the compositor's surface
//! are reachable from the same address space and passing a pointer would simply
//! work. It would work on one machine and be unrepresentable on the deployment,
//! which `design.md` §1a calls "a same-machine path wearing the design's
//! clothes". Naming every frame through this type is what stops that, and it is
//! why the type is mandatory on the in-guest path even though, in-guest, a
//! pointer would be legal.
//!
//! # What it does not carry
//!
//! No base address, on either side. §1's invariant is that the two sides map the
//! region at different bases and that **offsets are the only currency that
//! crosses**, so a handle that carried an address would be wrong on the side
//! that did not mint it. [`Slice::arena_offset`] is the only way to turn one
//! into a position, and it needs the slab size, which comes from the control
//! region's header rather than from the handle.

use crate::{Error, GRANULARITY, Result};

/// The pixel formats a handle may name.
///
/// **The discriminants are `wl_shm`'s own**, deliberately, so that a compositor
/// handed a handle does no translation at all: the number in the handle is the
/// number it passes to `wl_shm_pool.create_buffer`. A translation table is a
/// place for the two sides to disagree, and the whole point of one binary is
/// that there is no second table to drift.
///
/// Both are the formats `wl_shm` requires every compositor to support, which is
/// why the set is small and closed rather than a mirror of `VkFormat`. The Vulkan
/// side's swapchain already blits into a linear buffer (§0.6, `svPresentBlit`);
/// picking the destination format is its job, and picking one the compositor
/// cannot refuse is the point.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum Format {
    /// `wl_shm::format::argb8888`. Byte-for-byte `VK_FORMAT_B8G8R8A8_UNORM` on a
    /// little-endian host, which is what both target GPUs present.
    Argb8888 = 0,
    /// `wl_shm::format::xrgb8888`. The same bytes with the alpha channel
    /// ignored; every screenshot in waycompose §15 is one of these.
    Xrgb8888 = 1,
}

impl Format {
    /// Bytes per pixel. Both formats are 4, and the function exists so that a
    /// third format cannot be added without a caller being told to think.
    pub const fn bytes_per_pixel(self) -> u32 {
        match self {
            Format::Argb8888 | Format::Xrgb8888 => 4,
        }
    }

    /// Decode a format from the wire.
    ///
    /// Unknown values are refused rather than defaulted. A compositor that
    /// guessed here would show a window full of plausible pixels in the wrong
    /// channel order, which is the failure waycompose §12.5's `verify` line
    /// exists to catch one layer down and is not one to reintroduce here.
    pub const fn from_u32(v: u32) -> Option<Format> {
        match v {
            0 => Some(Format::Argb8888),
            1 => Some(Format::Xrgb8888),
            _ => None,
        }
    }
}

/// A range of one arena, named the only way a range may be named.
///
/// `(arena, slab, offset_in_slab, len)`. The slab is part of the name rather
/// than folded into a flat offset because a slab is the import unit on the GPU
/// side (§5.2, D4): one `VkDeviceMemory` lives inside exactly one slab, and a
/// handle that spanned two would name something the driver never imported.
/// [`Slice::validate`] refuses that case by name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Slice {
    /// Which arena. Today there is one and this is 0; it is in the wire form
    /// because waycompose §17.2 established that **an arena id names a region,
    /// not a client**, and a second region (a second guest, a second device) is
    /// a thing this design should be able to say rather than a thing it should
    /// have to be rebuilt to say.
    pub arena: u32,
    /// Which slab of that arena.
    pub slab: u32,
    /// Where in the slab, in bytes. Always a multiple of [`GRANULARITY`].
    pub offset_in_slab: u64,
    /// How many bytes. Always a multiple of [`GRANULARITY`] — see
    /// [`crate::round_up`] for why the rounding is reported honestly rather
    /// than hidden.
    pub len: u64,
}

impl Slice {
    /// The offset of this slice from the base of the arena.
    ///
    /// This is the only way to turn a handle into a position, and it takes the
    /// slab size as an argument rather than storing it, because the slab size is
    /// the control region header's `arena_slab_size` (§4.2) and belongs to the
    /// session rather than to the frame. Two sides that disagreed about it would
    /// disagree about every handle, loudly and immediately, which is the right
    /// failure.
    pub fn arena_offset(&self, slab_size: u64) -> Result<u64> {
        if slab_size == 0 || !slab_size.is_multiple_of(GRANULARITY) {
            return Err(Error::BadGeometry("slab size is not a multiple of the granularity"));
        }
        if self.offset_in_slab >= slab_size {
            return Err(Error::OutOfBounds {
                offset: self.offset_in_slab,
                len: self.len,
                arena_len: slab_size,
            });
        }
        u64::from(self.slab)
            .checked_mul(slab_size)
            .and_then(|base| base.checked_add(self.offset_in_slab))
            .ok_or(Error::BadGeometry("slab index times slab size overflows"))
    }

    /// Check that this slice is a thing the arena could actually have handed
    /// out, given the geometry.
    ///
    /// Three refusals, each of which is a real way to be wrong rather than a
    /// tidy-mindedness check:
    ///
    /// * **not granular** — an offset or length off a page boundary cannot have
    ///   come from [`crate::alloc::ArenaAllocator`], so it was computed by hand,
    ///   and a hand-computed offset is the one that is wrong;
    /// * **spans two slabs** — names memory that is two driver imports (D4);
    /// * **outside the arena** — R2.
    pub fn validate(&self, arena_len: u64, slab_size: u64) -> Result<()> {
        if self.len == 0 {
            return Err(Error::ZeroLength);
        }
        if !self.offset_in_slab.is_multiple_of(GRANULARITY) || !self.len.is_multiple_of(GRANULARITY) {
            return Err(Error::BadGeometry("a handle's offset and length must be page-granular"));
        }
        let end_in_slab = self
            .offset_in_slab
            .checked_add(self.len)
            .ok_or(Error::BadGeometry("offset plus length overflows"))?;
        if end_in_slab > slab_size {
            return Err(Error::LargerThanSlab { requested: self.len, slab_size });
        }
        let start = self.arena_offset(slab_size)?;
        let end = start
            .checked_add(self.len)
            .ok_or(Error::BadGeometry("offset plus length overflows"))?;
        if end > arena_len {
            return Err(Error::OutOfBounds { offset: start, len: self.len, arena_len });
        }
        Ok(())
    }
}

/// A frame: a [`Slice`], how to read it, and which frame it is.
///
/// This is the whole of what crosses when the Vulkan side has rendered
/// something the Wayland side must show. It is 48 bytes and it is the same 48
/// bytes on both sides.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameHandle {
    /// Where the pixels are.
    pub slice: Slice,
    /// Pixels across.
    pub width: u32,
    /// Pixels down.
    pub height: u32,
    /// Bytes per row. Not `width * bpp`: the Vulkan side's linear copy
    /// destination has whatever row pitch the driver asked for, and `wl_shm`
    /// carries a stride precisely so the consumer does not have to assume.
    pub stride: u32,
    /// How to interpret the bytes.
    pub format: Format,
    /// Which frame. Monotonic per surface.
    ///
    /// This is the release token, and it is the reason a handle is not merely a
    /// pointer with extra steps. The renderer may not write a slice again until
    /// the presenter has released the sequence that named it; with two processes
    /// that rule was enforced by the copy (the presenter had its own bytes), and
    /// **removing the copy is exactly what removes that enforcement**. See
    /// `docs/one-binary.md` §5 for the ownership protocol this number carries.
    pub seq: u64,
}

/// The number of `u32`s in a handle's wire form.
///
/// Named so that a Wayland request built from `uint` arguments and the ring
/// encoding cannot drift apart in length without a test noticing.
pub const WORDS: usize = 12;

impl FrameHandle {
    /// The fewest bytes a slice must have for this layout to be readable.
    ///
    /// `stride * height`, and not `stride * height` rounded to anything: the
    /// last row does not need padding after it, and requiring it would refuse a
    /// handle the driver legitimately produced.
    pub fn min_len(&self) -> Result<u64> {
        u64::from(self.stride)
            .checked_mul(u64::from(self.height))
            .ok_or(Error::BadGeometry("stride times height overflows"))
    }

    /// Check the layout against itself and against the slice it names.
    ///
    /// The check that matters is the last one: **the slice must be long enough
    /// for the layout**. A handle whose length is short by one row is a
    /// compositor reading whatever the allocator put after it, which is another
    /// client's pixels in the worst case and is the failure mode waycompose
    /// §17.3 narrowed `peer_slice` to prevent. It is refused here as well,
    /// because a bounds check at the consumer and a bounds check at the producer
    /// are not the same check.
    pub fn validate(&self, arena_len: u64, slab_size: u64) -> Result<()> {
        self.slice.validate(arena_len, slab_size)?;
        if self.width == 0 || self.height == 0 {
            return Err(Error::BadGeometry("a frame with no pixels"));
        }
        let row = u64::from(self.width)
            .checked_mul(u64::from(self.format.bytes_per_pixel()))
            .ok_or(Error::BadGeometry("width times bytes-per-pixel overflows"))?;
        if u64::from(self.stride) < row {
            return Err(Error::BadGeometry("stride is narrower than a row of pixels"));
        }
        if self.min_len()? > self.slice.len {
            return Err(Error::OutOfBounds {
                offset: self.slice.offset_in_slab,
                len: self.min_len()?,
                arena_len: self.slice.len,
            });
        }
        Ok(())
    }

    /// The wire form: [`WORDS`] little-endian `u32`s.
    ///
    /// **This is the whole of what may cross.** It is an array of integers, so a
    /// Wayland request declared with [`WORDS`] `uint` arguments carries it
    /// exactly, and such a request has no `h` in its signature and therefore no
    /// way to carry a descriptor — the same "unrepresentable rather than merely
    /// zero" argument waycompose §14.7 makes about its upstream socket, made
    /// here about the message instead of about the writer.
    ///
    /// 64-bit fields are split low word first, matching `sv_wire`'s
    /// little-endian rule (§4.2: "the design refuses big-endian peers at
    /// handshake rather than byte-swapping").
    pub fn to_words(&self) -> [u32; WORDS] {
        let lo = |v: u64| (v & 0xFFFF_FFFF) as u32;
        let hi = |v: u64| (v >> 32) as u32;
        [
            self.slice.arena,
            self.slice.slab,
            lo(self.slice.offset_in_slab),
            hi(self.slice.offset_in_slab),
            lo(self.slice.len),
            hi(self.slice.len),
            self.width,
            self.height,
            self.stride,
            self.format as u32,
            lo(self.seq),
            hi(self.seq),
        ]
    }

    /// Decode the wire form.
    ///
    /// Refuses an unknown format rather than defaulting; see
    /// [`Format::from_u32`]. Everything else is structurally decodable, and the
    /// checks that need the arena's geometry are [`FrameHandle::validate`],
    /// deliberately separate so that a decoder does not have to have the
    /// geometry to hand.
    pub fn from_words(w: &[u32; WORDS]) -> Result<FrameHandle> {
        let join = |lo: u32, hi: u32| (u64::from(hi) << 32) | u64::from(lo);
        Ok(FrameHandle {
            slice: Slice {
                arena: w[0],
                slab: w[1],
                offset_in_slab: join(w[2], w[3]),
                len: join(w[4], w[5]),
            },
            width: w[6],
            height: w[7],
            stride: w[8],
            format: Format::from_u32(w[9])
                .ok_or(Error::BadGeometry("a format neither side defines"))?,
            seq: join(w[10], w[11]),
        })
    }

    /// Encode onto an `sv_wire` stream, for the ring path.
    ///
    /// The Wayland path uses [`FrameHandle::to_words`] and this uses the same
    /// words, so the two transports carry identical bytes and a round-trip test
    /// through either proves the other.
    pub fn encode(&self, enc: &mut sv_wire::Encoder<'_>) -> sv_wire::Result<()> {
        for word in self.to_words() {
            enc.u32(word)?;
        }
        Ok(())
    }

    /// Decode from an `sv_wire` stream.
    pub fn decode(dec: &mut sv_wire::Decoder<'_>) -> Result<FrameHandle> {
        let mut w = [0u32; WORDS];
        for slot in &mut w {
            *slot = dec.u32().map_err(|_| Error::BadGeometry("a truncated frame handle"))?;
        }
        FrameHandle::from_words(&w)
    }
}

/// Handles that are useful in more than one crate's tests.
///
/// Exported rather than kept in `#[cfg(test)]` because the consumer of a handle
/// is, by construction, a different crate from the producer — that is the entire
/// point of the type — and a fixture only one side can see would be tested only
/// on one side.
pub mod fixtures {
    use super::{Format, FrameHandle, Slice};

    /// A 1280x900 `Xrgb8888` frame: the content subsurface Firefox actually
    /// creates, measured in waycompose §15.10 at 4,608,000 bytes per buffer.
    pub fn firefox_content(seq: u64) -> FrameHandle {
        FrameHandle {
            slice: Slice { arena: 0, slab: 0, offset_in_slab: 0, len: 4_608_000 },
            width: 1280,
            height: 900,
            stride: 1280 * 4,
            format: Format::Xrgb8888,
            seq,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A 64 MiB slab, and an arena of eight of them — so that `sample()`'s slab
    /// 3 is a slab that exists. Getting this wrong once was instructive: the
    /// handle was refused as `OutOfBounds` before the layout was ever looked at,
    /// which is [`FrameHandle::validate`] checking the slice first and is the
    /// right order.
    const SLAB: u64 = 64 * 1024 * 1024;
    const ARENA: u64 = 8 * SLAB;

    fn sample() -> FrameHandle {
        FrameHandle {
            slice: Slice { arena: 1, slab: 3, offset_in_slab: 8192, len: 4096 * 100 },
            width: 100,
            height: 100,
            stride: 400,
            format: Format::Argb8888,
            seq: 0x1_0000_0007,
        }
    }

    #[test]
    fn a_handle_round_trips_through_the_wayland_word_form() {
        let h = sample();
        assert_eq!(FrameHandle::from_words(&h.to_words()).unwrap(), h);
    }

    #[test]
    fn a_handle_round_trips_through_the_ring_encoding() {
        let h = sample();
        let mut buf = [0u8; WORDS * 4];
        let mut enc = sv_wire::Encoder::new(&mut buf);
        h.encode(&mut enc).unwrap();
        let n = enc.finish().unwrap();
        assert_eq!(n, WORDS * 4, "the two transports must carry identical bytes");
        let mut dec = sv_wire::Decoder::new(&buf);
        assert_eq!(FrameHandle::decode(&mut dec).unwrap(), h);
    }

    #[test]
    fn the_wire_form_is_integers_and_therefore_cannot_carry_a_descriptor() {
        // docs/design.md §1a, and waycompose §14.7's standard for how the claim is
        // made. The assertion a test *can* make is that the wire form is a
        // fixed-length array of u32 and that nothing is lost by treating it as
        // one: a descriptor is not a number the peer can act on, and there is no
        // second channel here for one to travel in.
        let h = sample();
        let words = h.to_words();
        assert_eq!(words.len(), WORDS);
        assert_eq!(core::mem::size_of_val(&words), WORDS * 4);
        // The whole value survives the trip through pure integers. If a future
        // field ever needed something that is not an integer, this assertion is
        // what fails, and it fails at the point the field is added.
        assert_eq!(FrameHandle::from_words(&words).unwrap(), h);
    }

    #[test]
    fn the_arena_offset_needs_the_slab_size_and_never_a_base_address() {
        let h = sample();
        // §1: offsets are the only currency that crosses.
        assert_eq!(h.slice.arena_offset(64 * 1024 * 1024).unwrap(), 3 * 64 * 1024 * 1024 + 8192);
        // A different slab size is a different answer, which is why it is the
        // session's number and not the frame's.
        assert_eq!(h.slice.arena_offset(4 * 1024 * 1024).unwrap(), 3 * 4 * 1024 * 1024 + 8192);
    }

    #[test]
    fn a_slice_may_not_span_two_slabs_because_that_is_two_driver_imports() {
        let s = Slice { arena: 0, slab: 0, offset_in_slab: 4096, len: 64 * 1024 * 1024 };
        // D4: one VkDeviceMemory lives inside exactly one slab.
        assert!(matches!(
            s.validate(256 * 1024 * 1024, 64 * 1024 * 1024),
            Err(Error::LargerThanSlab { .. })
        ));
    }

    #[test]
    fn a_non_granular_offset_is_refused_because_the_allocator_cannot_have_produced_it() {
        let s = Slice { arena: 0, slab: 0, offset_in_slab: 100, len: 4096 };
        assert!(matches!(
            s.validate(1024 * 1024, 1024 * 1024),
            Err(Error::BadGeometry(_))
        ));
    }

    #[test]
    fn a_slice_outside_the_arena_is_refused() {
        let s = Slice { arena: 0, slab: 9, offset_in_slab: 0, len: 4096 };
        assert!(matches!(
            s.validate(4 * 4096, 4096),
            Err(Error::OutOfBounds { .. }) | Err(Error::LargerThanSlab { .. })
        ));
    }

    #[test]
    fn a_layout_longer_than_its_slice_is_refused_rather_than_read_past() {
        // One row short: the exact shape of "the compositor reads the next
        // allocation", which waycompose §17.3 narrowed peer_slice to prevent.
        let mut h = fixtures::firefox_content(1);
        h.slice.len -= 4096;
        assert!(matches!(
            h.validate(ARENA, SLAB),
            Err(Error::OutOfBounds { .. })
        ));
    }

    #[test]
    fn a_stride_narrower_than_a_row_is_refused() {
        let mut h = sample();
        h.stride = 399;
        assert!(matches!(
            h.validate(ARENA, SLAB),
            Err(Error::BadGeometry(_))
        ));
    }

    #[test]
    fn a_stride_wider_than_a_row_is_accepted_because_drivers_pad() {
        let mut h = sample();
        h.stride = 512;
        h.slice.len = crate::round_up(512 * 100);
        h.validate(ARENA, SLAB).unwrap();
    }

    #[test]
    fn the_format_numbers_are_wl_shms_own_so_no_table_can_drift() {
        // A compositor passes this straight to wl_shm_pool.create_buffer.
        assert_eq!(Format::Argb8888 as u32, 0);
        assert_eq!(Format::Xrgb8888 as u32, 1);
        assert_eq!(Format::from_u32(2), None, "an unknown format is refused, not guessed");
    }

    #[test]
    fn the_measured_firefox_frame_validates_against_a_64_mib_slab() {
        // waycompose §15.10: 1280x900x4 = 4,608,000 bytes, the content
        // subsurface Firefox really creates.
        let h = fixtures::firefox_content(7);
        assert_eq!(h.min_len().unwrap(), 4_608_000);
        h.validate(ARENA, SLAB).unwrap();
    }
}
