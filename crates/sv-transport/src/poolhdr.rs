//! The pool header: the first page of the GPU pool's device, written by the
//! pool's owner in the guest and read by the VM host (waycompose-design.md
//! §97).
//!
//! The pool's first 64 KiB are never leased. They carry what the host needs
//! to tell that the file it opened is the pool a session's `Hello` names, and
//! that the pool has a live owner:
//!
//! ```text
//! offset  field
//!      0  magic "SVPOOL01" (written last, after the rest)
//!      8  version (1)
//!     16  nonce: this owner's run id; a new owner, a new nonce
//!     24  beat: incremented about once a second while the owner lives
//! ```
//!
//! Only the guest writes it. A session's `Hello` carries the nonce
//! (`Locator::IvshmemPool`), and `sv-hostd` refuses a session whose nonce is
//! not the one in the header: a host pointed at the wrong file, or a `Hello`
//! from a previous owner that a new one has replaced, fails at attach and
//! says which.

use crate::region::SharedRegion;
use std::sync::atomic::Ordering;

pub const MAGIC: [u8; 8] = *b"SVPOOL01";
pub const VERSION: u32 = 1;
/// Bytes at the start of the pool that are never leased: one 64 KiB granule.
pub const HEADER_LEN: u64 = 64 << 10;

const OFF_MAGIC: u64 = 0;
const OFF_VERSION: u64 = 8;
const OFF_NONCE: u64 = 16;
const OFF_BEAT: u64 = 24;

/// The header as read at one moment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Snapshot {
    pub valid: bool,
    pub nonce: u64,
    pub beat: u64,
}

impl Snapshot {
    /// Read from the header's bytes, as a host that `pread`s the file does.
    pub fn decode(b: &[u8]) -> Snapshot {
        if b.len() < 32 {
            return Snapshot::default();
        }
        let u32_at = |o: usize| u32::from_le_bytes(b[o..o + 4].try_into().unwrap_or([0; 4]));
        let u64_at = |o: usize| u64::from_le_bytes(b[o..o + 8].try_into().unwrap_or([0; 8]));
        Snapshot {
            valid: b[0..8] == MAGIC && u32_at(OFF_VERSION as usize) == VERSION,
            nonce: u64_at(OFF_NONCE as usize),
            beat: u64_at(OFF_BEAT as usize),
        }
    }
}

/// The header of a mapped pool.
pub struct PoolHeader<'a> {
    region: &'a SharedRegion,
}

impl<'a> PoolHeader<'a> {
    pub fn new(region: &'a SharedRegion) -> crate::Result<PoolHeader<'a>> {
        region.check(0, HEADER_LEN)?;
        Ok(PoolHeader { region })
    }

    fn u64(&self, off: u64) -> &std::sync::atomic::AtomicU64 {
        self.region.atomic_u64(off).expect("inside the checked header")
    }

    /// The owner's start: the fields, then the magic.
    pub fn publish(&self, nonce: u64) {
        self.region.write_bytes(OFF_MAGIC, &[0; 8]).expect("inside the header");
        self.region.atomic_u32(OFF_VERSION).expect("inside the header").store(VERSION, Ordering::Relaxed);
        self.u64(OFF_NONCE).store(nonce, Ordering::Relaxed);
        self.u64(OFF_BEAT).store(0, Ordering::Relaxed);
        std::sync::atomic::fence(Ordering::Release);
        self.region.write_bytes(OFF_MAGIC, &MAGIC).expect("inside the header");
    }

    pub fn beat(&self) {
        self.u64(OFF_BEAT).fetch_add(1, Ordering::Relaxed);
    }

    pub fn snapshot(&self) -> Snapshot {
        let mut b = [0u8; 32];
        let _ = self.region.read_bytes(0, &mut b);
        std::sync::atomic::fence(Ordering::Acquire);
        Snapshot::decode(&b)
    }
}

/// The host-side file of pool region `k`, from region 0's: `gpu` is region 0
/// and `gpu-3` is region 3. One rule for every reader. Today the pool is one
/// region, the GPU ivshmem device; the rule is what a second, statically
/// configured device would be named by.
pub fn region_path(boot: &str, k: u32) -> String {
    if k == 0 { boot.to_string() } else { format!("{boot}-{k}") }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_owner_publishes_and_beats_and_a_reader_of_bytes_agrees() {
        let r = SharedRegion::heap(HEADER_LEN as usize).unwrap();
        let h = PoolHeader::new(&r).unwrap();
        assert!(!h.snapshot().valid);
        h.publish(0xfeed);
        h.beat();
        h.beat();
        assert_eq!(h.snapshot(), Snapshot { valid: true, nonce: 0xfeed, beat: 2 });
        let mut b = [0u8; 32];
        r.read_bytes(0, &mut b).unwrap();
        assert_eq!(Snapshot::decode(&b), h.snapshot());
        // A new owner, a new nonce.
        h.publish(0xbeef);
        assert_eq!(h.snapshot(), Snapshot { valid: true, nonce: 0xbeef, beat: 0 });
        assert!(!Snapshot::decode(&[0u8; 32]).valid);
        assert!(!Snapshot::decode(&b[..8]).valid);
        assert_eq!(region_path("/run/windows-vm/wc-arena/gpu", 0), "/run/windows-vm/wc-arena/gpu");
        assert_eq!(region_path("/run/windows-vm/wc-arena/gpu", 3), "/run/windows-vm/wc-arena/gpu-3");
    }
}
