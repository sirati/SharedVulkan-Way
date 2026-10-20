//! A Linux guest's substrate for the region protocol (§108.3 kind
//! `WIN_LARGE`): 2 MiB hugetlb pages, stamped exactly as the Windows keeper
//! stamps its large-page sections. A hugetlb page is 2 MiB-aligned in the
//! guest's physical memory, so the host's probe (one stamp read per 2 MiB of
//! QEMU's RAM memfd) finds every unit, as on Windows.
//!
//! **Test substrate.** It is what the NixOS VM test's nested guest grows
//! from (`sv-ram-vmtest provider`). It does not satisfy the keeper rule
//! (§108.5): the pages are the process's own, so its death frees them at
//! once. That is acceptable for a test that never kills its provider, and the
//! reason it is not a product substrate.

use crate::provider::{Alloc, Substrate};
use std::collections::BTreeMap;
use sv_transport::lpstamp::{self, UNIT};
use sv_transport::region::SharedRegion;
use sv_transport::regiontab::{Kind, win_large_handle};

struct Held {
    addr: usize,
    len: usize,
}

pub struct HugeTlb {
    regions: BTreeMap<u32, Held>,
    next: u32,
    seed: u64,
}

impl Default for HugeTlb {
    fn default() -> Self {
        HugeTlb::new()
    }
}

impl HugeTlb {
    pub fn new() -> HugeTlb {
        let seed = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(1, |d| d.as_nanos() as u64) | 1;
        HugeTlb { regions: BTreeMap::new(), next: 1, seed }
    }

    fn rnd(&mut self) -> u64 {
        self.seed ^= self.seed << 13;
        self.seed ^= self.seed >> 7;
        self.seed ^= self.seed << 17;
        self.seed
    }
}

const MAP_HUGE_2MB: i32 = 21 << libc::MAP_HUGE_SHIFT;

impl Substrate for HugeTlb {
    fn kind(&self) -> Kind {
        Kind::WinLarge
    }

    fn alloc(&mut self, len: u64) -> Result<Alloc, String> {
        let t = std::time::Instant::now();
        if len == 0 || !len.is_multiple_of(UNIT) {
            return Err("a region is a whole number of 2 MiB units".into());
        }
        let n = (len / UNIT) as u32;
        // SAFETY: a fresh anonymous mapping at an address of the kernel's
        // choosing; the result is checked.
        let a = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len as usize,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | libc::MAP_HUGETLB | MAP_HUGE_2MB | libc::MAP_POPULATE | libc::MAP_LOCKED,
                -1,
                0,
            )
        };
        if a == libc::MAP_FAILED {
            return Err(format!("{} hugetlb pages: {}", n, std::io::Error::last_os_error()));
        }
        // SAFETY: the mapping made above, `len` bytes.
        let view = unsafe { SharedRegion::from_raw(a.cast(), len as usize) };
        let nonce = (self.rnd() as u128 | ((self.rnd() as u128) << 64)).to_le_bytes();
        for i in 0..n {
            view.write_bytes(u64::from(i) * UNIT, &lpstamp::stamp(&nonce, i, n)).map_err(|e| e.to_string())?;
        }
        let id = self.next;
        self.next += 1;
        self.regions.insert(id, Held { addr: a as usize, len: len as usize });
        Ok(Alloc { id, handle: win_large_handle(nonce, n, id), view, alloc_us: t.elapsed().as_micros() as u64 })
    }

    fn free(&mut self, id: u32) -> Result<(), String> {
        let h = self.regions.remove(&id).ok_or("no such region")?;
        // The page goes back to the guest kernel, which may give it to
        // anyone: scribble over the stamps first, so a host that kept writing
        // would be seen, and a stale stamp is never found again.
        // SAFETY: the mapping `alloc` made, still mapped.
        unsafe {
            let mut at = 0;
            while at < h.len {
                std::ptr::write_bytes((h.addr + at) as *mut u8, 0xA5, 64);
                at += UNIT as usize;
            }
            libc::munmap(h.addr as *mut libc::c_void, h.len);
        }
        Ok(())
    }
}
