//! The Windows substrate: regions are the shared broker's large-page sections
//! (§108.7, §108.10). The broker duplicates each unnamed section into this
//! process. The broker's independent holder also retains each section after
//! a broker or provider crash. The region is freed only after UNMAPPED.
//! This process holds its own section handle and view.

use crate::provider::{Alloc, Substrate};
use std::collections::BTreeMap;
use std::os::windows::io::IntoRawHandle;
use std::time::Instant;
use sv_transport::region::SharedRegion;
use sv_transport::regiontab::{Kind, win_large_handle};
use sv_transport::winram::{LargeSection, LargeView};
use wc_broker::client::{Client, path};

pub struct KeeperSubstrate {
    broker: Client,
    pub run: String,
    pool: String,
    open: BTreeMap<u32, (Option<LargeView>, LargeSection)>,
    orphans: Vec<(u32, u64)>,
    /// Whether the last view got `FILE_MAP_LARGE_PAGES`.
    pub large_translations: bool,
}

impl KeeperSubstrate {
    pub fn connect(pool: &str) -> Result<KeeperSubstrate, String> {
        let mut broker = Client::connect(&path()).map_err(|e| e.to_string())?;
        let run = broker
            .hello("guest-ram-provider")
            .map_err(|e| e.to_string())?
            .run;
        let pool = format!("guest-ram.{pool}");
        let orphans = broker.adopt(&pool).map_err(|e| e.to_string())?;
        let mut open = BTreeMap::new();
        let mut list = Vec::new();
        for (id, len) in orphans {
            let id32 =
                u32::try_from(id).map_err(|_| "broker region ID exceeds the region protocol")?;
            let (_reply, mut handles) = broker.open(id).map_err(|e| e.to_string())?;
            if handles.len() != 1 {
                return Err("a guest RAM region must be one section".into());
            }
            let h = handles.remove(0).into_raw_handle() as isize;
            // SAFETY: the broker duplicated this handle into this process for
            // us; we close it when the orphan is freed.
            open.insert(
                id32, (None, unsafe { LargeSection::from_raw_handle(h, len) }),
            );
            list.push((id32, len));
        }
        Ok(KeeperSubstrate {
            broker,
            run,
            pool, open, orphans: list, large_translations: false,
        })
    }

    /// Instrumentation only. The broker owns every production region.
    pub fn stats(&mut self) -> Result<String, String> {
        self.broker.stats().map_err(|e| e.to_string())
    }
}

impl Substrate for KeeperSubstrate {
    fn kind(&self) -> Kind {
        Kind::WinLarge
    }

    fn alloc(&mut self, len: u64) -> Result<Alloc, String> {
        let started = Instant::now();
        let (reply, mut handles) = self.broker
            .alloc(&self.pool, len, true)
            .map_err(|e| e.to_string())?;
        let id = match u32::try_from(reply.id) {
            Ok(id) => id,
            Err(_) => {
                let _ = self.broker.free(reply.id);
                return Err("broker region ID exceeds the region protocol".into());
            }
        };
        let nonce = match reply.note.as_deref().and_then(crate::keeper::unhex16) {
            Some(nonce) if handles.len() == 1 && reply.len == len => nonce,
            _ => {
                let _ = self.broker.free(reply.id);
                return Err("broker did not return one stamped large-page section".into());
            }
        };
        // SAFETY: the broker duplicated this owned section into our process.
        let section = unsafe { LargeSection::from_raw_handle(handles.remove(0).into_raw_handle() as isize, len) };
        let v = match section.map() {
            Ok(v) => v,
            Err(e) => {
                drop(section);
                let _ = self.broker.free(u64::from(id));
                return Err(format!("mapping region {id}: {e}"));
            }
        };
        self.large_translations = v.large_translations;
        // SAFETY: the view lives in `self.open` until `free`, and the
        // provider drops its `SharedRegion` before calling `free`.
        let view = unsafe { SharedRegion::from_raw(v.base_ptr(), v.len()) };
        let units = (len / sv_transport::lpstamp::UNIT) as u32;
        self.open.insert(id, (Some(v), section));
        Ok(Alloc { id, handle: win_large_handle(nonce, units, id), view, alloc_us: started.elapsed().as_micros() as u64,
        })
    }

    fn free(&mut self, id: u32) -> Result<(), String> {
        // Our view and handle first, then the broker's: the last holder goes
        // only after the peer has let go.
        self.open.remove(&id);
        // Adopted regions acquired a lease through open; new allocations
        // have only their ownership. Closing a missing lease is harmless.
        let _ = self.broker.close(u64::from(id));
        self.broker.free(u64::from(id)).map_err(|e| e.to_string())
    }

    fn orphans(&mut self) -> Vec<(u32, u64)> {
        std::mem::take(&mut self.orphans)
    }
}
