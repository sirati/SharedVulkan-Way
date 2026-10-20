//! `Locator::Probe`: one stamp per 2 MiB unit, and the host's probe for it
//! (waycompose-design.md §108.8).
//!
//! A Windows large page is 2 MiB, physically contiguous and aligned to 2 MiB
//! in guest-physical memory. QEMU maps guest RAM into its backend linearly per
//! RAM block, at offsets that are themselves 2 MiB aligned, so each large page
//! is also one 2 MiB-aligned run of the backend file. The host therefore never
//! has to translate an address and never has to sweep: it reads 64 bytes at
//! every 2 MiB of the backend, which for a 10 GiB guest is 5120 reads.
//!
//! The stamp carries a 128-bit nonce the provider drew for this one region,
//! so a stamp left in RAM by an earlier region, an earlier boot or another
//! session can never be taken for this one. A unit found twice is refused, not
//! resolved: two frames claiming to be the same unit is what a forged or stale
//! stamp looks like, and §84.5's relocated pages looked exactly like it.

pub const UNIT: u64 = 2 << 20;
pub const STAMP_LEN: usize = 64;
pub const MAGIC: [u8; 8] = *b"SVLP2M01";

/// FNV-1a 64 over the stamp's first 32 bytes: catches a torn or partial
/// stamp, not an adversary (the nonce is what makes a stamp this region's).
fn check(b: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &x in b {
        h ^= x as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// The stamp for unit `index` of `units` of the region with `nonce`.
pub fn stamp(nonce: &[u8; 16], index: u32, units: u32) -> [u8; STAMP_LEN] {
    let mut s = [0u8; STAMP_LEN];
    s[..8].copy_from_slice(&MAGIC);
    s[8..24].copy_from_slice(nonce);
    s[24..28].copy_from_slice(&index.to_le_bytes());
    s[28..32].copy_from_slice(&units.to_le_bytes());
    let c = check(&s[..32]);
    s[32..40].copy_from_slice(&c.to_le_bytes());
    s
}

/// A stamp's nonce, index and unit count, if it is a whole stamp.
pub fn decode(s: &[u8]) -> Option<([u8; 16], u32, u32)> {
    if s.len() < STAMP_LEN || s[..8] != MAGIC {
        return None;
    }
    if u64::from_le_bytes(s[32..40].try_into().ok()?) != check(&s[..32]) {
        return None;
    }
    let mut n = [0u8; 16];
    n.copy_from_slice(&s[8..24]);
    Some((n, u32::from_le_bytes(s[24..28].try_into().ok()?), u32::from_le_bytes(s[28..32].try_into().ok()?)))
}

/// One region the probe is to find.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Wanted {
    pub nonce: [u8; 16],
    pub units: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProbeError {
    /// `missing` of the region's units were not found.
    NotFound { missing: u32, first_missing: u32 },
    /// Unit `index` was found at two offsets.
    Duplicate { index: u32, a: u64, b: u64 },
    /// A stamp named this nonce with another unit count.
    Inconsistent { index: u32, units: u32 },
}

/// What the probe found for one region: the backend offset of each unit, in
/// unit order.
pub type Found = Result<Vec<u64>, ProbeError>;

/// How a probe went, for the logs.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ProbeStats {
    pub probes: u64,
    pub stamps_seen: u64,
}

/// Probe a backend of `len` bytes for every region in `wanted`. `read` fills a
/// stamp-sized buffer from a backend offset and returns false where the
/// offset cannot be read (not RAM, short file).
pub fn probe(len: u64, wanted: &[Wanted], mut read: impl FnMut(u64, &mut [u8; STAMP_LEN]) -> bool) -> (Vec<Found>, ProbeStats) {
    let mut slots: Vec<Vec<Option<u64>>> = wanted.iter().map(|w| vec![None; w.units as usize]).collect();
    let mut errs: Vec<Option<ProbeError>> = vec![None; wanted.len()];
    let mut stats = ProbeStats::default();
    let mut buf = [0u8; STAMP_LEN];
    let mut off = 0u64;
    while off + UNIT <= len {
        stats.probes += 1;
        if read(off, &mut buf) && buf[..8] == MAGIC
            && let Some((nonce, index, units)) = decode(&buf) {
                stats.stamps_seen += 1;
                if let Some(w) = wanted.iter().position(|w| w.nonce == nonce) {
                    if units != wanted[w].units || index >= units {
                        errs[w].get_or_insert(ProbeError::Inconsistent { index, units });
                    } else if let Some(prev) = slots[w][index as usize] {
                        errs[w].get_or_insert(ProbeError::Duplicate { index, a: prev, b: off });
                    } else {
                        slots[w][index as usize] = Some(off);
                    }
                }
            }
        off += UNIT;
    }
    let found = slots
        .into_iter()
        .zip(errs)
        .map(|(s, e)| {
            if let Some(e) = e {
                return Err(e);
            }
            let missing = s.iter().filter(|x| x.is_none()).count() as u32;
            if missing > 0 {
                let first_missing = s.iter().position(|x| x.is_none()).unwrap_or(0) as u32;
                return Err(ProbeError::NotFound { missing, first_missing });
            }
            Ok(s.into_iter().map(|x| x.unwrap()).collect())
        })
        .collect();
    (found, stats)
}

/// Coalesce unit offsets into `(file_offset, len)` runs, in unit order: what
/// the host maps, one `mmap` per run.
pub fn runs(offsets: &[u64]) -> Vec<(u64, u64)> {
    let mut out: Vec<(u64, u64)> = Vec::new();
    for &o in offsets {
        match out.last_mut() {
            Some((start, len)) if *start + *len == o => *len += UNIT,
            _ => out.push((o, UNIT)),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn backend(units: usize) -> Vec<u8> {
        vec![0u8; units * UNIT as usize]
    }
    fn put(b: &mut [u8], unit: usize, s: &[u8; STAMP_LEN]) {
        let o = unit * UNIT as usize;
        b[o..o + STAMP_LEN].copy_from_slice(s);
    }
    fn reader(b: &[u8]) -> impl FnMut(u64, &mut [u8; STAMP_LEN]) -> bool + '_ {
        move |o, buf| {
            buf.copy_from_slice(&b[o as usize..o as usize + STAMP_LEN]);
            true
        }
    }

    #[test]
    fn a_stamp_round_trips_and_a_torn_one_does_not() {
        let s = stamp(&[3; 16], 5, 9);
        assert_eq!(decode(&s), Some(([3; 16], 5, 9)));
        let mut t = s;
        t[26] ^= 1;
        assert_eq!(decode(&t), None);
    }

    #[test]
    fn two_regions_scattered_and_out_of_order_are_found_in_unit_order() {
        let mut b = backend(64);
        let (n1, n2) = ([1; 16], [2; 16]);
        // region 1: 3 units at 40, 7, 8; region 2: 2 units at 9, 63
        put(&mut b, 40, &stamp(&n1, 0, 3));
        put(&mut b, 7, &stamp(&n1, 1, 3));
        put(&mut b, 8, &stamp(&n1, 2, 3));
        put(&mut b, 9, &stamp(&n2, 0, 2));
        put(&mut b, 63, &stamp(&n2, 1, 2));
        // a stale stamp of another nonce
        put(&mut b, 20, &stamp(&[9; 16], 0, 3));
        let w = [Wanted { nonce: n1, units: 3 }, Wanted { nonce: n2, units: 2 }];
        let (f, st) = probe(b.len() as u64, &w, reader(&b));
        assert_eq!(st.probes, 64);
        assert_eq!(st.stamps_seen, 6);
        let u = UNIT;
        assert_eq!(f[0], Ok(vec![40 * u, 7 * u, 8 * u]));
        assert_eq!(f[1], Ok(vec![9 * u, 63 * u]));
        assert_eq!(runs(f[0].as_ref().unwrap()), vec![(40 * u, u), (7 * u, 2 * u)]);
    }

    #[test]
    fn a_unit_twice_or_missing_is_refused() {
        let mut b = backend(16);
        let n = [5; 16];
        put(&mut b, 1, &stamp(&n, 0, 2));
        put(&mut b, 2, &stamp(&n, 0, 2));
        let w = [Wanted { nonce: n, units: 2 }];
        let (f, _) = probe(b.len() as u64, &w, reader(&b));
        assert_eq!(f[0], Err(ProbeError::Duplicate { index: 0, a: UNIT, b: 2 * UNIT }));
        let mut b = backend(16);
        put(&mut b, 1, &stamp(&n, 0, 2));
        let (f, _) = probe(b.len() as u64, &w, reader(&b));
        assert_eq!(f[0], Err(ProbeError::NotFound { missing: 1, first_missing: 1 }));
    }

    #[test]
    fn a_stamp_not_on_a_unit_boundary_is_never_seen() {
        let mut b = backend(4);
        let n = [6; 16];
        let o = UNIT as usize + 4096;
        b[o..o + STAMP_LEN].copy_from_slice(&stamp(&n, 0, 1));
        let (f, _) = probe(b.len() as u64, &[Wanted { nonce: n, units: 1 }], reader(&b));
        assert!(f[0].is_err());
    }
}
