//! The provider's half of §108.4: announce, learn the answer, retire, free.
//!
//! Platform-neutral. What allocates and frees the memory is a [`Substrate`]:
//! the keeper's large-page sections on Windows ([`crate::winsub`]), a file
//! standing in for guest RAM in this crate's tests. The provider enforces
//! the three rules that are its own:
//! 1. a region is usable (`Event::Mapped`) only after the peer's `MAPPED`;
//! 3. it is freed only after `UNMAPPED` for its epoch, never on a timeout;
//! 5. every answer is matched against the epoch it was given for.

use std::collections::BTreeMap;
use std::time::Instant;
use sv_transport::region::SharedRegion;
use sv_transport::regiontab::{Kind, PState, QState, Reason, Table};

/// One allocation the substrate made.
pub struct Alloc {
    /// The substrate's own id for it (the keeper's region id).
    pub id: u32,
    pub handle: [u8; sv_transport::regiontab::HANDLE_LEN],
    /// The provider's own view of it.
    pub view: SharedRegion,
    /// Time the substrate spent allocating and stamping it.
    pub alloc_us: u64,
}

pub trait Substrate {
    fn kind(&self) -> Kind;
    fn alloc(&mut self, len: u64) -> Result<Alloc, String>;
    /// Free `id`. Called only after the peer's `UNMAPPED` (or `REFUSED`).
    fn free(&mut self, id: u32) -> Result<(), String>;
    /// The substrate's orphans from a dead provider of this pool: `(id, len)`.
    /// The provider matches them to table entries and frees them on
    /// `UNMAPPED`.
    fn orphans(&mut self) -> Vec<(u32, u64)> {
        Vec::new()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    Announced,
    Mapped,
    Retiring,
}

pub struct Region {
    pub entry: u32,
    pub k: u32,
    pub id: u32,
    pub len: u64,
    pub epoch: u64,
    pub phase: Phase,
    /// `None` for an adopted orphan: this run never mapped it.
    pub view: Option<SharedRegion>,
    /// Adopted from the table but not this provider's to free: the keeper
    /// gave it no such orphan (another Windows user's pool, §108.10). The
    /// host lets go of it and the entry is cleared; its owner frees it.
    pub foreign: bool,
    pub announced: Instant,
    pub alloc_us: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// Region `k` is mapped on the far side; `rtt_us` from announce to answer.
    Mapped { k: u32, entry: u32, rtt_us: u64 },
    /// The peer refused it; already freed.
    Refused { k: u32, reason: Reason },
    /// Retired and freed; `wait_us` from retire to `UNMAPPED`.
    Freed { k: u32, wait_us: u64 },
}

pub struct Provider<S: Substrate> {
    boot: SharedRegion,
    table_off: u64,
    pub substrate: S,
    pub regions: BTreeMap<u32, Region>,
    retired_at: BTreeMap<u32, Instant>,
    pub run: u64,
}

impl<S: Substrate> Provider<S> {
    /// Take over the table at `table_off` of `boot`. A table a previous run
    /// left is adopted (§108.5): its live entries become retiring orphans,
    /// freed in the substrate when the peer answers `UNMAPPED`.
    pub fn start(boot: SharedRegion, table_off: u64, mut substrate: S, run: u64) -> Result<Provider<S>, String> {
        let orphan_ids = substrate.orphans();
        let mine: Vec<u32> = orphan_ids.iter().map(|o| o.0).collect();
        let mut regions = BTreeMap::new();
        {
            let t = Table::new(&boot, table_off).map_err(|e| e.to_string())?;
            if t.header().valid {
                t.publish(run);
                for e in t.adopt() {
                    let id = if e.kind == Kind::WinLarge as u32 {
                        sv_transport::regiontab::parse_win_large(&e.handle).2
                    } else {
                        0
                    };
                    regions.insert(
                        e.index,
                        Region { entry: e.index, k: e.region, id, len: e.len, epoch: e.epoch, phase: Phase::Retiring, view: None, foreign: !mine.contains(&id), announced: Instant::now(), alloc_us: 0 },
                    );
                }
            } else {
                t.wipe();
                t.publish(run);
            }
            // An orphan the keeper holds but the table does not know (the
            // provider died between alloc and announce) was never announced,
            // so the host never mapped it: free it now.
            for (id, _) in orphan_ids {
                if !regions.values().any(|r| r.id == id) {
                    substrate.free(id)?;
                }
            }
        }
        let retired_at = regions.keys().map(|&e| (e, Instant::now())).collect();
        Ok(Provider { boot, table_off, substrate, regions, retired_at, run })
    }

    pub fn table(&self) -> Table<'_> {
        Table::new(&self.boot, self.table_off).expect("checked at start")
    }

    pub fn boot(&self) -> &SharedRegion {
        &self.boot
    }

    pub fn beat(&self) {
        self.table().provider_beat();
    }

    /// Allocate `len` bytes and announce them as region `k`. The region is
    /// not usable until [`Provider::poll`] reports `Mapped`.
    pub fn announce(&mut self, k: u32, len: u64) -> Result<u32, String> {
        let entry = self.table().free_entry().ok_or("the region table is full")?;
        let a = self.substrate.alloc(len)?;
        let kind = self.substrate.kind();
        let epoch = self.table().announce(entry, kind, k, len, &a.handle);
        self.regions.insert(
            entry,
            Region { entry, k, id: a.id, len, epoch, phase: Phase::Announced, view: Some(a.view), foreign: false, announced: Instant::now(), alloc_us: a.alloc_us },
        );
        Ok(entry)
    }

    /// Ask the peer to let go of the region in `entry`. The caller has made
    /// sure nothing is leased in it (§108.4 rule 2).
    pub fn retire(&mut self, entry: u32) {
        if let Some(r) = self.regions.get_mut(&entry) {
            r.phase = Phase::Retiring;
            self.retired_at.insert(entry, Instant::now());
            Table::new(&self.boot, self.table_off).expect("checked").retire(entry);
        }
    }

    /// Read every answer; free what may be freed. Never blocks.
    pub fn poll(&mut self) -> Result<Vec<Event>, String> {
        let mut out = Vec::new();
        let mut gone = Vec::new();
        for (&entry, r) in self.regions.iter_mut() {
            let t = Table::new(&self.boot, self.table_off).expect("checked");
            let e = t.entry(entry);
            if e.epoch != r.epoch {
                return Err(format!("entry {entry}: the table holds epoch {} where this provider wrote {}", e.epoch, r.epoch));
            }
            match (r.phase, e.answer()) {
                (Phase::Announced, QState::Mapped) => {
                    r.phase = Phase::Mapped;
                    out.push(Event::Mapped { k: r.k, entry, rtt_us: r.announced.elapsed().as_micros() as u64 });
                }
                (Phase::Announced, QState::Refused) => {
                    // The peer mapped nothing (rule 6): free at once.
                    r.view = None;
                    self.substrate.free(r.id)?;
                    t.clear(entry);
                    gone.push(entry);
                    out.push(Event::Refused { k: r.k, reason: e.reason });
                }
                (Phase::Retiring, QState::Unmapped) if e.pstate == PState::Retiring => {
                    // Rule 3: the only path to `free`.
                    r.view = None;
                    if !r.foreign {
                        self.substrate.free(r.id)?;
                    }
                    t.clear(entry);
                    gone.push(entry);
                    let wait = self.retired_at.get(&entry).map_or(0, |t| t.elapsed().as_micros() as u64);
                    out.push(Event::Freed { k: r.k, wait_us: wait });
                }
                _ => {}
            }
        }
        for e in gone {
            self.regions.remove(&e);
            self.retired_at.remove(&e);
        }
        Ok(out)
    }

    pub fn view(&self, entry: u32) -> Option<&SharedRegion> {
        self.regions.get(&entry).and_then(|r| r.view.as_ref())
    }
}
