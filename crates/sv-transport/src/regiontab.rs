//! The region table: how a pool grows and shrinks across the VM boundary
//! (waycompose-design.md §108.2–108.5).
//!
//! A 32 KiB block in a pool's bootstrap memory. The **provider** (the pool's
//! owner, the side whose memory a region is) announces and retires regions; the
//! **peer** (the side that maps them from across the boundary) answers. Each
//! half of each entry has exactly one writer, and the word that makes a half
//! valid (`pstate`, `qstate`) is written last with release ordering, so a
//! reader that sees the state word also sees the fields before it.
//!
//! The keeper rule (§108.4 rule 3) is not in this module's power to enforce,
//! but it is in its vocabulary: [`Table::retired`] is the only way for a
//! provider to learn that a region may be freed, and it is true only when the
//! peer has written `UNMAPPED` for that entry's epoch.

use crate::region::SharedRegion;
use std::sync::atomic::{Ordering, fence};

pub const MAGIC: [u8; 8] = *b"SVRGN001";
pub const VERSION: u32 = 1;
/// The table's size in bootstrap memory.
pub const TABLE_LEN: u64 = 32 << 10;
pub const ENTRY_LEN: u64 = 128;
/// Entries 1..=ENTRIES; entry 0's slot is the header.
pub const ENTRIES: u32 = (TABLE_LEN / ENTRY_LEN) as u32 - 1;
/// The bell token: "a region table changed" (§108.2).
pub const REGION_TOKEN: u8 = 0xFB;
/// A side's beat must advance at least this often.
pub const BEAT_MS: u64 = 250;
/// No beat for this long and the other side is taken for dead (§108.5).
pub const DEAD_MS: u64 = 2000;
pub const HANDLE_LEN: usize = 64;

// header
const H_MAGIC: u64 = 0;
const H_VERSION: u64 = 8;
const H_ENTRIES: u64 = 12;
const H_PROVIDER_RUN: u64 = 16;
const H_PROVIDER_BEAT: u64 = 24;
const H_PEER_RUN: u64 = 32;
const H_PEER_BEAT: u64 = 40;
const H_WANT_BYTES: u64 = 48;
const H_WANT_SEQ: u64 = 56;
const H_BUDGET: u64 = 64;
const H_EPOCH: u64 = 72;

// entry, provider half
const E_PSTATE: u64 = 0;
const E_KIND: u64 = 4;
const E_EPOCH: u64 = 8;
const E_REGION: u64 = 16;
const E_FLAGS: u64 = 20;
const E_LEN: u64 = 24;
const E_HANDLE: u64 = 32;
// entry, peer half
const E_QSTATE: u64 = 96;
const E_REASON: u64 = 100;
const E_QEPOCH: u64 = 104;
const E_QTIME: u64 = 112;

/// Bit 0 of an entry's flags: adopted from a dead provider run (§108.5).
pub const FLAG_ORPHAN: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum PState {
    Empty = 0,
    Announced = 1,
    Retiring = 2,
}

impl PState {
    pub fn from_u32(v: u32) -> PState {
        match v {
            1 => PState::Announced,
            2 => PState::Retiring,
            _ => PState::Empty,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum QState {
    None = 0,
    Mapped = 1,
    Refused = 2,
    Unmapped = 3,
}

impl QState {
    pub fn from_u32(v: u32) -> QState {
        match v {
            1 => QState::Mapped,
            2 => QState::Refused,
            3 => QState::Unmapped,
            _ => QState::None,
        }
    }
}

/// What backs a region (§108.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum Kind {
    Bar = 1,
    WinLarge = 2,
    WinAwe = 3,
    MacShm = 4,
    File = 5,
}

impl Kind {
    pub fn from_u32(v: u32) -> Option<Kind> {
        Some(match v {
            1 => Kind::Bar,
            2 => Kind::WinLarge,
            3 => Kind::WinAwe,
            4 => Kind::MacShm,
            5 => Kind::File,
            _ => return None,
        })
    }

    /// The substrate's allocation unit; a region's length is a multiple.
    pub fn unit(self) -> u64 {
        match self {
            Kind::Bar => 1 << 20,
            Kind::WinLarge => 2 << 20,
            Kind::WinAwe | Kind::File => 4096,
            Kind::MacShm => 64 << 20,
        }
    }
}

/// Why a peer refused (§108.4 rule 6).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum Reason {
    None = 0,
    Kind = 1,
    NotFound = 2,
    Duplicate = 3,
    Limit = 4,
    Map = 5,
    Budget = 6,
}

impl Reason {
    pub fn from_u32(v: u32) -> Reason {
        match v {
            1 => Reason::Kind,
            2 => Reason::NotFound,
            3 => Reason::Duplicate,
            4 => Reason::Limit,
            5 => Reason::Map,
            6 => Reason::Budget,
            _ => Reason::None,
        }
    }
}

/// One entry, as read at one moment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Entry {
    pub index: u32,
    pub pstate: PState,
    pub kind: u32,
    pub epoch: u64,
    pub region: u32,
    pub flags: u32,
    pub len: u64,
    pub handle: [u8; HANDLE_LEN],
    pub qstate: QState,
    pub reason: Reason,
    pub qepoch: u64,
    pub qtime_us: u64,
}

impl Entry {
    /// The peer's answer, if it is for this entry's current epoch.
    pub fn answer(&self) -> QState {
        if self.qepoch == self.epoch && self.epoch != 0 { self.qstate } else { QState::None }
    }
}

/// The header, as read at one moment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Header {
    pub valid: bool,
    pub provider_run: u64,
    pub provider_beat: u64,
    pub peer_run: u64,
    pub peer_beat: u64,
    pub want_bytes: u64,
    pub want_seq: u64,
    pub budget: u64,
    pub epoch: u64,
}

/// The table at `base` in a mapped region. Both sides use this type; which
/// methods a side calls decides which half it writes.
pub struct Table<'a> {
    region: &'a SharedRegion,
    base: u64,
}

impl<'a> Table<'a> {
    pub fn new(region: &'a SharedRegion, base: u64) -> crate::Result<Table<'a>> {
        region.check(base, TABLE_LEN)?;
        if !base.is_multiple_of(8) {
            return Err(crate::Error::Misaligned { off: base, align: 8 });
        }
        Ok(Table { region, base })
    }

    fn at(i: u32, field: u64) -> u64 {
        i as u64 * ENTRY_LEN + field
    }
    fn u32(&self, off: u64) -> &std::sync::atomic::AtomicU32 {
        self.region.atomic_u32(self.base + off).expect("inside the checked table")
    }
    fn u64(&self, off: u64) -> &std::sync::atomic::AtomicU64 {
        self.region.atomic_u64(self.base + off).expect("inside the checked table")
    }

    // ------------------------------------------------------------ provider

    /// A provider's start. Entries a previous run left are kept, not cleared:
    /// they may be regions whose memory still lives (§108.5), and
    /// [`Table::adopt`] decides what happens to them.
    pub fn publish(&self, run: u64) {
        self.region.write_bytes(self.base + H_MAGIC, &[0; 8]).expect("in table");
        self.u32(H_VERSION).store(VERSION, Ordering::Relaxed);
        self.u32(H_ENTRIES).store(ENTRIES, Ordering::Relaxed);
        self.u64(H_PROVIDER_RUN).store(run, Ordering::Relaxed);
        self.u64(H_PROVIDER_BEAT).store(0, Ordering::Relaxed);
        fence(Ordering::Release);
        self.region.write_bytes(self.base + H_MAGIC, &MAGIC).expect("in table");
    }

    /// Zero the whole table (a fresh bootstrap only: nothing can be live).
    pub fn wipe(&self) {
        self.region.fill(self.base, TABLE_LEN, 0).expect("in table");
    }

    pub fn provider_beat(&self) {
        self.u64(H_PROVIDER_BEAT).fetch_add(1, Ordering::Release);
    }

    pub fn set_budget(&self, bytes: u64) {
        self.u64(H_BUDGET).store(bytes, Ordering::Relaxed);
    }

    /// The next epoch: never 0, never reused within a table's life.
    fn next_epoch(&self) -> u64 {
        self.u64(H_EPOCH).fetch_add(1, Ordering::Relaxed) + 1
    }

    /// The first `Empty` entry, if any.
    pub fn free_entry(&self) -> Option<u32> {
        (1..=ENTRIES).find(|&i| PState::from_u32(self.u32(Self::at(i, E_PSTATE)).load(Ordering::Acquire)) == PState::Empty)
    }

    /// Announce region `region` in entry `i`; returns its epoch.
    pub fn announce(&self, i: u32, kind: Kind, region: u32, len: u64, handle: &[u8]) -> u64 {
        assert!((1..=ENTRIES).contains(&i) && handle.len() <= HANDLE_LEN);
        let epoch = self.next_epoch();
        let mut h = [0u8; HANDLE_LEN];
        h[..handle.len()].copy_from_slice(handle);
        self.u32(Self::at(i, E_KIND)).store(kind as u32, Ordering::Relaxed);
        self.u64(Self::at(i, E_EPOCH)).store(epoch, Ordering::Relaxed);
        self.u32(Self::at(i, E_REGION)).store(region, Ordering::Relaxed);
        self.u32(Self::at(i, E_FLAGS)).store(0, Ordering::Relaxed);
        self.u64(Self::at(i, E_LEN)).store(len, Ordering::Relaxed);
        self.region.write_bytes(self.base + Self::at(i, E_HANDLE), &h).expect("in table");
        fence(Ordering::Release);
        self.u32(Self::at(i, E_PSTATE)).store(PState::Announced as u32, Ordering::Release);
        epoch
    }

    /// Ask the peer to stop touching entry `i`'s region.
    pub fn retire(&self, i: u32) {
        self.u32(Self::at(i, E_PSTATE)).store(PState::Retiring as u32, Ordering::Release);
    }

    /// True once the peer has written `UNMAPPED` for the epoch of entry `i`:
    /// the one condition under which its memory may be freed.
    pub fn retired(&self, i: u32) -> bool {
        let e = self.entry(i);
        e.pstate == PState::Retiring && e.answer() == QState::Unmapped
    }

    /// The memory is freed (or was never mapped, after REFUSED): the entry is
    /// reusable.
    pub fn clear(&self, i: u32) {
        self.u32(Self::at(i, E_PSTATE)).store(PState::Empty as u32, Ordering::Release);
    }

    /// A new provider run takes over the entries a dead run left (§108.5):
    /// everything not `Empty` becomes a `Retiring` orphan under the same
    /// epoch, so the peer's `UNMAPPED` for it still counts. Returns them.
    pub fn adopt(&self) -> Vec<Entry> {
        let mut out = Vec::new();
        for i in 1..=ENTRIES {
            let e = self.entry(i);
            if e.pstate == PState::Empty {
                continue;
            }
            self.u32(Self::at(i, E_FLAGS)).fetch_or(FLAG_ORPHAN, Ordering::Relaxed);
            fence(Ordering::Release);
            self.retire(i);
            out.push(self.entry(i));
        }
        // Epochs continue past every adopted one.
        let max = out.iter().map(|e| e.epoch).max().unwrap_or(0);
        self.u64(H_EPOCH).fetch_max(max, Ordering::Relaxed);
        out
    }

    // ------------------------------------------------------------ peer

    pub fn peer_attach(&self, run: u64) {
        self.u64(H_PEER_RUN).store(run, Ordering::Release);
    }

    pub fn peer_beat(&self) {
        self.u64(H_PEER_BEAT).fetch_add(1, Ordering::Release);
    }

    /// Write the peer's answer for entry `i`, epoch `epoch`.
    pub fn answer(&self, i: u32, epoch: u64, q: QState, reason: Reason, now_us: u64) {
        self.u32(Self::at(i, E_REASON)).store(reason as u32, Ordering::Relaxed);
        self.u64(Self::at(i, E_QEPOCH)).store(epoch, Ordering::Relaxed);
        self.u64(Self::at(i, E_QTIME)).store(now_us, Ordering::Relaxed);
        fence(Ordering::Release);
        self.u32(Self::at(i, E_QSTATE)).store(q as u32, Ordering::Release);
    }

    /// The peer is short of `bytes` that the provider cannot see (§108.6).
    pub fn want(&self, bytes: u64) {
        self.u64(H_WANT_BYTES).store(bytes, Ordering::Relaxed);
        fence(Ordering::Release);
        self.u64(H_WANT_SEQ).fetch_add(1, Ordering::Release);
    }

    // ------------------------------------------------------------ both

    pub fn header(&self) -> Header {
        let mut m = [0u8; 8];
        let _ = self.region.read_bytes(self.base + H_MAGIC, &mut m);
        fence(Ordering::Acquire);
        Header {
            valid: m == MAGIC && self.u32(H_VERSION).load(Ordering::Relaxed) == VERSION,
            provider_run: self.u64(H_PROVIDER_RUN).load(Ordering::Acquire),
            provider_beat: self.u64(H_PROVIDER_BEAT).load(Ordering::Acquire),
            peer_run: self.u64(H_PEER_RUN).load(Ordering::Acquire),
            peer_beat: self.u64(H_PEER_BEAT).load(Ordering::Acquire),
            want_bytes: self.u64(H_WANT_BYTES).load(Ordering::Relaxed),
            want_seq: self.u64(H_WANT_SEQ).load(Ordering::Acquire),
            budget: self.u64(H_BUDGET).load(Ordering::Relaxed),
            epoch: self.u64(H_EPOCH).load(Ordering::Relaxed),
        }
    }

    pub fn entry(&self, i: u32) -> Entry {
        let pstate = PState::from_u32(self.u32(Self::at(i, E_PSTATE)).load(Ordering::Acquire));
        let qstate = QState::from_u32(self.u32(Self::at(i, E_QSTATE)).load(Ordering::Acquire));
        let mut handle = [0u8; HANDLE_LEN];
        let _ = self.region.read_bytes(self.base + Self::at(i, E_HANDLE), &mut handle);
        Entry {
            index: i,
            pstate,
            kind: self.u32(Self::at(i, E_KIND)).load(Ordering::Relaxed),
            epoch: self.u64(Self::at(i, E_EPOCH)).load(Ordering::Relaxed),
            region: self.u32(Self::at(i, E_REGION)).load(Ordering::Relaxed),
            flags: self.u32(Self::at(i, E_FLAGS)).load(Ordering::Relaxed),
            len: self.u64(Self::at(i, E_LEN)).load(Ordering::Relaxed),
            handle,
            qstate,
            reason: Reason::from_u32(self.u32(Self::at(i, E_REASON)).load(Ordering::Relaxed)),
            qepoch: self.u64(Self::at(i, E_QEPOCH)).load(Ordering::Relaxed),
            qtime_us: self.u64(Self::at(i, E_QTIME)).load(Ordering::Relaxed),
        }
    }

    /// Every entry that is not `Empty`.
    pub fn live(&self) -> Vec<Entry> {
        (1..=ENTRIES).map(|i| self.entry(i)).filter(|e| e.pstate != PState::Empty).collect()
    }
}

/// The `WIN_LARGE` handle: a 16-byte nonce, the unit count and the keeper's id.
pub fn win_large_handle(nonce: [u8; 16], units: u32, keeper_id: u32) -> [u8; HANDLE_LEN] {
    let mut h = [0u8; HANDLE_LEN];
    h[..16].copy_from_slice(&nonce);
    h[16..20].copy_from_slice(&units.to_le_bytes());
    h[20..24].copy_from_slice(&keeper_id.to_le_bytes());
    h
}

/// The fields of a `WIN_LARGE` handle.
pub fn parse_win_large(h: &[u8; HANDLE_LEN]) -> ([u8; 16], u32, u32) {
    let mut n = [0u8; 16];
    n.copy_from_slice(&h[..16]);
    (n, u32::from_le_bytes(h[16..20].try_into().unwrap()), u32::from_le_bytes(h[20..24].try_into().unwrap()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn table_mem() -> SharedRegion {
        SharedRegion::heap((TABLE_LEN + 4096) as usize).unwrap()
    }

    #[test]
    fn the_layout_is_what_the_section_says() {
        assert_eq!(ENTRIES, 255);
        assert_eq!(E_QSTATE, 96);
        assert_eq!(H_EPOCH + 8 + 48, ENTRY_LEN);
    }

    #[test]
    fn a_region_lives_announce_map_retire_unmap_clear() {
        let r = table_mem();
        let t = Table::new(&r, 4096).unwrap();
        t.wipe();
        t.publish(0xaa);
        assert!(t.header().valid);
        let i = t.free_entry().unwrap();
        assert_eq!(i, 1);
        let h = win_large_handle([7; 16], 32, 9);
        let ep = t.announce(i, Kind::WinLarge, 3, 64 << 20, &h);
        let e = t.entry(i);
        assert_eq!((e.pstate, e.kind, e.region, e.len, e.epoch), (PState::Announced, 2, 3, 64 << 20, ep));
        assert_eq!(parse_win_large(&e.handle), ([7; 16], 32, 9));
        assert_eq!(e.answer(), QState::None);
        // The peer maps it.
        t.answer(i, ep, QState::Mapped, Reason::None, 5);
        assert_eq!(t.entry(i).answer(), QState::Mapped);
        assert!(!t.retired(i));
        t.retire(i);
        // Still MAPPED: not retired until UNMAPPED for this epoch.
        assert!(!t.retired(i));
        t.answer(i, ep, QState::Unmapped, Reason::None, 6);
        assert!(t.retired(i));
        t.clear(i);
        assert_eq!(t.free_entry(), Some(1));
    }

    #[test]
    fn an_answer_for_an_old_epoch_is_not_an_answer() {
        let r = table_mem();
        let t = Table::new(&r, 4096).unwrap();
        t.wipe();
        t.publish(1);
        let e1 = t.announce(1, Kind::WinLarge, 1, 2 << 20, &[]);
        t.answer(1, e1, QState::Unmapped, Reason::None, 0);
        t.retire(1);
        t.clear(1);
        // The entry is reused: the old UNMAPPED must not retire the new one.
        let e2 = t.announce(1, Kind::WinLarge, 1, 2 << 20, &[]);
        assert_ne!(e1, e2);
        assert_eq!(t.entry(1).answer(), QState::None);
        t.retire(1);
        assert!(!t.retired(1));
    }

    #[test]
    fn a_new_provider_adopts_what_a_dead_one_left() {
        let r = table_mem();
        let t = Table::new(&r, 4096).unwrap();
        t.wipe();
        t.publish(1);
        let a = t.announce(1, Kind::WinLarge, 1, 2 << 20, &[]);
        let b = t.announce(2, Kind::WinLarge, 2, 4 << 20, &[]);
        t.answer(1, a, QState::Mapped, Reason::None, 0);
        // run 1 dies; run 2 starts
        t.publish(2);
        let orphans = t.adopt();
        assert_eq!(orphans.len(), 2);
        assert!(orphans.iter().all(|e| e.pstate == PState::Retiring && e.flags & FLAG_ORPHAN != 0));
        assert!(!t.retired(1) && !t.retired(2));
        t.answer(1, a, QState::Unmapped, Reason::None, 0);
        t.answer(2, b, QState::Unmapped, Reason::None, 0);
        assert!(t.retired(1) && t.retired(2));
        // Epochs never go back.
        let c = t.announce(3, Kind::WinLarge, 3, 2 << 20, &[]);
        assert!(c > b);
    }

    #[test]
    fn want_and_beats_reach_the_other_side() {
        let r = table_mem();
        let t = Table::new(&r, 4096).unwrap();
        t.wipe();
        t.publish(5);
        t.peer_attach(6);
        t.provider_beat();
        t.peer_beat();
        t.peer_beat();
        t.want(123 << 20);
        t.set_budget(1 << 30);
        let h = t.header();
        assert_eq!((h.provider_run, h.peer_run, h.provider_beat, h.peer_beat), (5, 6, 1, 2));
        assert_eq!((h.want_bytes, h.want_seq, h.budget), (123 << 20, 1, 1 << 30));
    }
}

/// The process's region wake: rung by the doorbell's `REGION_TOKEN` and by
/// every reopen or `RESCAN_TOKEN` (the rescan rule), waited on by a provider
/// with a region in flight instead of polling the table.
pub mod wake {
    use std::sync::{Condvar, Mutex};
    use std::time::Duration;

    static COUNT: Mutex<u64> = Mutex::new(0);
    static CV: Condvar = Condvar::new();

    pub fn notify() {
        *COUNT.lock().unwrap_or_else(|e| e.into_inner()) += 1;
        CV.notify_all();
    }

    /// Wait until a wake newer than `seen`, or `d`; updates `seen`.
    pub fn wait(seen: &mut u64, d: Duration) {
        let g = COUNT.lock().unwrap_or_else(|e| e.into_inner());
        let (g, _) = CV.wait_timeout_while(g, d, |n| *n == *seen).unwrap_or_else(|e| e.into_inner());
        *seen = *g;
    }

    pub fn current() -> u64 {
        *COUNT.lock().unwrap_or_else(|e| e.into_inner())
    }
}
