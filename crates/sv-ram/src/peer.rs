//! The peer's half of §108.4 on the Linux host: locate, map, verify, answer;
//! and on retire, let go, then answer.
//!
//! The peer never frees anything and never needs to notice that a provider
//! died. While the keeper holds a region, a host write into it lands in memory
//! Windows has given to no one else. So the peer acts only on what the table
//! says: `ANNOUNCED` (map it), `RETIRING` (unmap it, then answer). A dead
//! provider's regions become `RETIRING` when the next provider of that pool
//! adopts them (§108.5).

use std::collections::BTreeMap;
use std::os::fd::AsFd;
use std::os::unix::fs::FileExt;
use std::time::Instant;
use sv_transport::lpstamp::{self, ProbeError, Wanted};
use sv_transport::region::{SharedRegion, StitchRun};
use sv_transport::regiontab::{Entry, Kind, PState, QState, Reason, Table, parse_win_large};

pub struct Mapped {
    pub entry: u32,
    pub k: u32,
    pub epoch: u64,
    pub len: u64,
    pub view: SharedRegion,
    /// `(file_offset, len)` runs of the backend, in region order.
    pub runs: Vec<(u64, u64)>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PeerEvent {
    Mapped { entry: u32, k: u32, len: u64, runs: usize, locate_us: u64, map_us: u64 },
    Refused { entry: u32, k: u32, reason: Reason, why: String },
    Unmapped { entry: u32, k: u32 },
    ProviderRun { run: u64 },
}

pub struct Peer {
    boot: SharedRegion,
    table_off: u64,
    ram: std::fs::File,
    ram_len: u64,
    pub maps: BTreeMap<u32, Mapped>,
    provider_run: u64,
    pub run: u64,
    t0: Instant,
}

impl Peer {
    /// `boot`: the bootstrap device's mapping; `ram`: the guest RAM backend
    /// (QEMU's memfd through `/proc/<pid>/fd`, or the backend file).
    pub fn attach(boot: SharedRegion, table_off: u64, ram: std::fs::File, run: u64) -> Result<Peer, String> {
        let ram_len = ram.metadata().map_err(|e| e.to_string())?.len();
        let p = Peer { boot, table_off, ram, ram_len, maps: BTreeMap::new(), provider_run: 0, run, t0: Instant::now() };
        p.table().peer_attach(run);
        Ok(p)
    }

    pub fn table(&self) -> Table<'_> {
        Table::new(&self.boot, self.table_off).expect("bounds checked at attach")
    }

    fn now_us(&self) -> u64 {
        self.t0.elapsed().as_micros() as u64
    }

    pub fn beat(&self) {
        self.table().peer_beat();
    }

    /// One pass over the table. Returns what changed; `mapper` stops any
    /// writer of a region before it is unmapped (it is called with the
    /// entry about to go).
    pub fn scan(&mut self, mut before_unmap: impl FnMut(&Mapped)) -> Vec<PeerEvent> {
        let mut ev = Vec::new();
        let h = self.table().header();
        if !h.valid {
            return ev;
        }
        if h.provider_run != self.provider_run {
            self.provider_run = h.provider_run;
            ev.push(PeerEvent::ProviderRun { run: h.provider_run });
        }
        let entries: Vec<Entry> = (1..=sv_transport::regiontab::ENTRIES).map(|i| self.table().entry(i)).collect();
        let mut to_locate = Vec::new();
        for e in &entries {
            let mine = self.maps.get(&e.index).map(|m| m.epoch);
            match e.pstate {
                PState::Announced => {
                    if mine == Some(e.epoch) || e.answer() != QState::None {
                        continue;
                    }
                    if let Some(old) = mine {
                        // A new epoch in an entry we still map: the provider
                        // cleared it without our UNMAPPED. Never fatal for the
                        // guest (we held on), but it is a provider bug.
                        eprintln!("[peer] entry {}: epoch {} replaced {old} without UNMAPPED; unmapping the old", e.index, e.epoch);
                        if let Some(m) = self.maps.remove(&e.index) {
                            before_unmap(&m);
                        }
                    }
                    to_locate.push(*e);
                }
                PState::Retiring => {
                    if e.answer() == QState::Unmapped {
                        continue;
                    }
                    if let Some(m) = self.maps.get(&e.index) {
                        if m.epoch != e.epoch {
                            eprintln!("[peer] entry {}: retiring epoch {} but we map {}", e.index, e.epoch, m.epoch);
                        }
                    }
                    if let Some(m) = self.maps.remove(&e.index) {
                        before_unmap(&m);
                        drop(m); // munmap
                    }
                    let now = self.now_us();
                    self.table().answer(e.index, e.epoch, QState::Unmapped, Reason::None, now);
                    ev.push(PeerEvent::Unmapped { entry: e.index, k: e.region });
                }
                PState::Empty => {
                    if let Some(m) = self.maps.remove(&e.index) {
                        eprintln!("[peer] entry {}: cleared while mapped (epoch {}); unmapping", e.index, m.epoch);
                        before_unmap(&m);
                    }
                }
            }
        }
        if !to_locate.is_empty() {
            ev.extend(self.locate(&to_locate));
        }
        ev
    }

    fn refuse(&self, e: &Entry, reason: Reason, why: String) -> PeerEvent {
        let now = self.now_us();
        self.table().answer(e.index, e.epoch, QState::Refused, reason, now);
        PeerEvent::Refused { entry: e.index, k: e.region, reason, why }
    }

    fn locate(&mut self, es: &[Entry]) -> Vec<PeerEvent> {
        let mut ev = Vec::new();
        let large: Vec<&Entry> = es.iter().filter(|e| e.kind == Kind::WinLarge as u32).collect();
        for e in es.iter().filter(|e| e.kind != Kind::WinLarge as u32) {
            ev.push(self.refuse(e, Reason::Kind, format!("kind {} is not served by this peer", e.kind)));
        }
        if large.is_empty() {
            return ev;
        }
        let wanted: Vec<Wanted> = large
            .iter()
            .map(|e| {
                let (nonce, units, _) = parse_win_large(&e.handle);
                Wanted { nonce, units }
            })
            .collect();
        let t = Instant::now();
        let ram = &self.ram;
        let (found, _stats) = lpstamp::probe(self.ram_len, &wanted, |off, buf| ram.read_exact_at(buf, off).is_ok());
        let locate_us = t.elapsed().as_micros() as u64;
        for ((e, w), f) in large.into_iter().zip(&wanted).zip(found) {
            if e.len != w.units as u64 * lpstamp::UNIT {
                ev.push(self.refuse(e, Reason::NotFound, format!("len {} is not {} units", e.len, w.units)));
                continue;
            }
            let offsets = match f {
                Ok(o) => o,
                Err(ProbeError::Duplicate { index, a, b }) => {
                    ev.push(self.refuse(e, Reason::Duplicate, format!("unit {index} at {a:#x} and {b:#x}")));
                    continue;
                }
                Err(err) => {
                    ev.push(self.refuse(e, Reason::NotFound, format!("{err:?}")));
                    continue;
                }
            };
            let t = Instant::now();
            let runs = lpstamp::runs(&offsets);
            let mut at = 0;
            let stitch: Vec<StitchRun> = runs
                .iter()
                .map(|&(file_offset, len)| {
                    let r = StitchRun { file_offset, extent_offset: at, len };
                    at += len;
                    r
                })
                .collect();
            let view = match SharedRegion::stitch(self.ram.as_fd(), &stitch) {
                Ok(v) => v,
                Err(err) => {
                    ev.push(self.refuse(e, Reason::Map, format!("{err}")));
                    continue;
                }
            };
            // Verify through the mapping what the probe read through the file:
            // the two are the same pages or the mapping is wrong.
            let mut bad = None;
            for i in 0..w.units {
                let s = lpstamp::stamp(&w.nonce, i, w.units);
                if !view.equals(i as u64 * lpstamp::UNIT, &s).unwrap_or(false) {
                    bad = Some(i);
                    break;
                }
            }
            if let Some(i) = bad {
                drop(view);
                ev.push(self.refuse(e, Reason::Map, format!("unit {i}: the mapping does not show the stamp the probe read")));
                continue;
            }
            let map_us = t.elapsed().as_micros() as u64;
            let now = self.now_us();
            self.table().answer(e.index, e.epoch, QState::Mapped, Reason::None, now);
            ev.push(PeerEvent::Mapped { entry: e.index, k: e.region, len: e.len, runs: runs.len(), locate_us, map_us });
            self.maps.insert(e.index, Mapped { entry: e.index, k: e.region, epoch: e.epoch, len: e.len, view, runs });
        }
        ev
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::{Alloc, Event, Provider, Substrate};
    use sv_transport::regiontab::{TABLE_LEN, win_large_handle};

    /// Guest RAM simulated by a file: a region is units at scattered 2 MiB
    /// offsets, stamped, as Windows' large pages are.
    struct SimRam {
        file: std::fs::File,
        units: u64,
        used: Vec<bool>,
        regions: BTreeMap<u32, Vec<u64>>,
        next: u32,
        seed: u64,
        freed: Vec<u32>,
    }

    impl SimRam {
        fn new(units: u64) -> SimRam {
            let file = tempfile(units * lpstamp::UNIT);
            SimRam { file, units, used: vec![false; units as usize], regions: BTreeMap::new(), next: 1, seed: 0x1234_5678, freed: Vec::new() }
        }
        fn rnd(&mut self) -> u64 {
            self.seed ^= self.seed << 13;
            self.seed ^= self.seed >> 7;
            self.seed ^= self.seed << 17;
            self.seed
        }
    }

    fn tempfile(len: u64) -> std::fs::File {
        let p = std::env::temp_dir().join(format!("sv-ram-test-{}-{}", std::process::id(), rand_u64()));
        let f = std::fs::OpenOptions::new().read(true).write(true).create(true).truncate(true).open(&p).unwrap();
        std::fs::remove_file(&p).unwrap();
        f.set_len(len).unwrap();
        f
    }
    fn rand_u64() -> u64 {
        use std::hash::{BuildHasher, Hasher};
        let mut h = std::collections::hash_map::RandomState::new().build_hasher();
        h.write_u64(7);
        h.finish()
    }

    impl Substrate for SimRam {
        fn kind(&self) -> Kind {
            Kind::WinLarge
        }
        fn alloc(&mut self, len: u64) -> Result<Alloc, String> {
            let n = (len / lpstamp::UNIT) as usize;
            let mut pick = Vec::new();
            let mut tries = 0;
            while pick.len() < n {
                tries += 1;
                if tries > 100_000 {
                    return Err("full".into());
                }
                let u = self.rnd() % self.units;
                if !self.used[u as usize] {
                    self.used[u as usize] = true;
                    pick.push(u);
                }
            }
            let id = self.next;
            self.next += 1;
            let nonce = (self.rnd() as u128 | ((self.rnd() as u128) << 64)).to_le_bytes();
            for (i, &u) in pick.iter().enumerate() {
                self.file.write_all_at(&lpstamp::stamp(&nonce, i as u32, n as u32), u * lpstamp::UNIT).unwrap();
            }
            self.regions.insert(id, pick);
            Ok(Alloc { id, handle: win_large_handle(nonce, n as u32, id), view: SharedRegion::heap(4096).unwrap(), alloc_us: 0 })
        }
        fn free(&mut self, id: u32) -> Result<(), String> {
            for u in self.regions.remove(&id).ok_or("no such id")? {
                self.used[u as usize] = false;
                // Windows would reuse the frame: scribble over it.
                self.file.write_all_at(&[0xAA; 64], u * lpstamp::UNIT).unwrap();
            }
            self.freed.push(id);
            Ok(())
        }
    }

    fn boot() -> (std::fs::File, SharedRegion) {
        let f = tempfile(TABLE_LEN + 4096);
        let r = SharedRegion::map_fd(f.as_fd(), 0, (TABLE_LEN + 4096) as usize).unwrap();
        (f, r)
    }

    #[test]
    fn grow_map_write_retire_free_through_both_halves() {
        let (bf, b1) = boot();
        let sim = SimRam::new(256);
        let ram = sim.file.try_clone().unwrap();
        let mut p = Provider::start(b1, 4096, sim, 1).unwrap();
        let b2 = SharedRegion::map_fd(bf.as_fd(), 0, (TABLE_LEN + 4096) as usize).unwrap();
        let mut peer = Peer::attach(b2, 4096, ram, 9).unwrap();

        let e1 = p.announce(1, 64 << 20).unwrap();
        let e2 = p.announce(2, 6 << 20).unwrap();
        assert!(p.poll().unwrap().is_empty(), "nothing is usable before MAPPED");
        let ev = peer.scan(|_| {});
        assert_eq!(ev.iter().filter(|e| matches!(e, PeerEvent::Mapped { .. })).count(), 2, "{ev:?}");
        // The host writes through its view; the bytes land in the units.
        let m = &peer.maps[&e1];
        m.view.write_bytes(lpstamp::UNIT + 100, b"host").unwrap();
        let (off, _) = m.runs.iter().fold((None, 0u64), |(f, at), &(o, l)| {
            let f = f.or(if lpstamp::UNIT >= at && lpstamp::UNIT < at + l { Some(o + lpstamp::UNIT - at) } else { None });
            (f, at + l)
        });
        let mut got = [0u8; 4];
        peer.ram.read_exact_at(&mut got, off.unwrap() + 100).unwrap();
        assert_eq!(&got, b"host");

        let ev = p.poll().unwrap();
        assert_eq!(ev.iter().filter(|e| matches!(e, Event::Mapped { .. })).count(), 2);
        p.retire(e2);
        // Rule 3: not freed until the peer answers.
        assert!(p.poll().unwrap().is_empty());
        assert!(p.substrate.freed.is_empty());
        let mut stopped = Vec::new();
        let ev = peer.scan(|m| stopped.push(m.entry));
        assert_eq!(stopped, vec![e2]);
        assert!(ev.contains(&PeerEvent::Unmapped { entry: e2, k: 2 }));
        let ev = p.poll().unwrap();
        assert!(matches!(ev[0], Event::Freed { k: 2, .. }));
        assert_eq!(p.substrate.freed, vec![2]);
        assert!(peer.maps.contains_key(&e1) && !peer.maps.contains_key(&e2));
    }

    #[test]
    fn a_dead_providers_regions_are_freed_only_after_the_peer_lets_go() {
        let (bf, b1) = boot();
        let sim = SimRam::new(128);
        let ram = sim.file.try_clone().unwrap();
        let mut p = Provider::start(b1, 4096, sim, 1).unwrap();
        let b2 = SharedRegion::map_fd(bf.as_fd(), 0, (TABLE_LEN + 4096) as usize).unwrap();
        let mut peer = Peer::attach(b2, 4096, ram, 9).unwrap();
        let e = p.announce(1, 8 << 20).unwrap();
        peer.scan(|_| {});
        p.poll().unwrap();
        // The provider dies; the keeper (here: the SimRam) still holds the
        // region. A second provider run adopts it.
        let SimRam { file, units, used, regions, next, seed, freed } = p.substrate;
        let sim2 = SimRam { file, units, used, regions, next, seed, freed };
        let b3 = SharedRegion::map_fd(bf.as_fd(), 0, (TABLE_LEN + 4096) as usize).unwrap();
        let mut p2 = Provider::start(b3, 4096, sim2, 2).unwrap();
        assert_eq!(p2.regions.len(), 1);
        assert!(p2.poll().unwrap().is_empty(), "not freed before the peer lets go");
        let ev = peer.scan(|_| {});
        assert!(ev.contains(&PeerEvent::Unmapped { entry: e, k: 1 }), "{ev:?}");
        let ev = p2.poll().unwrap();
        assert!(matches!(ev[0], Event::Freed { k: 1, .. }), "{ev:?}");
        assert_eq!(p2.substrate.freed, vec![1]);
    }

    #[test]
    fn a_forged_duplicate_unit_is_refused_and_freed_at_once() {
        let (bf, b1) = boot();
        let sim = SimRam::new(64);
        let ram = sim.file.try_clone().unwrap();
        let mut p = Provider::start(b1, 4096, sim, 1).unwrap();
        let b2 = SharedRegion::map_fd(bf.as_fd(), 0, (TABLE_LEN + 4096) as usize).unwrap();
        let mut peer = Peer::attach(b2, 4096, ram, 9).unwrap();
        let e = p.announce(1, 4 << 20).unwrap();
        // copy unit 0's stamp somewhere else: a stale or forged one
        let units = p.substrate.regions[&1].clone();
        let mut s = [0u8; 64];
        p.substrate.file.read_exact_at(&mut s, units[0] * lpstamp::UNIT).unwrap();
        let spare = (0..64u64).find(|u| !p.substrate.used[*u as usize]).unwrap();
        p.substrate.file.write_all_at(&s, spare * lpstamp::UNIT).unwrap();
        let ev = peer.scan(|_| {});
        assert!(matches!(ev.last(), Some(PeerEvent::Refused { reason: Reason::Duplicate, .. })), "{ev:?}");
        let ev = p.poll().unwrap();
        assert_eq!(ev, vec![Event::Refused { k: 1, reason: Reason::Duplicate }]);
        assert!(!peer.maps.contains_key(&e));
    }
}
