//! The header at the front of an **ivshmem** arena, and nothing else.
//!
//! IVSHMEM ARENA (docs/waycompose-design.md §83). The guest compositor maps BAR2 of a
//! QEMU `ivshmem-plain` device; the VM host maps the file QEMU backs that BAR
//! with. Both are the same physical pages. What neither side has is any other
//! way to tell the other what it put there, so the first page of the region is
//! a header the guest writes and the host reads, and that page is **not part of
//! the arena**: the arena's offset 0 is the region's byte [`HEADER_BYTES`].
//!
//! # Why the header is outside the arena rather than at its offset 0
//!
//! Because the proxy's pool allocator is first-fit from offset 0 and would write
//! pixels over it the moment a client drew. That is §12.10's lesson about the
//! stamps, arriving a third time: anything a reader has to find later must not
//! live in bytes an allocator will hand out. A header that survives the session
//! is also what lets a proxy be restarted while the compositor is still up, and
//! lets a human read the region's identity off the host with `xxd`.
//!
//! # Why this file is platform-independent
//!
//! The writer is a Windows process and the reader is a Linux one, and the one
//! failure worth designing out is the two of them disagreeing about a field
//! offset. So the layout is encoded and decoded **here, once**, by the same
//! code on both sides, and tested on the machine that compiles it. Nothing in
//! this module touches a device.
//!
//! # The ordering rule
//!
//! The guest writes every field and then, after a release fence, the magic.
//! The host reads the magic first and the rest after an acquire fence. A reader
//! that sees the magic therefore sees a complete header, and a reader that sees
//! anything else sees "no header" — never a header with a stale run id in it.
//! [`invalidate`] is the other half: a compositor that is starting clears the
//! previous run's magic **before** it touches anything, so the header of a
//! compositor that has exited is not mistaken for the header of the one that is
//! starting.

use std::io;

/// The first eight bytes of a published header.
pub const MAGIC: [u8; 8] = *b"WCIVSHM1";
/// Bumped when the layout below changes. A reader refuses any version it does
/// not know. Version 2 (waycompose-design.md §89) added the Wayland rings'
/// region at the tail; a version-1 header reads as one with no rings.
pub const VERSION: u32 = 2;
/// Allocation-directory layout. Older version-2 readers must refuse it.
pub const LEASE_VERSION: u32 = 3;
/// How many bytes at the front of the region the header owns. One 4 KiB page,
/// because the arena behind it is allocated in pages and must start on one.
pub const HEADER_BYTES: u64 = 4096;

const OFF_MAGIC: usize = 0;
const OFF_VERSION: usize = 8;
const OFF_HEADER_BYTES: usize = 12;
const OFF_RUN_ID: usize = 16;
const OFF_GENERATION: usize = 24;
const OFF_REGION_LEN: usize = 32;
const OFF_ARENA_ID: usize = 40;
const OFF_OUTBOUND_OFF: usize = 48;
const OFF_OUTBOUND_LEN: usize = 56;
const OFF_RINGS_OFF: usize = 64;
const OFF_RINGS_LEN: usize = 72;
/// The number of bytes a header actually occupies. The rest of the page is
/// zero and reserved.
pub const ENCODED_LEN: usize = 80;
/// What a version-1 header occupied, which is all a reader may assume of one.
const ENCODED_LEN_V1: usize = 64;

/// What the guest says about the region it mapped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Header {
    /// Random per compositor start, never zero. The launcher passes the value
    /// the compositor printed to the host, which refuses a header carrying any
    /// other one: that is what stops a proxy attaching to the region a
    /// *previous* compositor described.
    pub run_id: u64,
    /// One more than the previous header's, or 1 when there was none. A count
    /// of compositor starts on this region since the file was created, for logs
    /// and for a human; the run id is what is checked.
    pub generation: u64,
    /// The whole device, header included, as the driver reported it.
    pub region_len: u64,
    /// The arena id the compositor registered the region under — the number in
    /// every `Descriptor::ArenaSlice` that names it.
    pub arena_id: u32,
    /// The compositor's outbound window, `(offset, len)`, in **arena** offsets
    /// (that is, relative to [`HEADER_BYTES`]). `(0, 0)` is "none".
    pub outbound: (u64, u64),
    /// The region that carries the session's Wayland connections as rings
    /// (waycompose-design.md §89), `(offset, len)` in **region** offsets — it
    /// is not part of the arena, so arena offsets cannot name it. Always the
    /// region's tail, so the arena is what lies between the header page and
    /// it. `(0, 0)` is "none": the compositor was not asked for rings, or could
    /// not open its doorbell, and every connection crosses the socket path.
    pub rings: (u64, u64),
}

impl Header {
    /// The arena's length: the region less the header page and the rings.
    pub fn arena_len(&self) -> u64 {
        self.region_len.saturating_sub(HEADER_BYTES).saturating_sub(self.rings.1)
    }

    /// The header's bytes, magic **included**. A writer must not copy this as
    /// one block — see [`write_into`] — but a test and a reader want it whole.
    pub fn encode(&self) -> [u8; ENCODED_LEN] {
        let mut b = [0u8; ENCODED_LEN];
        b[OFF_MAGIC..OFF_MAGIC + 8].copy_from_slice(&MAGIC);
        b[OFF_VERSION..OFF_VERSION + 4].copy_from_slice(&VERSION.to_le_bytes());
        b[OFF_HEADER_BYTES..OFF_HEADER_BYTES + 4]
            .copy_from_slice(&(HEADER_BYTES as u32).to_le_bytes());
        b[OFF_RUN_ID..OFF_RUN_ID + 8].copy_from_slice(&self.run_id.to_le_bytes());
        b[OFF_GENERATION..OFF_GENERATION + 8].copy_from_slice(&self.generation.to_le_bytes());
        b[OFF_REGION_LEN..OFF_REGION_LEN + 8].copy_from_slice(&self.region_len.to_le_bytes());
        b[OFF_ARENA_ID..OFF_ARENA_ID + 4].copy_from_slice(&self.arena_id.to_le_bytes());
        b[OFF_OUTBOUND_OFF..OFF_OUTBOUND_OFF + 8].copy_from_slice(&self.outbound.0.to_le_bytes());
        b[OFF_OUTBOUND_LEN..OFF_OUTBOUND_LEN + 8].copy_from_slice(&self.outbound.1.to_le_bytes());
        b[OFF_RINGS_OFF..OFF_RINGS_OFF + 8].copy_from_slice(&self.rings.0.to_le_bytes());
        b[OFF_RINGS_LEN..OFF_RINGS_LEN + 8].copy_from_slice(&self.rings.1.to_le_bytes());
        b
    }

    /// Read a header, refusing everything that is not exactly one.
    ///
    /// Every refusal names what it saw, because the reader is a launcher on
    /// another machine and "bad header" is not a message anybody can act on.
    /// A version-1 header is read as one with no rings: it is what a compositor
    /// from before §89 wrote, and the fields it has mean what they meant.
    /// Require the allocation-directory format before touching control/pixels.
    pub fn decode_for_leases(b: &[u8]) -> io::Result<Header> {
        if b.len() < ENCODED_LEN || u32::from_le_bytes(b[OFF_VERSION..OFF_VERSION + 4].try_into().unwrap()) != LEASE_VERSION {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "BAR allocation directory requires lease header format 3"));
        }
        Self::decode(b)
    }
    /// Encode the incompatible allocation-directory format explicitly.
    pub fn encode_leases(&self) -> [u8; ENCODED_LEN] {
        let mut bytes = self.encode();
        bytes[OFF_VERSION..OFF_VERSION + 4].copy_from_slice(&LEASE_VERSION.to_le_bytes());
        bytes
    }
    pub fn decode(b: &[u8]) -> io::Result<Header> {
        let bad = |m: String| io::Error::new(io::ErrorKind::InvalidData, m);
        if b.len() < ENCODED_LEN_V1 {
            return Err(bad(format!("{} bytes is shorter than a header", b.len())));
        }
        let u32_at = |o: usize| u32::from_le_bytes(b[o..o + 4].try_into().unwrap());
        let u64_at = |o: usize| u64::from_le_bytes(b[o..o + 8].try_into().unwrap());
        if b[OFF_MAGIC..OFF_MAGIC + 8] != MAGIC {
            return Err(bad(format!(
                "no header (magic {:02x?}): no compositor has published this region, or the \
                 one that is starting has not finished",
                &b[OFF_MAGIC..OFF_MAGIC + 8]
            )));
        }
        let version = u32_at(OFF_VERSION);
        if version != 1 && version != VERSION && version != LEASE_VERSION {
            return Err(bad(format!(
                "header version {version}, and this reader knows 1, {VERSION} and {LEASE_VERSION}"
            )));
        }
        if version != 1 && b.len() < ENCODED_LEN {
            return Err(bad(format!("{} bytes is shorter than a version-{VERSION} header", b.len())));
        }
        let header_bytes = u32_at(OFF_HEADER_BYTES) as u64;
        if header_bytes != HEADER_BYTES {
            return Err(bad(format!(
                "the header says it is {header_bytes} bytes and this reader puts the arena \
                 after {HEADER_BYTES}"
            )));
        }
        let h = Header {
            run_id: u64_at(OFF_RUN_ID),
            generation: u64_at(OFF_GENERATION),
            region_len: u64_at(OFF_REGION_LEN),
            arena_id: u32_at(OFF_ARENA_ID),
            outbound: (u64_at(OFF_OUTBOUND_OFF), u64_at(OFF_OUTBOUND_LEN)),
            rings: if version == 1 { (0, 0) } else { (u64_at(OFF_RINGS_OFF), u64_at(OFF_RINGS_LEN)) },
        };
        if h.run_id == 0 {
            return Err(bad("run id 0, which no compositor writes".into()));
        }
        if h.region_len <= HEADER_BYTES || !h.region_len.is_multiple_of(HEADER_BYTES) {
            return Err(bad(format!(
                "a {}-byte region cannot hold a {HEADER_BYTES}-byte header and a whole number \
                 of pages after it",
                h.region_len
            )));
        }
        let (roff, rlen) = h.rings;
        if rlen != 0
            && (!roff.is_multiple_of(HEADER_BYTES)
                || !rlen.is_multiple_of(HEADER_BYTES)
                || roff.checked_add(rlen) != Some(h.region_len)
                || roff <= HEADER_BYTES)
        {
            return Err(bad(format!(
                "rings at {roff}+{rlen} are not whole pages at the tail of a {}-byte region, \
                 after its header",
                h.region_len
            )));
        }
        if rlen == 0 && roff != 0 {
            return Err(bad(format!("rings at {roff} with no length")));
        }
        let (off, len) = h.outbound;
        if len != 0 && off.checked_add(len) != Some(h.arena_len()) {
            return Err(bad(format!(
                "an outbound window at {off}+{len} is not the tail of a {}-byte arena",
                h.arena_len()
            )));
        }
        Ok(h)
    }
}

/// Publish `h` into the first page of a region: every field, then a release
/// fence, then the magic.
///
/// # Safety
///
/// `page` must be valid for writes of [`ENCODED_LEN`] bytes for the duration of
/// the call. Concurrent readers are the point; they are on the other side of a
/// hypervisor and synchronise through the magic.
pub unsafe fn write_into(page: *mut u8, h: &Header) {
    let bytes = h.encode();
    // SAFETY: inherited caller contract.
    unsafe { write_encoded(page, &bytes); }
}
/// Publish allocation-directory format 3 with the same release ordering.
///
/// # Safety
/// page must cover ENCODED_LEN writable retained bytes, as write_into.
pub unsafe fn write_leases_into(page: *mut u8, h: &Header) {
    let bytes = h.encode_leases();
    // SAFETY: inherited caller contract.
    unsafe { write_encoded(page, &bytes); }
}
unsafe fn write_encoded(page: *mut u8, bytes: &[u8; ENCODED_LEN]) {
    // SAFETY: the caller promises `page` covers `ENCODED_LEN` bytes. Volatile,
    // because the reader is not in this program and the compiler must not
    // merge, reorder or drop these stores on the grounds that nothing here
    // reads them back.
    unsafe {
        for (i, b) in bytes.iter().enumerate().skip(8) {
            page.add(i).write_volatile(*b);
        }
        std::sync::atomic::fence(std::sync::atomic::Ordering::Release);
        for (i, b) in bytes.iter().enumerate().take(8) {
            page.add(i).write_volatile(*b);
        }
    }
}

/// Clear the magic so that the region reads as "no header" until the next
/// [`write_into`]. Returns the generation the previous header carried, if it
/// had one, so that the next can be one more.
///
/// # Safety
///
/// As [`write_into`].
pub unsafe fn invalidate(page: *mut u8) -> Option<u64> {
    let mut old = [0u8; ENCODED_LEN];
    // SAFETY: the caller promises `page` covers `ENCODED_LEN` bytes.
    unsafe {
        for (i, b) in old.iter_mut().enumerate() {
            *b = page.add(i).read_volatile();
        }
        for i in 0..8 {
            page.add(i).write_volatile(0);
        }
        std::sync::atomic::fence(std::sync::atomic::Ordering::Release);
    }
    Header::decode(&old).ok().map(|h| h.generation)
}

/// Which of `paths` — ivshmem device interface paths — is the device in
/// root-bus slot `slot`, function 0 (waycompose-design.md §87).
///
/// A device interface path ends `#<instance id>#{guid}`, and a PCI instance
/// id's last `&`-component is the devfn in hex, `slot << 3 | function`: the
/// compositor's device at slot 0x1c is `…&0&E0`, the GPU arena's at 0x1d is
/// `…&0&E8`. Matched case-insensitively and on the whole last component only.
/// Here rather than in `platform::windows` because it is string handling,
/// which this machine can test.
pub fn path_for_slot(paths: &[String], slot: u8) -> Option<String> {
    let devfn = format!("{:02x}", u16::from(slot) << 3);
    paths
        .iter()
        .find(|p| {
            let lower = p.to_ascii_lowercase();
            let mut parts = lower.rsplitn(3, '#');
            let _guid = parts.next();
            let instance = parts.next().unwrap_or("");
            instance.rsplit('&').next() == Some(devfn.as_str())
        })
        .cloned()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The two devices testhost has (§83, §87), as the setup API spells
    /// them; slot 0x1c is `E0`, 0x1d is `E8`, and a slot with no device is
    /// `None` rather than a near miss.
    #[test]
    fn a_slot_names_one_device_by_its_devfn() {
        let paths = vec![
            r"\\?\pci#ven_1af4&dev_1110&subsys_11001af4&rev_01#3&267a616a&0&e0#{df576976-569d-4672-95a0-f57e4ea0b210}".to_string(),
            r"\\?\pci#ven_1af4&dev_1110&subsys_11001af4&rev_01#3&267a616a&0&e8#{df576976-569d-4672-95a0-f57e4ea0b210}".to_string(),
        ];
        assert_eq!(path_for_slot(&paths, 0x1c).as_deref(), Some(paths[0].as_str()));
        assert_eq!(path_for_slot(&paths, 0x1d).as_deref(), Some(paths[1].as_str()));
        assert_eq!(path_for_slot(&paths, 0x1e), None);
        // A devfn that appears as an earlier component of the instance id is
        // not the device's; only the last component counts.
        assert_eq!(path_for_slot(&paths, 0x01), None);
        let tricky = vec![r"\\?\pci#ven_1af4&dev_1110#3&e0&0&e8#{guid}".to_string()];
        assert_eq!(path_for_slot(&tricky, 0x1c), None);
        assert_eq!(path_for_slot(&tricky, 0x1d).as_deref(), Some(tricky[0].as_str()));
        let upper: Vec<String> = paths.iter().map(|p| p.to_ascii_uppercase()).collect();
        assert_eq!(path_for_slot(&upper, 0x1d).as_deref(), Some(upper[1].as_str()));
    }

    fn h() -> Header {
        Header {
            run_id: 0x1b44_0000_dead_beef,
            generation: 3,
            region_len: 128 << 20,
            arena_id: 0,
            outbound: ((128 << 20) - HEADER_BYTES - (1 << 20), 1 << 20),
            rings: (0, 0),
        }
    }

    #[test]
    fn allocation_layout_has_explicit_incompatible_format_boundary() {
        let legacy = h().encode();
        assert!(Header::decode_for_leases(&legacy).is_err());
        let leased = h().encode_leases();
        let version = u32::from_le_bytes(leased[8..12].try_into().unwrap());
        assert_eq!(version, 3);
        assert!(version != 1 && version != VERSION, "legacy reader must refuse lease bytes");
        assert_eq!(Header::decode_for_leases(&leased).unwrap(), h());
        assert!(Header::decode_for_leases(&leased[..64]).is_err());
        let mut page = [0u8; ENCODED_LEN];
        // SAFETY: owned complete output header.
        unsafe { write_leases_into(page.as_mut_ptr(), &h()); }
        assert_eq!(page, leased);
    }
    #[test]
    fn a_header_round_trips_and_the_layout_is_the_one_written_down() {
        let b = h().encode();
        assert_eq!(&b[0..8], b"WCIVSHM1");
        // Offsets pinned as numbers: a reordering of the struct must not move
        // a field on one side of the hypervisor and not the other.
        assert_eq!(u64::from_le_bytes(b[16..24].try_into().unwrap()), 0x1b44_0000_dead_beef);
        assert_eq!(u64::from_le_bytes(b[32..40].try_into().unwrap()), 128 << 20);
        assert_eq!(Header::decode(&b).unwrap(), h());
        assert_eq!(h().arena_len(), (128 << 20) - 4096);
    }

    #[test]
    fn everything_that_is_not_exactly_a_header_is_refused_by_name() {
        let mut b = h().encode();
        b[0] = 0;
        assert!(Header::decode(&b).unwrap_err().to_string().contains("no header"));
        let mut b = h().encode();
        b[8] = 4;
        assert!(Header::decode(&b).unwrap_err().to_string().contains("version 4"));
        let mut z = h();
        z.run_id = 0;
        assert!(Header::decode(&z.encode()).is_err());
        let mut w = h();
        w.outbound = (0, 1 << 20);
        assert!(Header::decode(&w.encode()).unwrap_err().to_string().contains("not the tail"));
        let mut s = h();
        s.region_len = HEADER_BYTES;
        assert!(Header::decode(&s.encode()).is_err());
        assert!(Header::decode(&[0u8; 10]).is_err());
    }

    /// The magic goes last and is the only thing a reader trusts, and
    /// `invalidate` hands back the generation so the next one can count on.
    #[test]
    fn publish_then_invalidate_on_a_page_of_memory() {
        let mut page = vec![0u8; HEADER_BYTES as usize];
        let p = page.as_mut_ptr();
        // SAFETY: a live, page-sized local buffer.
        unsafe {
            assert_eq!(invalidate(p), None, "a zero page has no header");
            write_into(p, &h());
        }
        assert_eq!(Header::decode(&page).unwrap(), h());
        // SAFETY: as above.
        let prev = unsafe { invalidate(page.as_mut_ptr()) };
        assert_eq!(prev, Some(3));
        assert!(Header::decode(&page).is_err(), "an invalidated header must not read");
        // The fields are still there for a human with xxd; only the magic went.
        assert_eq!(u64::from_le_bytes(page[24..32].try_into().unwrap()), 3);
    }

    /// §89. Rings at the tail: the arena shrinks by them, the outbound window
    /// is the tail of what is left, and the offsets are pinned as numbers.
    fn with_rings() -> Header {
        let rings = 6 << 20;
        let arena = (128 << 20) - HEADER_BYTES - rings;
        Header { outbound: (arena - (1 << 20), 1 << 20), rings: ((128 << 20) - rings, rings), ..h() }
    }

    #[test]
    fn a_version_2_header_carries_the_rings_at_the_regions_tail() {
        let r = with_rings();
        let b = r.encode();
        assert_eq!(u32::from_le_bytes(b[8..12].try_into().unwrap()), 2);
        assert_eq!(u64::from_le_bytes(b[64..72].try_into().unwrap()), (128 << 20) - (6 << 20));
        assert_eq!(u64::from_le_bytes(b[72..80].try_into().unwrap()), 6 << 20);
        assert_eq!(Header::decode(&b).unwrap(), r);
        assert_eq!(r.arena_len(), (128 << 20) - 4096 - (6 << 20));
        assert_eq!(r.rings.0, HEADER_BYTES + r.arena_len(), "the rings start where the arena ends");
    }

    #[test]
    fn rings_anywhere_but_the_tail_are_refused() {
        let mut r = with_rings();
        r.rings.0 -= 4096;
        assert!(Header::decode(&r.encode()).unwrap_err().to_string().contains("tail"));
        let mut r = with_rings();
        r.rings = (HEADER_BYTES, (128 << 20) - HEADER_BYTES);
        r.outbound = (0, 0);
        assert!(Header::decode(&r.encode()).is_err(), "rings that leave no arena");
        let mut r = with_rings();
        r.rings.1 = 0;
        assert!(Header::decode(&r.encode()).is_err(), "an offset with no length");
        // An outbound window that is the tail of the region rather than of the
        // arena is the mistake a writer that forgot the rings would make.
        let mut r = with_rings();
        r.outbound = ((128 << 20) - HEADER_BYTES - (1 << 20), 1 << 20);
        assert!(Header::decode(&r.encode()).unwrap_err().to_string().contains("not the tail"));
    }

    /// A compositor from before §89 wrote version 1 and 64 bytes. It must still
    /// read, as a header with no rings, so a host updated first is not a host
    /// that refuses the guest it has.
    #[test]
    fn a_version_1_header_reads_as_one_without_rings() {
        let mut b = h().encode();
        b[8..12].copy_from_slice(&1u32.to_le_bytes());
        b[64..80].copy_from_slice(&[0xee; 16]);
        assert_eq!(Header::decode(&b[..64]).unwrap(), h());
        assert_eq!(Header::decode(&b).unwrap().rings, (0, 0), "v1 bytes past 64 are not rings");
    }
}
