//! The Windows substrate: regions are the keeper's large-page sections
//! (§108.7, §108.10). The keeper duplicates each unnamed section into this
//! process, so while it runs the memory has two holders: the keeper's handle,
//! and this process's handle and view.

use crate::keeper::KeeperClient;
use crate::provider::{Alloc, Substrate};
use std::collections::BTreeMap;
use sv_transport::region::SharedRegion;
use sv_transport::regiontab::{Kind, win_large_handle};
use sv_transport::winram::{LargeSection, LargeView};

pub struct KeeperSubstrate {
    pub keeper: KeeperClient,
    open: BTreeMap<u32, (Option<LargeView>, LargeSection)>,
    orphans: Vec<(u32, u64)>,
    /// Whether the last view got `FILE_MAP_LARGE_PAGES`.
    pub large_translations: bool,
}

impl KeeperSubstrate {
    pub fn connect(pool: &str) -> Result<KeeperSubstrate, String> {
        let (keeper, orphans) = KeeperClient::connect(pool)?;
        let mut open = BTreeMap::new();
        let mut list = Vec::new();
        for (id, len, h) in orphans {
            // SAFETY: the keeper duplicated this handle into this process for
            // us; we close it when the orphan is freed.
            open.insert(id, (None, unsafe { LargeSection::from_raw_handle(h, len) }));
            list.push((id, len));
        }
        Ok(KeeperSubstrate { keeper, open, orphans: list, large_translations: false })
    }
}

impl Substrate for KeeperSubstrate {
    fn kind(&self) -> Kind {
        Kind::WinLarge
    }

    fn alloc(&mut self, len: u64) -> Result<Alloc, String> {
        let g = self.keeper.alloc(len)?;
        // SAFETY: duplicated into this process for us alone.
        let section = unsafe { LargeSection::from_raw_handle(g.handle, len) };
        let v = match section.map() {
            Ok(v) => v,
            Err(e) => {
                drop(section);
                let _ = self.keeper.free(g.id);
                return Err(format!("mapping region {}: {e}", g.id));
            }
        };
        self.large_translations = v.large_translations;
        // SAFETY: the view lives in `self.open` until `free`, and the
        // provider drops its `SharedRegion` before calling `free`.
        let view = unsafe { SharedRegion::from_raw(v.base_ptr(), v.len()) };
        let units = (len / sv_transport::lpstamp::UNIT) as u32;
        self.open.insert(g.id, (Some(v), section));
        Ok(Alloc { id: g.id, handle: win_large_handle(g.nonce, units, g.id), view, alloc_us: g.keeper_us })
    }

    fn free(&mut self, id: u32) -> Result<(), String> {
        // Our view and handle first, then the keeper's: the last holder goes
        // only after the peer has let go.
        self.open.remove(&id);
        self.keeper.free(id)
    }

    fn orphans(&mut self) -> Vec<(u32, u64)> {
        std::mem::take(&mut self.orphans)
    }
}
