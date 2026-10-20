//! The guest-RAM substrate inside the broker daemon (waycompose-design.md
//! §108.15, §111.10; security-model.md R43, R44).
//!
//! The broker daemon (`wc-arena-host sessiond --broker`) runs as its own
//! system user, the only one on the host that may open QEMU's RAM memfd and
//! `/dev/udmabuf`. This module is the component it embeds
//! ([`GuestRamComponent`], a `wc_broker::host::Embedded`): the
//! [`Peer`](crate::peer::Peer) loop over the GPU pool's region table, with the
//! broker as the host's other mappers:
//!
//! * a region the guest announces is located, mapped and verified as before;
//!   then it is cut into udmabufs ([`crate::udma`]), each checked against the
//!   peer's own view, and published to the broker as pool `gpu.<k>` for the
//!   one host user the pool serves. Only then is it answered `MAPPED`;
//! * a client (an ICD, a proxy) asks the broker for `gpu.<k>` (`find`), gets
//!   the udmabufs, maps each at its fixed place, and holds the lease until it
//!   has unmapped them;
//! * on `RETIRING` the region is retired in the broker (its holders are
//!   told), and `UNMAPPED` is answered only once the broker reports that the
//!   last lease has ended ([`Publisher::on_drained`]): a closed lease, a
//!   closed connection, or the holder's exit;
//! * then the broker's own udmabufs are closed, and the guest frees the
//!   region.
//!
//! A client never sees the memfd, `/proc/<qemu>` or `/dev/udmabuf`. A
//! udmabuf reaches exactly its region's pages, and the kernel refuses a
//! mapping past them.
//!
//! **The daemon's own restart.** Its leases die with it, while clients may
//! still map regions. So every published region is recorded in the daemon's
//! state directory: its runs (the stamps no longer prove them once the guest
//! has written over them) and the processes that hold it (pid and start
//! time). Every lease is recorded synchronously before its handles leave
//! the broker. A failed record refuses the lease. A new run republishes the
//! recorded regions and waits for both its current leases and every recorded
//! holder to exit before answering `UNMAPPED`.

use crate::peer::{Mapped, Mappers, Peer, PeerEvent};
use crate::peerloop::{self, Ram, TableOff, Waker};
use crate::udma::{self, Limits, Part};
use std::collections::{BTreeMap, BTreeSet};
use std::os::fd::{AsRawFd, BorrowedFd, OwnedFd};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use sv_transport::lpstamp::UNIT;
use sv_transport::regiontab::PState;

/// What the broker offers the embedding process (`wc_broker::server::Broker`):
/// regions of its own, for one user, and word when one has drained.
pub trait Publisher: Send + Sync + 'static {
    /// Publish `pieces` (an fd each, and the range `(offset, len)` of the
    /// region it covers) as a region of pool `pool` for user `uid`.
    fn publish(&self, uid: u32, pool: &str, epoch: u64, pieces: Vec<(OwnedFd, u64, u64)>) -> u64;
    /// The region is to go: no new lease, and its holders are told.
    fn retire(&self, id: u64);
    /// The region is gone: the broker's own fds are closed.
    fn unpublish(&self, id: u64);
    /// Adds `f(id, epoch)`, called for every retiring region whose last
    /// lease has ended. It may be called from inside [`Publisher::retire`]
    /// and from the broker's threads, with the broker's lock held, so it must
    /// not call back.
    fn on_drained(&self, f: Box<dyn FnMut(u64, u64) + Send>);
    /// Check and persist a lease before its handles are sent.
    fn on_leased_checked(&self, f: wc_broker::server::LeaseCheck);
    /// The processes holding a lease on region `id`.
    fn holders(&self, id: u64) -> Vec<u32>;
}

impl Publisher for wc_broker::server::Broker<wc_broker::sys::unix::Unix> {
    fn publish(&self, uid: u32, pool: &str, epoch: u64, pieces: Vec<(OwnedFd, u64, u64)>) -> u64 {
        let pieces = pieces.into_iter().map(|(fd, o, l)| (wc_broker::sys::unix::Unix::object(fd), o, l)).collect();
        wc_broker::server::Broker::publish(self, wc_broker::User::uid(uid), pool, epoch, pieces)
    }
    fn retire(&self, id: u64) {
        wc_broker::server::Broker::retire(self, id)
    }
    fn unpublish(&self, id: u64) {
        wc_broker::server::Broker::unpublish(self, id)
    }
    fn on_drained(&self, f: Box<dyn FnMut(u64, u64) + Send>) {
        wc_broker::server::Broker::on_drained(self, f)
    }
    fn on_leased_checked(&self, f: wc_broker::server::LeaseCheck) {
        wc_broker::server::Broker::on_leased_checked(self, f)
    }
    fn holders(&self, id: u64) -> Vec<u32> {
        wc_broker::server::Broker::holders(self, id)
    }
}

/// The name a client finds region `k` of `pool` by.
pub fn region_pool(pool: &str, k: u32) -> String {
    format!("{pool}.{k}")
}

/// Makes a region's per-region handles from the memfd and its runs.
pub type MakeParts = dyn Fn(BorrowedFd<'_>, &[(u64, u64)]) -> std::io::Result<Vec<Part>> + Send;

/// udmabufs at the kernel's limits.
pub fn udmabufs() -> Box<MakeParts> {
    let l = Limits::read();
    Box::new(move |fd, runs| udma::create(fd, runs, l))
}

/// Whose the regions are.
pub enum Owner {
    /// One fixed uid.
    Uid(u32),
    /// The Linux user the live sessions run as: the guest's GPU process and
    /// its compositor are one guest user, whom users.conf maps to one Linux
    /// user. A region announced before any session is up waits for one, up
    /// to [`Owner::WAIT`]; sessions of two different users at once are
    /// refused, never guessed between.
    Sessions(Box<dyn Fn() -> Vec<u32> + Send>),
}

impl Owner {
    pub const WAIT: Duration = Duration::from_secs(10);

    fn resolve(&self) -> Result<u32, String> {
        match self {
            Owner::Uid(u) => Ok(*u),
            Owner::Sessions(f) => {
                let t = Instant::now();
                loop {
                    let mut uids = f();
                    uids.sort_unstable();
                    uids.dedup();
                    match uids.as_slice() {
                        [u] => return Ok(*u),
                        [] if t.elapsed() < Self::WAIT => std::thread::sleep(Duration::from_millis(50)),
                        [] => return Err(format!("no session is up after {} s: nobody to publish the region for", Self::WAIT.as_secs())),
                        many => return Err(format!("sessions of uids {many:?} are up at once: whose region this is cannot be told")),
                    }
                }
            }
        }
    }
}

/// A process, by pid and start time (field 22 of `/proc/PID/stat`), so a
/// reused pid is not taken for the holder.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Proc {
    pub pid: u32,
    pub start: u64,
}

impl Proc {
    /// `None`: the process is gone.
    pub fn of(pid: u32) -> Option<Proc> {
        let s = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        // The command is in parentheses and may hold spaces; fields follow
        // the last ')'.
        let rest = &s[s.rfind(')')? + 1..];
        let start = rest.split_whitespace().nth(19)?.parse().ok()?;
        Some(Proc { pid, start })
    }

    /// Whether this process still runs. A `/proc` that cannot be read at all
    /// (another user's, under `hidepid`) counts as running: a wrong "gone"
    /// would answer `UNMAPPED` early, and a wrong "running" only holds memory.
    pub fn alive(&self) -> bool {
        match std::fs::read_to_string(format!("/proc/{}/stat", self.pid)) {
            Ok(_) => Proc::of(self.pid) == Some(*self),
            Err(e) => e.kind() != std::io::ErrorKind::NotFound,
        }
    }
}

struct Published {
    id: u64,
    epoch: u64,
    runs: Vec<(u64, u64)>,
    retiring: bool,
    /// The holders as last recorded.
    holders: BTreeSet<Proc>,
    /// From a previous run of the daemon: these processes may still map it.
    inherited: BTreeSet<Proc>,
    held_logged: bool,
}

/// The broker as the peer's [`Mappers`].
pub struct BrokerMappers {
    publisher: Arc<dyn Publisher>,
    owner: Owner,
    uid: Option<u32>,
    pool: String,
    make: Box<MakeParts>,
    /// By (region, epoch): a pool may reuse a region index while its
    /// previous epoch still waits for its holders.
    live: BTreeMap<(u32, u64), Published>,
    drained: Arc<Mutex<BTreeSet<u64>>>,
    state: Option<PathBuf>,
    durable: Arc<Mutex<DurableRecords>>,
    recovery_error: Option<String>,
}

fn say(s: &str) {
    eprintln!("[guest-ram] {s}");
}

/// Map `p` read-only and compare the first bytes of each 2 MiB unit with the
/// peer's view at the same place of the region: the udmabuf must show the
/// pages the peer located, or it is not handed out.
fn verify(p: &Part, view: &sv_transport::region::SharedRegion) -> Result<(), String> {
    let len = p.len as usize;
    // SAFETY: a fresh shared read-only mapping of the udmabuf, unmapped below.
    let a = unsafe { libc::mmap(std::ptr::null_mut(), len, libc::PROT_READ, libc::MAP_SHARED, p.fd.as_raw_fd(), 0) };
    if a == libc::MAP_FAILED {
        return Err(format!("mapping the part at {}: {}", p.offset, std::io::Error::last_os_error()));
    }
    let mut bad = None;
    let mut at = 0u64;
    while at < p.len {
        // SAFETY: inside the mapping made above.
        let s = unsafe { std::slice::from_raw_parts((a as *const u8).add(at as usize), 64) };
        if !view.equals(p.offset + at, s).unwrap_or(false) {
            bad = Some(p.offset + at);
            break;
        }
        at += UNIT;
    }
    // SAFETY: the mapping made above.
    unsafe { libc::munmap(a, len) };
    match bad {
        Some(o) => Err(format!("the udmabuf does not show the region's bytes at {o}")),
        None => Ok(()),
    }
}

fn procs_text(p: &BTreeSet<Proc>) -> String {
    if p.is_empty() {
        return "-".into();
    }
    p.iter().map(|p| format!("{}@{}", p.pid, p.start)).collect::<Vec<_>>().join(",")
}

fn parse_procs(s: &str) -> Option<BTreeSet<Proc>> {
    if s == "-" {
        return Some(BTreeSet::new());
    }
    s.split(',')
        .map(|w| w.split_once('@').and_then(|(p, t)| Some(Proc { pid: p.parse().ok()?, start: t.parse().ok()? })))
        .collect()
}

/// One record of the state file.
#[derive(Clone)]
struct Record {
    k: u32,
    epoch: u64,
    runs: Vec<(u64, u64)>,
    holders: BTreeSet<Proc>,
}

type DurableRecords = BTreeMap<(u32, u64), (u64, Record)>;

// An integrity check, not authentication. The directory's uid permissions
// are the access control. This detects truncation even at a record boundary.
fn state_checksum(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |hash, b| (hash ^ u64::from(*b)).wrapping_mul(0x0000_0100_0000_01b3))
}

/// Atomic replacement and fsync complete before any lease handles are sent.
fn write_records<'a>(
    path: &std::path::Path,
    records: impl Iterator<Item = &'a Record>,
) -> std::io::Result<()> {
    use std::io::Write;
    let mut payload = String::new();
    for p in records {
        let runs = p
            .runs
            .iter()
            .map(|(o, l)| format!("{o}:{l}"))
            .collect::<Vec<_>>()
            .join(",");
        use std::fmt::Write as _;
        writeln!(payload, "region {} {} {} {}", p.k, p.epoch, runs, procs_text(&p.holders)).expect("writing a String");
    }
    let tmp = path.with_extension("tmp");
    let mut file = std::fs::File::create(&tmp)?;
    writeln!(file, "guest-ram-state-v1 {:016x}", state_checksum(payload.as_bytes()))?;
    file.write_all(payload.as_bytes())?;
    file.sync_all()?;
    std::fs::rename(&tmp, path)?;
    if let Some(parent) = path.parent() {
        std::fs::File::open(parent)?.sync_all()?;
    }
    Ok(())
}

impl BrokerMappers {
    /// `state`: the daemon's private directory, where published regions are
    /// recorded across its restarts. `None` is an explicitly in-memory
    /// simulation; production construction requires a directory.
    pub fn new(publisher: Arc<dyn Publisher>, owner: Owner, pool: &str, make: Box<MakeParts>, state: Option<PathBuf>, waker: Waker,
    ) -> BrokerMappers {
        let drained = Arc::new(Mutex::new(BTreeSet::new()));
        let d = Arc::clone(&drained);
        publisher.on_drained(Box::new(move |id, _epoch| {
            d.lock().unwrap_or_else(|e| e.into_inner()).insert(id);
            waker.wake();
        }));
        let durable = Arc::new(Mutex::new(DurableRecords::new()));
        let records = durable.clone();
        let prefix = format!("{pool}.");
        let path = state.as_ref().map(|d| d.join(format!("{pool}.regions")));
        publisher.on_leased_checked(Box::new(move |info, pid| {
            if !info.pool.starts_with(&prefix) {
                return Ok(());
            }
            let mut records = records.lock().unwrap_or_else(|e| e.into_inner());
            let record = records
                .values_mut()
                .find(|(id, _)| *id == info.id)
                .map(|(_, record)| record)
                .ok_or_else(|| std::io::Error::other("guest region is not recorded yet"))?;
            let holder = Proc::of(pid).ok_or_else(|| {
                std::io::Error::other("cannot identify the lease holder's start time")
            })?;
            record.holders.insert(holder);
            if let Some(path) = &path {
                write_records(path, records.values().map(|(_, record)| record))?;
            }
            Ok(())
        }));
        BrokerMappers { publisher, owner, uid: None, pool: pool.to_string(), make, live: BTreeMap::new(), drained, state,
            durable,
            recovery_error: None,
        }
    }

    fn state_file(&self) -> Option<PathBuf> {
        self.state.as_ref().map(|d| d.join(format!("{}.regions", self.pool)))
    }

    /// Polling cannot erase a lease recorded concurrently before handle send.
    /// Recorded holders remain until their region is removed.
    fn save(&self) {
        if self.recovery_error.is_some() {
            return; // Preserve the unreadable/corrupt evidence for recovery.
        }
        let mut records = self.durable.lock().unwrap_or_else(|e| e.into_inner());
        records.retain(|key, _| self.live.contains_key(key));
        for ((k, _), p) in &self.live {
            let (_, record) = records.entry((*k, p.epoch)).or_insert_with(|| {
                (
                    p.id,
                    Record {
                        k: *k,
                        epoch: p.epoch,
                        runs: p.runs.clone(),
                        holders: BTreeSet::new(),
                    },
                )
            });
            record
                .holders
                .extend(p.holders.union(&p.inherited).copied());
        }
        if let Some(path) = self.state_file()
            && let Err(e) = write_records(&path, records.values().map(|(_, record)| record)) {
            say(&format!("{}: {e}; new leases will be refused until their records can be written", path.display()));
        }
    }

    fn load(&self) -> Result<Vec<Record>, String> {
        // Explicitly in-memory simulations cannot recover a prior daemon.
        let Some(path) = self.state_file() else { return Ok(Vec::new()) };
        let text = std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        let invalid = || format!("{}: invalid or incomplete guest-RAM state", path.display());
        let payload = if let Some(rest) = text.strip_prefix("guest-ram-state-v1 ") {
            let (checksum, payload) = rest.split_once('\n').ok_or_else(invalid)?;
            let checksum = u64::from_str_radix(checksum, 16).map_err(|_| invalid())?;
            if checksum != state_checksum(payload.as_bytes()) {
                return Err(invalid());
            }
            payload
        } else {
            // Old journals cannot prove completeness: a torn/truncated file
            // can still contain valid rows, and older daemons recorded
            // holders only on their heartbeat. Never guess on upgrade.
            return Err(format!("{}: missing versioned state integrity header", path.display()));
        };
        if !payload.is_empty() && !payload.ends_with('\n') {
            return Err(invalid());
        }
        let mut out = Vec::new();
        let mut seen = BTreeSet::new();
        for line in payload.lines() {
            let words: Vec<_> = line.split(' ').collect();
            let ["region", k, epoch, runs, holders] = words.as_slice() else { return Err(invalid()) };
            let k: u32 = k.parse().map_err(|_| invalid())?;
            let epoch: u64 = epoch.parse().map_err(|_| invalid())?;
            if k == 0 || epoch == 0 || !seen.insert((k, epoch)) {
                return Err(invalid());
            }
            let runs: Vec<(u64, u64)> = runs.split(',').map(|r| {
                let (offset, len) = r.split_once(':').ok_or_else(invalid)?;
                let offset: u64 = offset.parse().map_err(|_| invalid())?;
                let len: u64 = len.parse().map_err(|_| invalid())?;
                if len == 0 || !offset.is_multiple_of(4096) || !len.is_multiple_of(4096) || offset.checked_add(len).is_none() {
                    return Err(invalid());
                }
                Ok((offset, len))
            }).collect::<Result<_, String>>()?;
            let holders = parse_procs(holders).ok_or_else(invalid)?;
            if holders.iter().any(|p| p.pid == 0) {
                return Err(invalid());
            }
            out.push(Record { k, epoch, runs, holders });
        }
        Ok(out)
    }

    fn owner_uid(&mut self) -> Result<u32, String> {
        let u = self.owner.resolve()?;
        if let Some(prev) = self.uid
            && prev != u
            && !self.live.is_empty()
        {
            return Err(format!("the pool's regions are uid {prev}'s, and the sessions now are uid {u}'s"));
        }
        self.uid = Some(u);
        Ok(u)
    }

    fn publish_parts(&mut self, uid: u32, k: u32, epoch: u64, runs: &[(u64, u64)], ram: BorrowedFd<'_>, view: Option<&sv_transport::region::SharedRegion>) -> Result<u64, String> {
        let parts = (self.make)(ram, runs).map_err(|e| format!("making the udmabufs: {e}"))?;
        if let Some(v) = view {
            for p in &parts {
                verify(p, v)?;
            }
        }
        let n = parts.len();
        let pieces = parts.into_iter().map(|p| (p.fd, p.offset, p.len)).collect();
        let name = region_pool(&self.pool, k);
        let id = self.publisher.publish(uid, &name, epoch, pieces);
        say(&format!("region {k} (epoch {epoch}, {} MiB, {n} udmabuf(s)) published as {name}, broker region {id}, for uid {uid}", runs.iter().map(|r| r.1).sum::<u64>() >> 20));
        Ok(id)
    }

    /// After a restart of the daemon: publish again what the previous run
    /// published and the table still holds (same region, same epoch), for
    /// the previous run's holders to find again; it is answered `UNMAPPED`
    /// only once they have all exited.
    /// A lost directory cannot prove there were no earlier transferred handles.
    /// Initialize only while the shared table has no live regions at all.
    fn initialize_empty(&mut self, peer: &Peer) {
        if (1..=sv_transport::regiontab::ENTRIES).any(|i| peer.table().entry(i).pstate != PState::Empty) {
            self.recovery_error = Some("new state directory with a nonempty region table".into());
            say("recovery blocked: new state directory with a nonempty region table");
            return;
        }
        if let Some(path) = self.state_file()
            && let Err(error) = write_records(&path, std::iter::empty()) {
                self.recovery_error = Some(error);
            }
    }

    pub fn inherit(&mut self, peer: &Peer) {
        if self.recovery_error.is_some() {
            return; // Recovery requires a trusted journal and a fresh daemon.
        }
        let records = match self.load() {
            Ok(records) => records,
            Err(error) => {
                say(&format!("recovery blocked: {error}; no region will be published or answered UNMAPPED"));
                self.recovery_error = Some(error);
                return;
            }
        };
        if self.state.is_some() {
            for i in 1..=sv_transport::regiontab::ENTRIES {
                let entry = peer.table().entry(i);
                if entry.pstate != PState::Empty
                    && (entry.answer() == sv_transport::regiontab::QState::Mapped
                        || (entry.pstate == PState::Retiring && entry.answer() == sv_transport::regiontab::QState::None))
                    && !records.iter().any(|r| r.k == entry.region && r.epoch == entry.epoch)
                {
                    let error = format!("state has no record for previously mapped/retiring region {} epoch {}", entry.region, entry.epoch);
                    say(&format!("recovery blocked: {error}"));
                    self.recovery_error = Some(error);
                    return;
                }
            }
        }
        for r in records {
            let live = (1..=sv_transport::regiontab::ENTRIES)
                .map(|i| peer.table().entry(i))
                .any(|e| e.region == r.k && e.epoch == r.epoch && e.pstate != PState::Empty);
            if !live {
                continue;
            }
            let alive: BTreeSet<Proc> = r.holders.iter().copied().filter(Proc::alive).collect();
            let uid = match self.owner_uid() {
                Ok(u) => u,
                Err(e) => {
                    // Not reachable for anyone now; still never answered
                    // while its old holders live.
                    say(&format!("region {} (epoch {}) of the previous run is not published again: {e}", r.k, r.epoch));
                    if !alive.is_empty() {
                        say(&format!("region {}: held for the previous run's holders {}", r.k, procs_text(&alive)));
                    }
                    self.live.insert((r.k, r.epoch), Published { id: u64::MAX, epoch: r.epoch, runs: r.runs, retiring: false, holders: BTreeSet::new(), inherited: alive, held_logged: false });
                    continue;
                }
            };
            match self.publish_parts(uid, r.k, r.epoch, &r.runs, peer.ram(), None) {
                Ok(id) => {
                    say(&format!("region {} (epoch {}): published by the previous run; its holders then: {}, alive now: {}", r.k, r.epoch, procs_text(&r.holders), procs_text(&alive)));
                    self.live.insert((r.k, r.epoch), Published { id, epoch: r.epoch, runs: r.runs, retiring: false, holders: BTreeSet::new(), inherited: alive, held_logged: false });
                }
                Err(e) => {
                    say(&format!("region {} (epoch {}) of the previous run: {e}", r.k, r.epoch));
                    // A failed re-publication must not forget old mappings.
                    self.live.insert(
                        (r.k, r.epoch),
                        Published {
                            id: u64::MAX,
                            epoch: r.epoch,
                            runs: r.runs,
                            retiring: false,
                            holders: BTreeSet::new(),
                            inherited: alive,
                            held_logged: false,
                        },
                    );
                }
            }
        }
        self.save();
    }

    /// Regions published now: `(k, broker id)`.
    pub fn published(&self) -> Vec<(u32, u64)> {
        self.live.iter().map(|((k, _), p)| (*k, p.id)).collect()
    }
}

impl Mappers for BrokerMappers {
    fn can_scan(&self) -> bool {
        self.recovery_error.is_none()
    }

    fn publish(&mut self, m: &Mapped, ram: BorrowedFd<'_>) -> Result<(), String> {
        if let Some(error) = &self.recovery_error {
            return Err(format!("guest-RAM recovery blocked: {error}"));
        }
        let uid = self.owner_uid()?;
        let id = self.publish_parts(uid, m.k, m.epoch, &m.runs, ram, Some(&m.view))?;
        self.live.insert((m.k, m.epoch), Published { id, epoch: m.epoch, runs: m.runs.clone(), retiring: false, holders: BTreeSet::new(), inherited: BTreeSet::new(), held_logged: false });
        self.save();
        Ok(())
    }

    fn let_go(&mut self, k: u32, epoch: u64) -> bool {
        if self.recovery_error.is_some() {
            return false;
        }
        // Never published for this epoch: nothing of the broker maps it.
        let Some(p) = self.live.get_mut(&(k, epoch)) else { return true };
        if !p.retiring {
            p.retiring = true;
            if p.id != u64::MAX {
                // May report the drain at once (no lease): the callback only
                // takes `drained`, which is not held here.
                self.publisher.retire(p.id);
            }
            say(&format!("region {k} (epoch {epoch}): retiring; UNMAPPED once its last lease has ended"));
        }
        let p = self.live.get_mut(&(k, epoch)).expect("present");
        let drained = p.id == u64::MAX || self.drained.lock().unwrap_or_else(|e| e.into_inner()).contains(&p.id);
        if !drained {
            return false;
        }
        p.inherited.retain(Proc::alive);
        if !p.inherited.is_empty() {
            if !p.held_logged {
                p.held_logged = true;
                say(&format!("region {k} (epoch {epoch}): drained here, but held for the previous run's holders {}", procs_text(&p.inherited)));
            }
            return false;
        }
        let id = p.id;
        if id != u64::MAX {
            self.drained.lock().unwrap_or_else(|e| e.into_inner()).remove(&id);
            self.publisher.unpublish(id);
        }
        self.live.remove(&(k, epoch));
        self.save();
        say(&format!("region {k} (epoch {epoch}): drained; the broker's udmabufs are closed"));
        true
    }

    fn tick(&mut self) {
        if self.recovery_error.is_some() {
            return;
        }
        let mut changed = false;
        for p in self.live.values_mut() {
            if p.id == u64::MAX {
                continue;
            }
            let now: BTreeSet<Proc> = self.publisher.holders(p.id).into_iter().filter_map(Proc::of).collect();
            let before = p.inherited.len();
            p.inherited.retain(Proc::alive);
            if now != p.holders || p.inherited.len() != before {
                p.holders = now;
                changed = true;
            }
        }
        if changed {
            self.save();
        }
    }
}

/// How the daemon serves one pool.
pub struct Config {
    pub peer: peerloop::Config,
    pub owner: Owner,
    /// The pool's name; region `k` is found as `{pool}.{k}`.
    pub pool: String,
    /// The daemon's private state directory. `None` is only for explicitly
    /// in-memory instruments, never the production GuestRamComponent.
    pub state: Option<PathBuf>,
}

/// Run the pool's peer in a thread of the daemon, publishing through
/// `publisher`. Returns once the table is attached (which waits for the GPU
/// pool to publish it, under [`TableOff::Auto`]); the thread runs for the
/// daemon's life.
pub fn spawn(publisher: Arc<dyn Publisher>, cfg: Config) -> Result<std::thread::JoinHandle<()>, String> {
    spawn_with(publisher, cfg, udmabufs())
}

pub fn spawn_with(publisher: Arc<dyn Publisher>, cfg: Config, make: Box<MakeParts>) -> Result<std::thread::JoinHandle<()>, String> {
    let mut fresh_state = false;
    if let Some(d) = &cfg.state {
        if let Some(parent) = d.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
        }
        match std::fs::create_dir(d) {
            Ok(()) => fresh_state = true,
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {},
            Err(e) => return Err(format!("{}: {e}", d.display())),
        }
    }
    std::thread::Builder::new()
        .name(format!("guest-ram-{}", cfg.pool))
        .spawn(move || {
            // The guest may start its GPU process long after the daemon:
            // attach waits for the table, and a failure is retried.
            let mut a = loop {
                match peerloop::attach(&cfg.peer) {
                    Ok(a) => break a,
                    Err(e) => {
                        say(&format!("pool {}: not attached yet: {e}; again in 5 s", cfg.pool));
                        std::thread::sleep(Duration::from_secs(5));
                    }
                }
            };
            say(&format!(
                "pool {}: bootstrap {} ({} MiB), guest RAM {} ({} MiB), table at +{}",
                cfg.pool,
                cfg.peer.boot.display(),
                a.boot_len >> 20,
                a.ram_path.display(),
                a.ram_len >> 20,
                a.peer.table_off()
            ));
            let (waker, rx) = peerloop::channel();
            let mut m = BrokerMappers::new(publisher, cfg.owner, &cfg.pool, make, cfg.state.clone(), waker.clone());
            if fresh_state {
                m.initialize_empty(&a.peer);
            }
            m.inherit(&a.peer);
            a.peer.set_mappers(Box::new(m));
            let r = peerloop::run(a, &cfg.peer, waker, rx, |_| {}, |_, evs| {
                for e in evs {
                    match e {
                        PeerEvent::Mapped { k, len, runs, locate_us, map_us, .. } => {
                            say(&format!("MAPPED region {k}: {} MiB, {runs} run(s), located in {locate_us} us, mapped and verified in {map_us} us", len >> 20))
                        }
                        PeerEvent::Refused { k, reason, why, .. } => say(&format!("REFUSED region {k}: {reason:?} ({why})")),
                        PeerEvent::Unmapped { k, .. } => say(&format!("UNMAPPED region {k}")),
                        PeerEvent::ProviderRun { run } => say(&format!("provider run {run:016x}")),
                    }
                }
            });
            if let Err(e) = r {
                say(&format!("pool {}: the peer stopped: {e}", cfg.pool));
            }
        })
        .map_err(|e| e.to_string())
}

/// The component the broker daemon embeds (`wc_broker::host::Embedded`),
/// made from its `--guest-ram-*` flags.
pub struct GuestRamComponent {
    flags: wc_broker::host::GuestRam,
    state: Option<PathBuf>,
    started: Mutex<bool>,
}

impl GuestRamComponent {
    /// `None` when the flags do not ask for guest RAM. `state`: the daemon's
    /// private state directory.
    pub fn new(flags: wc_broker::host::GuestRam, state: Option<PathBuf>) -> Result<Option<GuestRamComponent>, String> {
        if !flags.wanted() {
            return Ok(None);
        }
        if state.is_none() {
            return Err("the production guest-RAM component requires a durable state directory".into());
        }
        if flags.boot.is_none() {
            return Err("guest RAM needs --guest-ram-boot or --gpu-region: the file whose page 0 holds the GPU pool's header".into());
        }
        if flags.bell.as_deref().is_some_and(|b| sv_transport::brokered::service(b).is_some()) {
            return Err("--guest-ram-bell: this daemon is the broker; give the hub's socket path".into());
        }
        Ok(Some(GuestRamComponent { flags, state, started: Mutex::new(false) }))
    }

    fn config(&self, sessions: &wc_broker::host::SessionsView) -> Config {
        use wc_broker::host::{GuestRamUser, QemuRam};
        let ram = match self.flags.qemu.clone().expect("wanted") {
            QemuRam::Pid(p) => Ram::QemuPid(p),
            QemuRam::PidFile(f) => Ram::QemuPidFile(f),
            QemuRam::File(f) => Ram::Path(f),
        };
        let owner = match self.flags.user {
            Some(GuestRamUser::Uid(u)) => Owner::Uid(u),
            // `mapped`, and the default: whom the live sessions run as, which
            // the daemon already mapped through users.conf.
            Some(GuestRamUser::Mapped) | None => {
                let s = sessions.clone();
                Owner::Sessions(Box::new(move || s.all().into_iter().map(|s| s.uid).collect()))
            }
        };
        Config {
            peer: peerloop::Config {
                boot: self.flags.boot.clone().expect("checked in new"),
                table_off: TableOff::Auto,
                ram,
                bell: self.flags.bell.clone().map(PathBuf::from),
            },
            owner,
            pool: "gpu".into(),
            state: self.state.clone(),
        }
    }
}

impl wc_broker::host::Embedded for GuestRamComponent {
    fn name(&self) -> &str {
        "guest RAM (§108)"
    }

    fn attach(&self, broker: &wc_broker::server::Broker<wc_broker::sys::unix::Unix>, sessions: &wc_broker::host::SessionsView) {
        let mut started = self.started.lock().unwrap_or_else(|e| e.into_inner());
        if *started {
            return;
        }
        let publisher: Arc<dyn Publisher> = Arc::new(broker.clone());
        match spawn(publisher, self.config(sessions)) {
            Ok(_) => *started = true,
            Err(e) => say(&format!("not started: {e}")),
        }
    }
}

/// For [`MakeParts`] in tests: the parts are copies (memfds), not views; the
/// broker's side of the logic is the same.
#[cfg(test)]
fn copies() -> Box<MakeParts> {
    use std::os::unix::fs::FileExt;
    Box::new(|fd, runs| {
        let l = Limits { list_limit: 4, size_limit: 8 << 20 };
        let src = std::fs::File::from(fd.try_clone_to_owned()?);
        let mut out = Vec::new();
        for (offset, items) in udma::plan(runs, l) {
            let len: u64 = items.iter().map(|i| i.1).sum();
            let m = sv_transport::region::memfd("udma-copy", len).map_err(|e| std::io::Error::other(e.to_string()))?;
            let f = std::fs::File::from(m);
            let mut at = 0;
            for (o, n) in items {
                let mut b = vec![0u8; n as usize];
                src.read_exact_at(&mut b, o)?;
                f.write_all_at(&b, at)?;
                at += n;
            }
            out.push(Part { fd: OwnedFd::from(f), offset, len });
        }
        Ok(out)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::{Event, Provider};
    use crate::simram::SimGuestRam;
    use sv_transport::region::SharedRegion;
    use sv_transport::regiontab::TABLE_LEN;

    type Drained = Box<dyn FnMut(u64, u64) + Send>;

    #[derive(Default)]
    struct Fake {
        log: Mutex<Vec<String>>,
        /// id -> lease holders' pids (one entry per lease)
        leases: Mutex<BTreeMap<u64, Vec<u32>>>,
        retiring: Mutex<BTreeSet<u64>>,
        next: Mutex<u64>,
        cb: Mutex<Vec<Drained>>,
        lease_cb: Mutex<Vec<wc_broker::server::LeaseCheck>>,
        pieces: Mutex<BTreeMap<u64, Vec<(u64, u64)>>>,
    }

    impl Fake {
        fn open(&self, id: u64, pid: u32) {
            let info = wc_broker::core::RegionInfo { id, pool: "gpu.2".into(), len: 0, epoch: 0 };
            for f in self.lease_cb.lock().unwrap().iter_mut() {
                f(&info, pid).unwrap();
            }
            self.leases.lock().unwrap().get_mut(&id).unwrap().push(pid);
        }
        fn close(&self, id: u64) {
            let mut l = self.leases.lock().unwrap();
            let v = l.get_mut(&id).unwrap();
            v.pop();
            if v.is_empty() && self.retiring.lock().unwrap().contains(&id) {
                drop(l);
                for f in self.cb.lock().unwrap().iter_mut() {
                    f(id, 0);
                }
            }
        }
    }

    impl Publisher for Fake {
        fn publish(&self, uid: u32, pool: &str, epoch: u64, pieces: Vec<(OwnedFd, u64, u64)>) -> u64 {
            let mut n = self.next.lock().unwrap();
            *n += 1;
            self.log.lock().unwrap().push(format!("publish {uid} {pool} {epoch} {}", pieces.len()));
            self.pieces.lock().unwrap().insert(*n, pieces.iter().map(|p| (p.1, p.2)).collect());
            self.leases.lock().unwrap().insert(*n, Vec::new());
            *n
        }
        fn retire(&self, id: u64) {
            self.log.lock().unwrap().push(format!("retire {id}"));
            self.retiring.lock().unwrap().insert(id);
            if self.leases.lock().unwrap().get(&id).is_none_or(|v| v.is_empty()) {
                for f in self.cb.lock().unwrap().iter_mut() {
                    f(id, 0);
                }
            }
        }
        fn unpublish(&self, id: u64) {
            self.log.lock().unwrap().push(format!("unpublish {id}"));
        }
        fn on_drained(&self, f: Box<dyn FnMut(u64, u64) + Send>) {
            self.cb.lock().unwrap().push(f);
        }
        fn on_leased_checked(&self, f: wc_broker::server::LeaseCheck) {
            self.lease_cb.lock().unwrap().push(f);
        }
        fn holders(&self, id: u64) -> Vec<u32> {
            let mut v = self.leases.lock().unwrap().get(&id).cloned().unwrap_or_default();
            v.sort_unstable();
            v.dedup();
            v
        }
    }

    struct Rig {
        dir: PathBuf,
        boot: std::fs::File,
        ram_path: PathBuf,
    }

    impl Drop for Rig {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    fn rig(units: u64) -> Rig {
        static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("sv-ram-brokered-{}-{n}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let ram_path = dir.join("ram");
        std::fs::File::create(&ram_path).unwrap().set_len(units * UNIT).unwrap();
        let bp = dir.join("boot");
        let boot = std::fs::OpenOptions::new().read(true).write(true).create(true).truncate(true).open(&bp).unwrap();
        boot.set_len(TABLE_LEN + 4096).unwrap();
        Rig { dir, boot, ram_path }
    }

    fn view(f: &std::fs::File) -> SharedRegion {
        SharedRegion::map_fd(std::os::fd::AsFd::as_fd(f), 0, (TABLE_LEN + 4096) as usize).unwrap()
    }

    fn peer(r: &Rig) -> Peer {
        let ram = std::fs::OpenOptions::new().read(true).write(true).open(&r.ram_path).unwrap();
        Peer::attach(view(&r.boot), 4096, ram, 7).unwrap()
    }

    fn mappers(fake: &Arc<Fake>, owner: Owner, state: Option<PathBuf>) -> BrokerMappers {
        let (waker, _rx) = peerloop::channel();
        BrokerMappers::new(fake.clone(), owner, "gpu", copies(), state, waker)
    }

    /// The whole host side against a broker stand-in: published before
    /// MAPPED, for the pool's user under `gpu.K`; a retire held while a lease
    /// lives, answered when it ends, and the broker's fds closed first.
    #[test]
    fn regions_are_published_for_their_user_and_answered_unmapped_only_when_drained() {
        let r = rig(64);
        let sim = SimGuestRam::open(r.ram_path.to_str().unwrap()).unwrap();
        let mut p = Provider::start(view(&r.boot), 4096, sim, 1).unwrap();
        let mut peer = peer(&r);
        let fake = Arc::new(Fake::default());
        peer.set_mappers(Box::new(mappers(&fake, Owner::Uid(1000), None)));

        let e = p.announce(3, 20 << 20).unwrap();
        peer.scan(|_| {});
        let ev = p.poll().unwrap();
        assert!(matches!(ev[0], Event::Mapped { k: 3, .. }), "{ev:?}");
        let epoch = p.regions[&e].epoch;
        let log = fake.log.lock().unwrap().clone();
        assert_eq!(log, vec![format!("publish 1000 gpu.3 {epoch} 3")], "20 MiB at 8 MiB a part");
        let pieces = fake.pieces.lock().unwrap()[&1].clone();
        assert_eq!(pieces, vec![(0, 8 << 20), (8 << 20, 8 << 20), (16 << 20, 4 << 20)]);

        fake.open(1, std::process::id());
        p.retire(e);
        for _ in 0..3 {
            let ev = peer.scan(|_| {});
            assert!(!ev.iter().any(|e| matches!(e, PeerEvent::Unmapped { .. })), "answered with a lease alive: {ev:?}");
        }
        assert!(p.poll().unwrap().is_empty());
        assert_eq!(fake.log.lock().unwrap().iter().filter(|l| l.starts_with("retire")).count(), 1, "retired once");
        assert!(!fake.log.lock().unwrap().iter().any(|l| l.starts_with("unpublish")));
        fake.close(1);
        let ev = peer.scan(|_| {});
        assert!(ev.contains(&PeerEvent::Unmapped { entry: e, k: 3 }), "{ev:?}");
        assert_eq!(fake.log.lock().unwrap().last().unwrap(), "unpublish 1");
        assert!(matches!(p.poll().unwrap()[0], Event::Freed { k: 3, .. }));
    }

    /// A region nobody leased drains at once, inside `retire`.
    #[test]
    fn a_region_nobody_holds_is_answered_on_the_same_scan() {
        let r = rig(16);
        let sim = SimGuestRam::open(r.ram_path.to_str().unwrap()).unwrap();
        let mut p = Provider::start(view(&r.boot), 4096, sim, 1).unwrap();
        let mut peer = peer(&r);
        let fake = Arc::new(Fake::default());
        peer.set_mappers(Box::new(mappers(&fake, Owner::Uid(1000), None)));
        let e = p.announce(1, 4 << 20).unwrap();
        peer.scan(|_| {});
        p.poll().unwrap();
        p.retire(e);
        let ev = peer.scan(|_| {});
        assert!(ev.contains(&PeerEvent::Unmapped { entry: e, k: 1 }), "{ev:?}");
    }

    /// The owner is whoever the live sessions run as; two users at once are
    /// refused rather than guessed between.
    #[test]
    fn the_owner_is_the_sessions_user_and_never_a_guess() {
        let r = rig(16);
        let sim = SimGuestRam::open(r.ram_path.to_str().unwrap()).unwrap();
        let mut p = Provider::start(view(&r.boot), 4096, sim, 1).unwrap();
        let mut peer = peer(&r);
        let fake = Arc::new(Fake::default());
        let uids = Arc::new(Mutex::new(vec![1001u32, 1001]));
        let u2 = Arc::clone(&uids);
        peer.set_mappers(Box::new(mappers(&fake, Owner::Sessions(Box::new(move || u2.lock().unwrap().clone())), None)));
        p.announce(1, 2 << 20).unwrap();
        peer.scan(|_| {});
        assert!(fake.log.lock().unwrap()[0].starts_with("publish 1001 gpu.1 "));
        *uids.lock().unwrap() = vec![1001, 1002];
        p.announce(2, 2 << 20).unwrap();
        let ev = peer.scan(|_| {});
        assert!(matches!(ev.last(), Some(PeerEvent::Refused { k: 2, .. })), "{ev:?}");
        assert_eq!(fake.log.lock().unwrap().len(), 1);
    }

    /// A restarted daemon publishes again what its previous run published,
    /// and holds the retire until every holder the previous run recorded has
    /// exited, though the new broker has no lease on it.
    #[test]
    fn a_restarted_daemon_waits_for_the_previous_runs_holders() {
        let r = rig(32);
        let state = r.dir.join("state");
        std::fs::create_dir_all(&state).unwrap();
        let sim = SimGuestRam::open(r.ram_path.to_str().unwrap()).unwrap();
        let mut p = Provider::start(view(&r.boot), 4096, sim, 1).unwrap();
        // A process that holds the region and will exit.
        let mut child = std::process::Command::new("sleep").arg("30").spawn().unwrap();
        let e = {
            let mut peer = peer(&r);
            let fake = Arc::new(Fake::default());
            peer.set_mappers(Box::new(mappers(&fake, Owner::Uid(1000), Some(state.clone()))));
            let e = p.announce(2, 6 << 20).unwrap();
            peer.scan(|_| {});
            p.poll().unwrap();
            fake.open(1, child.id());
            // No beat: the lease is already durable before open returns.
            e
        };
        let text = std::fs::read_to_string(state.join("gpu.regions")).unwrap();
        assert!(text.contains(&format!(" {}@", child.id())), "{text}");
        // The daemon is gone; a new run.
        let mut peer = peer(&r);
        let fake = Arc::new(Fake::default());
        let mut m = mappers(&fake, Owner::Uid(1000), Some(state.clone()));
        m.inherit(&peer);
        assert_eq!(m.published().len(), 1);
        assert!(fake.log.lock().unwrap()[0].starts_with("publish 1000 gpu.2 "), "{:?}", fake.log.lock().unwrap());
        peer.set_mappers(Box::new(m));
        p.retire(e);
        for _ in 0..3 {
            let ev = peer.scan(|_| {});
            assert!(!ev.iter().any(|e| matches!(e, PeerEvent::Unmapped { .. })), "{ev:?}");
            peer.beat();
        }
        assert!(p.substrate.free_bytes() < 32 * UNIT, "still held by the guest");
        child.kill().unwrap();
        child.wait().unwrap();
        let ev = peer.scan(|_| {});
        assert!(ev.contains(&PeerEvent::Unmapped { entry: e, k: 2 }), "{ev:?}");
        assert!(matches!(p.poll().unwrap()[0], Event::Freed { k: 2, .. }));
    }

    /// The old mapper stays mapped while a new daemon cannot trust its
    /// journal. Neither retirement nor a heartbeat may discard that state.
    #[test]
    fn unreadable_or_corrupt_recovery_state_never_releases_an_existing_mapper() {
        for failure in ["directory", "lost-directory", "missing", "malformed", "empty", "truncated", "legacy", "missing-record"] {
            let r = rig(16);
            let state = r.dir.join("state");
            std::fs::create_dir(&state).unwrap();
            let path = state.join("gpu.regions");
            let sim = SimGuestRam::open(r.ram_path.to_str().unwrap()).unwrap();
            let mut provider = Provider::start(view(&r.boot), 4096, sim, 1).unwrap();
            let entry = {
                let mut old_peer = peer(&r);
                let old_broker = Arc::new(Fake::default());
                old_peer.set_mappers(Box::new(mappers(&old_broker, Owner::Uid(1000), Some(state.clone()))));
                let entry = provider.announce(2, 2 << 20).unwrap();
                old_peer.scan(|_| {});
                provider.poll().unwrap();
                old_broker.open(1, std::process::id());
                entry
            };
            let original = std::fs::read(&path).unwrap();
            let ram = std::fs::OpenOptions::new().read(true).write(true).open(&r.ram_path).unwrap();
            let old_mapper = SharedRegion::map_fd(std::os::fd::AsFd::as_fd(&ram), 0, (16 * UNIT) as usize).unwrap();
            let address = unsafe { old_mapper.base_ptr() };
            old_mapper.write_bytes(4096, b"old mapping remains live").unwrap();
            match failure {
                "directory" => {
                    std::fs::remove_file(&path).unwrap();
                    std::fs::create_dir(&path).unwrap();
                }
                "lost-directory" => {
                    std::fs::remove_dir_all(&state).unwrap();
                    std::fs::create_dir(&state).unwrap();
                }
                "missing" => std::fs::remove_file(&path).unwrap(),
                "malformed" => {
                    let payload = "region broken\n";
                    std::fs::write(&path, format!("guest-ram-state-v1 {:016x}\n{payload}", state_checksum(payload.as_bytes()))).unwrap();
                }
                "legacy" => std::fs::write(&path, std::str::from_utf8(&original).unwrap().split_once('\n').unwrap().1).unwrap(),
                "empty" => std::fs::write(&path, "").unwrap(),
                "truncated" => std::fs::write(&path, &original[..original.len() - 1]).unwrap(),
                "missing-record" => write_records(&path, std::iter::empty()).unwrap(),
                _ => unreachable!(),
            }
            let damaged = std::fs::read(&path).ok();
            let mut restarted_peer = peer(&r);
            let broker = Arc::new(Fake::default());
            let mut m = mappers(&broker, Owner::Uid(1000), Some(state.clone()));
            if failure == "lost-directory" {
                restarted_peer.table().answer(entry, restarted_peer.table().entry(entry).epoch, sv_transport::regiontab::QState::None, sv_transport::regiontab::Reason::None, 0);
                m.initialize_empty(&restarted_peer);
            }
            m.inherit(&restarted_peer);
            assert!(m.recovery_error.is_some(), "{failure}");
            assert!(!m.let_go(999, 1), "unknown epochs must also remain blocked");
            restarted_peer.set_mappers(Box::new(m));
            // A new announcement would otherwise fail publication and receive
            // REFUSED, which also permits the provider to free its memory.
            provider.announce(3, 2 << 20).unwrap();
            provider.retire(entry);
            for _ in 0..3 {
                let events = restarted_peer.scan(|_| {});
                assert!(events.is_empty(), "{failure}: {events:?}");
                restarted_peer.beat();
                assert!(provider.poll().unwrap().is_empty(), "{failure}");
            }
            assert_eq!(std::fs::read(&path).ok(), damaged, "do not overwrite damaged recovery state");
            assert_eq!(unsafe { old_mapper.base_ptr() }, address);
            assert!(old_mapper.equals(4096, b"old mapping remains live").unwrap());
            old_mapper.write_bytes(4096, b"still mapped").unwrap();
            assert!(provider.substrate.free_bytes() < 16 * UNIT, "{failure}: guest RAM was freed");
            assert!(broker.log.lock().unwrap().is_empty(), "no publication or drain while recovery is blocked");
        }
    }

    #[test]
    fn an_already_acknowledged_retirement_needs_no_recovery_record() {
        let r = rig(16);
        let state = r.dir.join("state");
        std::fs::create_dir(&state).unwrap();
        let sim = SimGuestRam::open(r.ram_path.to_str().unwrap()).unwrap();
        let mut provider = Provider::start(view(&r.boot), 4096, sim, 1).unwrap();
        let mut old_peer = peer(&r);
        let broker = Arc::new(Fake::default());
        old_peer.set_mappers(Box::new(mappers(&broker, Owner::Uid(1000), Some(state.clone()))));
        let entry = provider.announce(2, 2 << 20).unwrap();
        old_peer.scan(|_| {});
        provider.poll().unwrap();
        provider.retire(entry);
        assert!(old_peer.scan(|_| {}).iter().any(|e| matches!(e, PeerEvent::Unmapped { .. })));
        // The broker has durably removed the record; the provider has not
        // polled the completed UNMAPPED yet, so its entry is still RETIRING.
        let mut new_peer = peer(&r);
        let broker = Arc::new(Fake::default());
        let mut m = mappers(&broker, Owner::Uid(1000), Some(state));
        m.inherit(&new_peer);
        assert!(m.recovery_error.is_none());
        new_peer.set_mappers(Box::new(m));
        new_peer.scan(|_| {});
        assert!(matches!(provider.poll().unwrap().as_slice(), [Event::Freed { .. }]));
    }

    #[test]
    fn the_production_component_requires_durable_state() {
        let flags = wc_broker::host::GuestRam {
            boot: Some("bootstrap".into()),
            qemu: Some(wc_broker::host::QemuRam::File("ram".into())),
            ..Default::default()
        };
        assert!(GuestRamComponent::new(flags, None).is_err());
        assert!(GuestRamComponent::new(wc_broker::host::GuestRam::default(), None).unwrap().is_none());
    }

    #[test]
    fn a_beat_cannot_erase_a_holder_recorded_after_its_snapshot() {
        let r = rig(16);
        let state = r.dir.join("state");
        std::fs::create_dir_all(&state).unwrap();
        let sim = SimGuestRam::open(r.ram_path.to_str().unwrap()).unwrap();
        let mut provider = Provider::start(view(&r.boot), 4096, sim, 1).unwrap();
        let mut peer = peer(&r);
        let fake = Arc::new(Fake::default());
        peer.set_mappers(Box::new(mappers(&fake, Owner::Uid(1000), Some(state.clone()))));
        provider.announce(2, 2 << 20).unwrap();
        peer.scan(|_| {});
        provider.poll().unwrap();
        let info = wc_broker::core::RegionInfo { id: 1, pool: "gpu.2".into(), len: 2 << 20, epoch: 1 };
        // The callback has recorded a lease, while the beat's holder snapshot
        // is still empty. Updating that snapshot must not erase the record.
        fake.lease_cb.lock().unwrap()[0](&info, std::process::id()).unwrap();
        peer.beat();
        let text = std::fs::read_to_string(state.join("gpu.regions")).unwrap();
        assert!(text.contains(&format!(" {}@", std::process::id())), "{text}");
    }

    #[test]
    fn a_lease_with_an_unwritable_record_is_refused() {
        let r = rig(16);
        let state = r.dir.join("state");
        std::fs::create_dir_all(&state).unwrap();
        let sim = SimGuestRam::open(r.ram_path.to_str().unwrap()).unwrap();
        let mut provider = Provider::start(view(&r.boot), 4096, sim, 1).unwrap();
        let mut peer = peer(&r);
        let fake = Arc::new(Fake::default());
        peer.set_mappers(Box::new(mappers(&fake, Owner::Uid(1000), Some(state.clone()))));
        provider.announce(2, 2 << 20).unwrap();
        peer.scan(|_| {});
        provider.poll().unwrap();
        // Turn the record's destination into a directory. The atomic rename
        // must fail, and the new holder must receive no handles.
        std::fs::remove_file(state.join("gpu.regions")).unwrap();
        std::fs::create_dir(state.join("gpu.regions")).unwrap();
        let info = wc_broker::core::RegionInfo { id: 1, pool: "gpu.2".into(), len: 2 << 20, epoch: 1 };
        let mut callbacks = fake.lease_cb.lock().unwrap();
        assert!(callbacks[0](&info, std::process::id()).is_err());
        assert!(fake.holders(1).is_empty());
    }

    /// The pool reuses a region index while the previous epoch still waits
    /// for a holder: both are published, each answered on its own.
    #[test]
    fn a_reused_region_index_is_a_new_region_beside_the_retiring_one() {
        let r = rig(32);
        let sim = SimGuestRam::open(r.ram_path.to_str().unwrap()).unwrap();
        let mut p = Provider::start(view(&r.boot), 4096, sim, 1).unwrap();
        let mut peer = peer(&r);
        let fake = Arc::new(Fake::default());
        peer.set_mappers(Box::new(mappers(&fake, Owner::Uid(1000), None)));
        let a = p.announce(1, 4 << 20).unwrap();
        peer.scan(|_| {});
        p.poll().unwrap();
        fake.open(1, std::process::id());
        p.retire(a);
        peer.scan(|_| {});
        let b = p.announce(1, 4 << 20).unwrap();
        let ev = peer.scan(|_| {});
        assert!(ev.iter().any(|e| matches!(e, PeerEvent::Mapped { k: 1, .. })), "{ev:?}");
        assert_eq!(fake.log.lock().unwrap().iter().filter(|l| l.starts_with("publish 1000 gpu.1 ")).count(), 2);
        fake.close(1);
        let ev = peer.scan(|_| {});
        assert!(ev.contains(&PeerEvent::Unmapped { entry: a, k: 1 }), "{ev:?}");
        assert!(!ev.contains(&PeerEvent::Unmapped { entry: b, k: 1 }));
        assert_eq!(fake.log.lock().unwrap().last().unwrap(), "unpublish 1", "the old epoch's, not the new one's");
    }

    #[test]
    fn a_process_is_its_pid_and_start_time() {
        let me = Proc::of(std::process::id()).unwrap();
        assert!(me.alive());
        assert!(!Proc { start: me.start + 1, ..me }.alive(), "a reused pid is another process");
        assert_eq!(parse_procs(&procs_text(&[me].into())), Some([me].into()));
        assert_eq!(parse_procs("-"), Some(BTreeSet::new()));
    }
}
