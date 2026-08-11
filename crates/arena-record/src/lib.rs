//! The page record the two sides of the step-0 experiment write into shared
//! pages, and the only definition of it.
//!
//! Both sides depend on this crate rather than each keeping its own copy of the
//! offsets. That is not tidiness: the experiment's whole value is that a match
//! means the two sides really are looking at the same physical page, and two
//! independently-typed layouts that happen to agree on the magic but disagree
//! on a field offset would turn a genuine negative into a confusing one.
//!
//! One record per 4096-byte page. Every writer stamps the page index it
//! believes it is writing, so a translation that is off by a page — the exact
//! failure R21 predicts — shows up as a mismatched index rather than as
//! silence.

pub const PAGE: usize = 4096;

/// Page header, written by the guest when it first fills the window.
pub const OFF_MAGIC: usize = 0; // b"SVARENA1"
pub const OFF_RUN_ID: usize = 8; // u64, random per run
pub const OFF_PAGE_INDEX: usize = 16; // u32, index within the window
pub const OFF_MODE: usize = 20; // u32, see MODE_*
pub const OFF_GPA: usize = 24; // u64, guest's belief about the GPA (0 = unknown)
pub const OFF_GUEST_TAG: usize = 32; // b"GUESTPAT"

/// Written by the Linux host, through the memfd, before the Vulkan import.
pub const OFF_HOST1: usize = 64; // b"HOSTWRT1"
pub const OFF_HOST1_STAMP: usize = 72; // u64
pub const OFF_HOST1_INDEX: usize = 80; // u32, host's belief about the page index

/// Written by the guest *through the Vulkan mapping* of the imported memory.
pub const OFF_VK: usize = 96; // b"VKWROTE1"
pub const OFF_VK_STAMP: usize = 104; // u64
pub const OFF_VK_INDEX: usize = 112; // u32

/// Written by the host while the guest still holds the Vulkan import, to show
/// the sharing survives the import rather than merely preceding it.
pub const OFF_HOST2: usize = 128; // b"HOSTWRT2"
pub const OFF_HOST2_STAMP: usize = 136; // u64
pub const OFF_HOST2_INDEX: usize = 144; // u32

pub const MAGIC: &[u8; 8] = b"SVARENA1";
pub const GUEST_TAG: &[u8; 8] = b"GUESTPAT";
pub const HOST1_TAG: &[u8; 8] = b"HOSTWRT1";
pub const VK_TAG: &[u8; 8] = b"VKWROTE1";
pub const HOST2_TAG: &[u8; 8] = b"HOSTWRT2";

/// Filler for the rest of the page, so a partially-written page is obvious.
pub const FILL: u8 = 0xAB;

pub const MODE_AWE: u32 = 1;
pub const MODE_LOCKED_VA: u32 = 0;
pub const MODE_LARGE_PAGES: u32 = 2;
pub const MODE_AWE2_LARGE: u32 = 3;

pub fn put_u32(page: &mut [u8], off: usize, v: u32) {
    page[off..off + 4].copy_from_slice(&v.to_le_bytes());
}

pub fn put_u64(page: &mut [u8], off: usize, v: u64) {
    page[off..off + 8].copy_from_slice(&v.to_le_bytes());
}

pub fn get_u32(page: &[u8], off: usize) -> u32 {
    u32::from_le_bytes(page[off..off + 4].try_into().unwrap())
}

pub fn get_u64(page: &[u8], off: usize) -> u64 {
    u64::from_le_bytes(page[off..off + 8].try_into().unwrap())
}

pub fn tag_at(page: &[u8], off: usize) -> [u8; 8] {
    page[off..off + 8].try_into().unwrap()
}

/// The bytes a fresh guest page should contain.
pub fn make_guest_page(run_id: u64, page_index: u32, mode: u32, gpa: u64) -> [u8; PAGE] {
    let mut p = [FILL; PAGE];
    p[OFF_MAGIC..OFF_MAGIC + 8].copy_from_slice(MAGIC);
    put_u64(&mut p, OFF_RUN_ID, run_id);
    put_u32(&mut p, OFF_PAGE_INDEX, page_index);
    put_u32(&mut p, OFF_MODE, mode);
    put_u64(&mut p, OFF_GPA, gpa);
    p[OFF_GUEST_TAG..OFF_GUEST_TAG + 8].copy_from_slice(GUEST_TAG);
    // The slots the other side will fill are zeroed rather than left as filler,
    // so "not written yet" is distinguishable from "written with garbage".
    for off in [OFF_HOST1, OFF_VK, OFF_HOST2] {
        for b in &mut p[off..off + 24] {
            *b = 0;
        }
    }
    p
}

/// Is this page one of ours, from this run?
pub fn is_ours(page: &[u8], run_id: u64) -> bool {
    page.len() >= PAGE && &tag_at(page, OFF_MAGIC) == MAGIC && get_u64(page, OFF_RUN_ID) == run_id
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The slots the three writers use must not overlap, or a "match" on one
    /// side would be another side's bytes. This is the only property of the
    /// layout that the experiment's conclusions actually rest on.
    #[test]
    fn writer_slots_are_disjoint_and_inside_the_page() {
        let mut slots = [
            (OFF_MAGIC, 40), // magic .. guest tag inclusive
            (OFF_HOST1, 24),
            (OFF_VK, 24),
            (OFF_HOST2, 24),
        ];
        slots.sort();
        for w in slots.windows(2) {
            assert!(
                w[0].0 + w[0].1 <= w[1].0,
                "slot at {} overlaps the one at {}",
                w[0].0,
                w[1].0
            );
        }
        let last = slots[slots.len() - 1];
        assert!(last.0 + last.1 <= PAGE);
    }

    #[test]
    fn a_fresh_guest_page_identifies_itself() {
        let p = make_guest_page(0xdead_beef_1234_5678, 42, MODE_AWE, 0x1_0000_0000);
        assert!(is_ours(&p, 0xdead_beef_1234_5678));
        assert!(
            !is_ours(&p, 0xdead_beef_1234_5679),
            "a different run must not match"
        );
        assert_eq!(get_u32(&p, OFF_PAGE_INDEX), 42);
        assert_eq!(get_u32(&p, OFF_MODE), MODE_AWE);
        assert_eq!(get_u64(&p, OFF_GPA), 0x1_0000_0000);
        assert_eq!(&tag_at(&p, OFF_GUEST_TAG), GUEST_TAG);
    }

    /// "Not written yet" has to be distinguishable from "written", or the host
    /// and Vulkan checks would pass against filler bytes.
    #[test]
    fn the_other_sides_slots_start_empty() {
        let p = make_guest_page(1, 0, MODE_LOCKED_VA, 0);
        for off in [OFF_HOST1, OFF_VK, OFF_HOST2] {
            assert_eq!(tag_at(&p, off), [0u8; 8]);
            assert_eq!(get_u64(&p, off + 8), 0);
        }
        assert_eq!(p[PAGE - 1], FILL, "the tail should still be filler");
    }

    #[test]
    fn scalars_round_trip_little_endian() {
        let mut p = [0u8; PAGE];
        put_u64(&mut p, OFF_HOST1_STAMP, 0x0bad_c0de_5555_aaaa);
        put_u32(&mut p, OFF_HOST1_INDEX, 16383);
        assert_eq!(get_u64(&p, OFF_HOST1_STAMP), 0x0bad_c0de_5555_aaaa);
        assert_eq!(get_u32(&p, OFF_HOST1_INDEX), 16383);
        assert_eq!(
            p[OFF_HOST1_STAMP], 0xaa,
            "little-endian, as both sides assume"
        );
    }
}
