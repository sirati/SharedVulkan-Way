//! A Linux stand-in for a Windows guest's RAM, for the development loop and
//! the end-to-end harness (`scripts/sv-pool-grow-e2e.sh`): one shared file is
//! "guest RAM"; a region is 2 MiB units at scattered offsets of it, stamped
//! exactly as the keeper stamps a large-page section, and seen by the
//! provider through a stitched view. The host's peer locates it with the same
//! probe and maps it the same way as on Windows, so everything but the
//! substrate is the code the guest runs.

use crate::provider::{Alloc, Substrate};
use std::collections::BTreeMap;
use std::os::fd::AsFd;
use std::os::unix::fs::FileExt;
use sv_transport::lpstamp::{self, UNIT};
use sv_transport::region::{SharedRegion, StitchRun};
use sv_transport::regiontab::{Kind, win_large_handle};

pub struct SimGuestRam {
    file: std::fs::File,
    used: Vec<bool>,
    regions: BTreeMap<u32, Vec<u64>>,
    next: u32,
    seed: u64,
}

impl SimGuestRam {
    /// `file` must be a multiple of 2 MiB long.
    pub fn open(path: &str) -> Result<SimGuestRam, String> {
        let file = std::fs::OpenOptions::new().read(true).write(true).open(path).map_err(|e| format!("{path}: {e}"))?;
        let len = file.metadata().map_err(|e| e.to_string())?.len();
        let seed = std::process::id() as u64 ^ 0x9E37_79B9_7F4A_7C15;
        Ok(SimGuestRam { file, used: vec![false; (len / UNIT) as usize], regions: BTreeMap::new(), next: 1, seed })
    }

    fn rnd(&mut self) -> u64 {
        self.seed ^= self.seed << 13;
        self.seed ^= self.seed >> 7;
        self.seed ^= self.seed << 17;
        self.seed
    }

    pub fn free_bytes(&self) -> u64 {
        self.used.iter().filter(|u| !**u).count() as u64 * UNIT
    }
}

impl Substrate for SimGuestRam {
    fn kind(&self) -> Kind {
        Kind::WinLarge
    }

    fn alloc(&mut self, len: u64) -> Result<Alloc, String> {
        let t = std::time::Instant::now();
        let n = (len / UNIT) as usize;
        if !len.is_multiple_of(UNIT) || n == 0 {
            return Err("a region is a whole number of 2 MiB units".into());
        }
        if (self.free_bytes() / UNIT) < n as u64 {
            return Err(format!("budget: {} MiB asked, {} MiB free", len >> 20, self.free_bytes() >> 20));
        }
        let total = self.used.len() as u64;
        let mut pick = Vec::with_capacity(n);
        // Scattered like Windows' large pages: a random start, then the next
        // free unit, with a random skip now and then.
        let mut at = self.rnd() % total;
        while pick.len() < n {
            if !self.used[at as usize] {
                self.used[at as usize] = true;
                pick.push(at);
            }
            at = (at + 1 + if self.rnd().is_multiple_of(4) { self.rnd() % 8 } else { 0 }) % total;
        }
        let nonce = (self.rnd() as u128 | ((self.rnd() as u128) << 64)).to_le_bytes();
        for (i, &u) in pick.iter().enumerate() {
            self.file.write_all_at(&lpstamp::stamp(&nonce, i as u32, n as u32), u * UNIT).map_err(|e| e.to_string())?;
        }
        let mut ext = 0;
        let runs: Vec<StitchRun> = lpstamp::runs(&pick.iter().map(|u| u * UNIT).collect::<Vec<_>>())
            .into_iter()
            .map(|(file_offset, l)| {
                let r = StitchRun { file_offset, extent_offset: ext, len: l };
                ext += l;
                r
            })
            .collect();
        let view = SharedRegion::stitch(self.file.as_fd(), &runs).map_err(|e| e.to_string())?;
        let id = self.next;
        self.next += 1;
        self.regions.insert(id, pick);
        Ok(Alloc { id, handle: win_large_handle(nonce, n as u32, id), view, alloc_us: t.elapsed().as_micros() as u64 })
    }

    fn free(&mut self, id: u32) -> Result<(), String> {
        for u in self.regions.remove(&id).ok_or("no such region")? {
            self.used[u as usize] = false;
            // The guest would give the frame to someone else: scribble on it,
            // so a host that kept writing would be seen.
            let _ = self.file.write_all_at(&[0xA5; 64], u * UNIT);
        }
        Ok(())
    }
}
