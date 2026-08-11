//! The GPU-workload half of the shared record: what the compute shader is
//! required to compute, and what the Linux host is required to have written.
//!
//! Three parties have to agree here, not two — the Linux host writes the input,
//! a SPIR-V compute shader on the Intel iGPU transforms it, and the Windows
//! guest and the Linux host each check the result. If any pair of them kept
//! their own copy of these functions, a disagreement would look exactly like
//! the failure the experiment exists to detect: bytes that are not what the
//! other side thinks they are. So the functions live here once, and
//! `crates/gpu-probe/shaders/arena.comp` mirrors them in GLSL under a test
//! (`shader_matches_the_rust_definition`) that fails if the two ever drift.
//!
//! Everything is `u32` and everything wraps, because GLSL's `uint` arithmetic
//! wraps and a Rust `+` that panics on overflow in debug and wraps in release
//! would make the two sides agree only in release builds.
//!
//! # The page header is not part of the payload
//!
//! Each 4096-byte page keeps its first [`HEADER_BYTES`] bytes reserved for the
//! `SVARENA1` record defined in the parent module: the magic, the run id and
//! the page index. Neither the host's fill nor the shader touches them.
//!
//! That is deliberate, and it is the cheap version of R26's canary. Step 0's
//! per-page stamps were destroyed the moment real data landed in the pages,
//! which is precisely why R26 has no detector on the 4 KiB path. Here the
//! stamps survive the workload, so after the GPU has written the whole window
//! the host can still ask "is this page still the page I mapped?" and get an
//! answer. It costs 6.25% of the window and it converts a whole class of silent
//! wrong-page results into loud ones.

use crate::PAGE;

/// Bytes at the start of every page that belong to the `SVARENA1` record and
/// are never payload. The last record field ends at 148
/// (`OFF_HOST2_INDEX + 4`); 256 rounds that up to a cache-line multiple and
/// leaves room for the record to grow.
pub const HEADER_BYTES: usize = 256;

/// `u32`s per page that the header occupies.
pub const HEADER_WORDS: u32 = (HEADER_BYTES / 4) as u32;

/// `u32`s per page in total, header included.
pub const WORDS_PER_PAGE: u32 = (PAGE / 4) as u32;

/// Is this word index payload, or is it part of a page header?
///
/// The shader applies the same predicate — as `(i & 1023u) >= 64u` — so a word
/// the host skipped is a word the GPU skipped.
#[inline]
pub fn is_payload(word_index: u32) -> bool {
    word_index % WORDS_PER_PAGE >= HEADER_WORDS
}

/// What the Linux host writes into payload word `i` of the input region for a
/// run stamped `stamp`.
///
/// It has to depend on both, or the check degenerates. Depending on `stamp`
/// means a second host write is distinguishable from the first — the failure
/// where the GPU is reading a stale copy of the page shows up as "the values
/// are the previous run's", not as silence. Depending on `i` means a memset
/// cannot impersonate it and neither can a copy that is off by a page.
/// Folding the two halves of the stamp with a plain XOR looked fine and was
/// not: every stamp of the form `0xAAAAAAAA_AAAAAAAA` collapsed to zero, so two
/// different stamps could produce byte-identical fills and the concurrency
/// check would have had nothing to distinguish. Multiply each half by a
/// different odd constant first.
#[inline]
pub fn host_fill(stamp: u64, i: u32) -> u32 {
    let mut h = (stamp as u32).wrapping_mul(0x85EB_CA6B)
        ^ ((stamp >> 32) as u32).wrapping_mul(0xC2B2_AE35);
    h ^= i.wrapping_mul(0x9E37_79B1);
    h ^= h >> 16;
    h = h.wrapping_mul(0x7FEB_352D);
    h ^= h >> 15;
    h
}

/// What the compute shader must write into payload word `i` of the output
/// region, given that word `i` of the input region held `v`.
///
/// A finalising hash rather than something like `v + 1`: it depends on every
/// bit of `v` and on `i`, so the only ways to produce it are to have actually
/// read `v` and to have known which element you were. A GPU that read the wrong
/// page, read a stale page, or wrote a constant produces a value that is wrong
/// in every bit rather than nearly right, and the check cannot be passed by
/// accident.
///
/// `SEED` is not decoration. Without it the function has zero as a fixed point
/// — `transform(0, 0) == 0` — so word 0 of a page the GPU never touched would
/// have passed the check. That is the exact false positive this whole
/// experiment exists to avoid, and it survived until a unit test asked.
pub const SEED: u32 = 0x27D4_EB2F;

#[inline]
pub fn transform(v: u32, i: u32) -> u32 {
    let mut h = v ^ (v >> 15);
    h = h.wrapping_mul(0x2545_F491);
    h = h.wrapping_add(i.wrapping_mul(0x9E37_79B1)).wrapping_add(SEED);
    h ^= h >> 13;
    h = h.wrapping_mul(0x85EB_CA6B);
    h ^= h >> 16;
    h
}

/// The two things `--mode` on the dispatch selects, kept here because the host
/// verifier has to know which one it is checking.
///
/// `Transform` is the real workload. `Copy` exists for the concurrency phase:
/// while the host churns the input between two stamps, a copy kernel lets the
/// checker ask "is every word the host's old value or the host's new value?",
/// which needs no second hash inside the shader and detects a value that is
/// neither — the only thing a torn or incoherent read could produce.
pub const KERNEL_TRANSFORM: u32 = 0;
pub const KERNEL_COPY: u32 = 1;

/// How the window is split between the shader's two buffers.
///
/// Input is the first half, output the second, both page-aligned by
/// construction. The two `VkBuffer`s are bound to the *same* imported
/// `VkDeviceMemory` at offsets 0 and `input_bytes`, so a run also exercises
/// binding a buffer at a non-zero offset into imported memory rather than only
/// at zero.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Split {
    /// Pages in each half. The window is `2 * pages_per_half` pages.
    pub pages_per_half: usize,
}

impl Split {
    /// An odd page count would leave a page in neither half, so round down.
    pub fn for_window(total_pages: usize) -> Split {
        Split {
            pages_per_half: total_pages / 2,
        }
    }
    pub fn input_bytes(self) -> usize {
        self.pages_per_half * PAGE
    }
    /// Word indices the shader walks, header words included — it skips them
    /// itself, so that the index a word has is its index in the region and not
    /// a compacted one that the two sides could compute differently.
    pub fn words(self) -> u32 {
        (self.pages_per_half * PAGE / 4) as u32
    }
    /// Page index in the whole window of the output page holding word `i`.
    pub fn output_page_of_word(self, i: u32) -> usize {
        self.pages_per_half + (i / WORDS_PER_PAGE) as usize
    }
    /// Page index in the whole window of the input page holding word `i`.
    pub fn input_page_of_word(self, i: u32) -> usize {
        (i / WORDS_PER_PAGE) as usize
    }
    /// Byte offset of word `i` within its page.
    pub fn byte_in_page(self, i: u32) -> usize {
        (i % WORDS_PER_PAGE) as usize * 4
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The property every conclusion rests on: a transform of the right value
    /// at the right index is not a transform of anything else. If these
    /// collided, a wrong-page read could pass the check.
    #[test]
    fn the_transform_separates_value_and_index() {
        assert_ne!(transform(0, 0), transform(0, 1), "index must matter");
        assert_ne!(transform(0, 0), transform(1, 0), "value must matter");
        // A constant output is what a GPU that never ran would leave behind, so
        // make sure a constant cannot be right for two indices at once.
        let v = 0xDEAD_BEEF;
        assert_ne!(transform(v, 100), transform(v, 101));
        // And zero is not a fixed point: an untouched page reads as zero, and
        // if transform(v,i) were 0 for a zero input that page would look
        // correct. This failed the first time it was asked.
        for i in 0..4096u32 {
            assert_ne!(transform(0, i), 0, "zero is a fixed point at index {i}");
        }
    }

    /// A second host write has to be distinguishable from the first, or "the
    /// GPU read a stale page" is indistinguishable from success.
    #[test]
    fn the_fill_depends_on_the_stamp_and_the_index() {
        assert_ne!(host_fill(1, 7), host_fill(2, 7));
        assert_ne!(host_fill(1, 7), host_fill(1, 8));
        // The concurrency check asks "is this word fill(A,i) or fill(B,i)?",
        // which is only meaningful if the two differ essentially everywhere.
        let collisions = (0..100_000u32)
            .filter(|&i| host_fill(0xAAAA_AAAA_AAAA_AAAA, i) == host_fill(0x5555_5555_5555_5555, i))
            .count();
        assert_eq!(collisions, 0, "two stamps must not agree on real data");
    }

    /// Everything must wrap rather than panic, or the Rust side disagrees with
    /// GLSL exactly when a debug build is used.
    #[test]
    fn the_arithmetic_wraps_like_glsl_does() {
        // Would overflow a checked mul in a debug build if it were not wrapping.
        let _ = transform(u32::MAX, u32::MAX);
        let _ = host_fill(u64::MAX, u32::MAX);
    }

    /// Header words are skipped by both sides or by neither. An off-by-one here
    /// makes the host verify a word the GPU never wrote, which reads as a GPU
    /// failure.
    #[test]
    fn headers_are_skipped_and_payload_is_not() {
        assert!(!is_payload(0));
        assert!(!is_payload(HEADER_WORDS - 1));
        assert!(is_payload(HEADER_WORDS));
        assert!(is_payload(WORDS_PER_PAGE - 1));
        // ...and the pattern repeats every page, which is what the shader's
        // `(i & 1023) >= 64` relies on.
        assert!(!is_payload(WORDS_PER_PAGE));
        assert!(is_payload(WORDS_PER_PAGE + HEADER_WORDS));
        // The shader's bitwise form and this modular one must agree, since the
        // GLSL cannot call this function.
        for i in [0u32, 1, 63, 64, 1023, 1024, 1087, 1088, 70_000, 1_048_576] {
            assert_eq!(is_payload(i), (i & 1023) >= 64, "disagreement at {i}");
        }
    }

    /// The split has to keep the two halves disjoint and page-aligned, because
    /// the second `VkBuffer` is bound at `input_bytes` into the same memory.
    #[test]
    fn the_split_keeps_the_halves_disjoint() {
        let s = Split::for_window(16384);
        assert_eq!(s.pages_per_half, 8192);
        assert_eq!(s.input_bytes(), 8192 * PAGE);
        assert_eq!(s.words(), 8192 * 1024);
        // The last input word is on the last input page; the first output word
        // is on the first output page, and they are different pages.
        let last = s.words() - 1;
        assert_eq!(s.input_page_of_word(last), 8191);
        assert_eq!(s.output_page_of_word(0), 8192);
        assert!(s.input_page_of_word(last) < s.output_page_of_word(0));
        assert_eq!(s.byte_in_page(last), PAGE - 4);
        // An odd window must not produce a half that runs off the end.
        let odd = Split::for_window(3);
        assert!(odd.pages_per_half * 2 <= 3);
    }
}
