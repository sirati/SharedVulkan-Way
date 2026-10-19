//! The guest-RAM substrate inside the broker daemon (waycompose-design.md
//! §108.15, §111; security-model.md R43, R44).
//!
//! The broker daemon runs as its own system user. It is the only process on
//! the host that holds QEMU's RAM memfd and `/dev/udmabuf`. This module is
//! what it embeds: the [`Peer`](crate::peer::Peer) loop over a pool's region
//! table, with the broker as the host's other mappers:
//!
//! * a region the guest announces is located, mapped and verified as before;
//!   then it is cut into udmabufs ([`crate::udma`]), each checked against the
//!   peer's own view, and published to the broker as pool `{pool}.{k}` for
//!   the one host user the pool serves. Only then is it answered `MAPPED`;
//! * a client (an ICD, a proxy) asks the broker for `{pool}.{k}`, gets the
//!   udmabufs, maps each at its fixed place, and holds a lease until it has
//!   unmapped them;
//! * on `RETIRING` the region is retired in the broker (its lease holders
//!   are told), and `UNMAPPED` is answered only once the broker says the last
//!   lease has ended ([`Publisher::on_drained`]): a closed lease, a closed
//!   connection, or the holder's exit;
//! * then the broker's own udmabufs are closed, and the guest frees the
//!   region.
//!
//! A client never sees the memfd, `/proc/<qemu>` or `/dev/udmabuf`: a
//! udmabuf reaches exactly its region's pages, and the kernel refuses a
//! mapping past them.
//!
//! **The daemon's own restart.** Its leases die with it, while clients may
//! still map regions. So every published region is recorded in the daemon's
//! state directory (its runs, which the stamps no longer prove once the
//! guest has written over them). A new run publishes the recorded regions
//! the table still holds, so clients can find them again, and it never
//! answers `UNMAPPED` for one of them: the old run's holders are unknown.
//! That keeps guest memory allocated, and corrupts nothing (§108.4 rule 3).

use crate::peer::{Mapped, Mappers, Peer, PeerEvent};
use crate::peerloop::{self, Waker};
use crate::udma::{self, Limits, Part};
use std::collections::{BTreeMap, BTreeSet};
use std::os::fd::{AsRawFd, BorrowedFd, OwnedFd};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
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
    /// `f(id, epoch)` for every retiring region whose last lease has ended.
    /// It may be called from inside [`Publisher::retire`] and from the
    /// broker's threads, with the broker's lock held: it must not call back.
    fn on_drained(&self, f: Box<dyn FnMut(u64, u64) + Send>);
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

struct Published {
    id: u64,
    epoch: u64,
    runs: Vec<(u64, u64)>,
    retiring: bool,
    /// Published by a previous run of the daemon: its holders are unknown.
    inherited: bool,
    held_logged: bool,
}

/// The broker as the peer's [`Mappers`].
pub struct BrokerMappers {
    publisher: Arc<dyn Publisher>,
    uid: u32,
    pool: String,
    make: Box<MakeParts>,
    live: BTreeMap<u32, Published>,
    drained: Arc<Mutex<BTreeSet<u64>>>,
    state: Option<PathBuf>,
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

impl BrokerMappers {
    /// `state`: the daemon's private directory, where published regions are
    /// recorded across its restarts.
    pub fn new(publisher: Arc<dyn Publisher>, uid: u32, pool: &str, make: Box<MakeParts>, state: Option<PathBuf>, waker: Waker) -> BrokerMappers {
        let drained = Arc::new(Mutex::new(BTreeSet::new()));
        let d = Arc::clone(&drained);
        publisher.on_drained(Box::new(move |id, _epoch| {
            d.lock().unwrap_or_else(|e| e.into_inner()).insert(id);
            waker.wake();
        }));
        BrokerMappers { publisher, uid, pool: pool.to_string(), make, live: BTreeMap::new(), drained, state }
    }

    fn state_file(&self) -> Option<PathBuf> {
        self.state.as_ref().map(|d| d.join(format!("{}.regions", self.pool)))
    }

    /// Record what is published (atomically, mode 0600 in a private dir).
    fn save(&self) {
        let Some(path) = self.state_file() else { return };
        let mut s = String::new();
        for (k, p) in &self.live {
            let runs: Vec<String> = p.runs.iter().map(|(o, l)| format!("{o}:{l}")).collect();
            s.push_str(&format!("region {k} {} {}\n", p.epoch, runs.join(",")));
        }
        let tmp = path.with_extension("tmp");
        if let Err(e) = std::fs::write(&tmp, s).and_then(|_| std::fs::rename(&tmp, &path)) {
            say(&format!("{}: {e}; a restart of this daemon would not know what it published", path.display()));
        }
    }

    fn load(&self) -> Vec<(u32, u64, Vec<(u64, u64)>)> {
        let Some(path) = self.state_file() else { return Vec::new() };
        let Ok(text) = std::fs::read_to_string(&path) else { return Vec::new() };
        let mut out = Vec::new();
        for l in text.lines() {
            let w: Vec<&str> = l.split(' ').collect();
            let ["region", k, e, runs] = w.as_slice() else { continue };
            let (Ok(k), Ok(e)) = (k.parse(), e.parse()) else { continue };
            let runs: Option<Vec<(u64, u64)>> = runs
                .split(',')
                .map(|r| r.split_once(':').and_then(|(o, l)| Some((o.parse().ok()?, l.parse().ok()?))))
                .collect();
            if let Some(runs) = runs {
                out.push((k, e, runs));
            }
        }
        out
    }

    fn publish_parts(&mut self, k: u32, epoch: u64, runs: &[(u64, u64)], ram: BorrowedFd<'_>, view: Option<&sv_transport::region::SharedRegion>) -> Result<u64, String> {
        let parts = (self.make)(ram, runs).map_err(|e| format!("making the udmabufs: {e}"))?;
        if let Some(v) = view {
            for p in &parts {
                verify(p, v)?;
            }
        }
        let n = parts.len();
        let pieces = parts.into_iter().map(|p| (p.fd, p.offset, p.len)).collect();
        let name = region_pool(&self.pool, k);
        let id = self.publisher.publish(self.uid, &name, epoch, pieces);
        say(&format!("region {k} (epoch {epoch}, {} MiB, {n} udmabuf(s)) published as {name}, broker region {id}, for uid {}", runs.iter().map(|r| r.1).sum::<u64>() >> 20, self.uid));
        Ok(id)
    }

    /// After a restart of the daemon: publish again what the previous run
    /// published and the table still holds (same region, same epoch).
    pub fn inherit(&mut self, peer: &Peer) {
        for (k, epoch, runs) in self.load() {
            let live = (1..=sv_transport::regiontab::ENTRIES)
                .map(|i| peer.table().entry(i))
                .any(|e| e.region == k && e.epoch == epoch && e.pstate != PState::Empty);
            if !live {
                continue;
            }
            match self.publish_parts(k, epoch, &runs, peer.ram(), None) {
                Ok(id) => {
                    say(&format!("region {k} (epoch {epoch}): published by the previous run; it is never answered UNMAPPED by this one, whose holders are unknown"));
                    self.live.insert(k, Published { id, epoch, runs, retiring: false, inherited: true, held_logged: false });
                }
                Err(e) => say(&format!("region {k} (epoch {epoch}) of the previous run: {e}")),
            }
        }
        self.save();
    }

    /// Regions published now: `(k, broker id)`.
    pub fn published(&self) -> Vec<(u32, u64)> {
        self.live.iter().map(|(k, p)| (*k, p.id)).collect()
    }
}

impl Mappers for BrokerMappers {
    fn publish(&mut self, m: &Mapped, ram: BorrowedFd<'_>) -> Result<(), String> {
        if let Some(old) = self.live.remove(&m.k) {
            say(&format!("region {}: a new epoch {} while epoch {} was published; the old one is retired", m.k, m.epoch, old.epoch));
            self.publisher.retire(old.id);
        }
        let id = self.publish_parts(m.k, m.epoch, &m.runs, ram, Some(&m.view))?;
        self.live.insert(m.k, Published { id, epoch: m.epoch, runs: m.runs.clone(), retiring: false, inherited: false, held_logged: false });
        self.save();
        Ok(())
    }

    fn let_go(&mut self, k: u32, epoch: u64) -> bool {
        let Some(p) = self.live.get_mut(&k) else { return true };
        if p.epoch != epoch {
            // Never published for this epoch.
            return true;
        }
        if !p.retiring {
            p.retiring = true;
            let id = p.id;
            // May report the drain at once (no lease): the callback only
            // takes `drained`, which is not held here.
            self.publisher.retire(id);
            say(&format!("region {k} (epoch {epoch}): retiring; UNMAPPED once its last lease has ended"));
        }
        let p = self.live.get_mut(&k).expect("present");
        let drained = self.drained.lock().unwrap_or_else(|e| e.into_inner()).contains(&p.id);
        if !drained {
            return false;
        }
        if p.inherited {
            if !p.held_logged {
                p.held_logged = true;
                say(&format!("region {k} (epoch {epoch}): drained, but a previous run of this daemon published it; held"));
            }
            return false;
        }
        let id = p.id;
        self.drained.lock().unwrap_or_else(|e| e.into_inner()).remove(&id);
        self.publisher.unpublish(id);
        self.live.remove(&k);
        self.save();
        say(&format!("region {k} (epoch {epoch}): drained; the broker's udmabufs are closed"));
        true
    }
}

/// How the daemon serves one pool.
pub struct Config {
    pub peer: peerloop::Config,
    /// The host user the pool's regions are for.
    pub uid: u32,
    /// The pool's name; region `k` is found as `{pool}.{k}`.
    pub pool: String,
    /// The daemon's private state directory.
    pub state: Option<PathBuf>,
}

/// Run the pool's peer in a thread of the daemon, publishing through
/// `publisher`. Returns once the table is attached; the thread runs for the
/// daemon's life.
pub fn spawn(publisher: Arc<dyn Publisher>, cfg: Config) -> Result<std::thread::JoinHandle<()>, String> {
    spawn_with(publisher, cfg, udmabufs())
}

pub fn spawn_with(publisher: Arc<dyn Publisher>, cfg: Config, make: Box<MakeParts>) -> Result<std::thread::JoinHandle<()>, String> {
    if let Some(d) = &cfg.state {
        std::fs::create_dir_all(d).map_err(|e| format!("{}: {e}", d.display()))?;
    }
    let (waker, rx) = peerloop::channel();
    let (tx, ready) = std::sync::mpsc::channel::<Result<(), String>>();
    let h = std::thread::Builder::new()
        .name(format!("guest-ram-{}", cfg.pool))
        .spawn(move || {
            let mut a = match peerloop::attach(&cfg.peer) {
                Ok(a) => a,
                Err(e) => {
                    let _ = tx.send(Err(e));
                    return;
                }
            };
            say(&format!(
                "pool {} for uid {}: bootstrap {} ({} MiB), guest RAM {} ({} MiB), table at +{}",
                cfg.pool,
                cfg.uid,
                cfg.peer.boot.display(),
                a.boot_len >> 20,
                a.ram_path.display(),
                a.ram_len >> 20,
                a.peer.table_off()
            ));
            let mut m = BrokerMappers::new(publisher, cfg.uid, &cfg.pool, make, cfg.state.clone(), waker.clone());
            m.inherit(&a.peer);
            a.peer.set_mappers(Box::new(m));
            let _ = tx.send(Ok(()));
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
        .map_err(|e| e.to_string())?;
    match ready.recv() {
        Ok(Ok(())) => Ok(h),
        Ok(Err(e)) => Err(e),
        Err(_) => Err("the guest-RAM thread ended before attaching".into()),
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

    #[derive(Default)]
    struct Fake {
        log: Mutex<Vec<String>>,
        leases: Mutex<BTreeMap<u64, u32>>,
        retiring: Mutex<BTreeSet<u64>>,
        next: Mutex<u64>,
        cb: Mutex<Option<Box<dyn FnMut(u64, u64) + Send>>>,
        pieces: Mutex<BTreeMap<u64, Vec<(u64, u64)>>>,
    }

    impl Fake {
        fn close(&self, id: u64) {
            let mut l = self.leases.lock().unwrap();
            let n = l.get_mut(&id).unwrap();
            *n -= 1;
            if *n == 0 && self.retiring.lock().unwrap().contains(&id) {
                drop(l);
                (self.cb.lock().unwrap().as_mut().unwrap())(id, 0);
            }
        }
    }

    impl Publisher for Fake {
        fn publish(&self, uid: u32, pool: &str, epoch: u64, pieces: Vec<(OwnedFd, u64, u64)>) -> u64 {
            let mut n = self.next.lock().unwrap();
            *n += 1;
            self.log.lock().unwrap().push(format!("publish {uid} {pool} {epoch} {}", pieces.len()));
            self.pieces.lock().unwrap().insert(*n, pieces.iter().map(|p| (p.1, p.2)).collect());
            self.leases.lock().unwrap().insert(*n, 0);
            *n
        }
        fn retire(&self, id: u64) {
            self.log.lock().unwrap().push(format!("retire {id}"));
            self.retiring.lock().unwrap().insert(id);
            if self.leases.lock().unwrap().get(&id) == Some(&0) {
                (self.cb.lock().unwrap().as_mut().unwrap())(id, 0);
            }
        }
        fn unpublish(&self, id: u64) {
            self.log.lock().unwrap().push(format!("unpublish {id}"));
        }
        fn on_drained(&self, f: Box<dyn FnMut(u64, u64) + Send>) {
            *self.cb.lock().unwrap() = Some(f);
        }
    }

    struct Rig {
        _dir: PathBuf,
        boot: std::fs::File,
        ram_path: PathBuf,
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
        Rig { _dir: dir, boot, ram_path }
    }

    fn view(f: &std::fs::File) -> SharedRegion {
        SharedRegion::map_fd(f.as_fd(), 0, (TABLE_LEN + 4096) as usize).unwrap()
    }

    fn peer(r: &Rig) -> Peer {
        let ram = std::fs::OpenOptions::new().read(true).write(true).open(&r.ram_path).unwrap();
        Peer::attach(view(&r.boot), 4096, ram, 7).unwrap()
    }

    /// The whole host side against a broker stand-in: published before
    /// MAPPED, for the pool's user under `gpu.K`; a retire held while a lease
    /// lives, answered when it ends, and the broker's fds closed before.
    #[test]
    fn regions_are_published_for_their_user_and_answered_unmapped_only_when_drained() {
        let r = rig(64);
        let sim = SimGuestRam::open(r.ram_path.to_str().unwrap()).unwrap();
        let mut p = Provider::start(view(&r.boot), 4096, sim, 1).unwrap();
        let mut peer = peer(&r);
        let fake = Arc::new(Fake::default());
        let (waker, _rx) = peerloop::channel();
        let m = BrokerMappers::new(fake.clone(), 1000, "gpu", copies(), None, waker);
        peer.set_mappers(Box::new(m));

        let e = p.announce(3, 20 << 20).unwrap();
        peer.scan(|_| {});
        let ev = p.poll().unwrap();
        assert!(matches!(ev[0], Event::Mapped { k: 3, .. }), "{ev:?}");
        let epoch = p.regions[&e].epoch;
        let log = fake.log.lock().unwrap().clone();
        assert_eq!(log, vec![format!("publish 1000 gpu.3 {epoch} 3")], "20 MiB at 8 MiB a part");
        let pieces = fake.pieces.lock().unwrap()[&1].clone();
        assert_eq!(pieces, vec![(0, 8 << 20), (8 << 20, 8 << 20), (16 << 20, 4 << 20)]);

        // A client holds a lease.
        *fake.leases.lock().unwrap().get_mut(&1).unwrap() += 1;
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
        let (waker, _rx) = peerloop::channel();
        peer.set_mappers(Box::new(BrokerMappers::new(fake.clone(), 1000, "gpu", copies(), None, waker)));
        let e = p.announce(1, 4 << 20).unwrap();
        peer.scan(|_| {});
        p.poll().unwrap();
        p.retire(e);
        let ev = peer.scan(|_| {});
        assert!(ev.contains(&PeerEvent::Unmapped { entry: e, k: 1 }), "{ev:?}");
    }

    /// A restarted daemon publishes again what its previous run published,
    /// and holds the retire of such a region even when the new broker has no
    /// lease on it.
    #[test]
    fn a_restarted_daemon_publishes_its_regions_again_and_never_answers_them() {
        let r = rig(32);
        let state = r._dir.join("state");
        std::fs::create_dir_all(&state).unwrap();
        let sim = SimGuestRam::open(r.ram_path.to_str().unwrap()).unwrap();
        let mut p = Provider::start(view(&r.boot), 4096, sim, 1).unwrap();
        let e = {
            let mut peer = peer(&r);
            let fake = Arc::new(Fake::default());
            let (waker, _rx) = peerloop::channel();
            peer.set_mappers(Box::new(BrokerMappers::new(fake, 1000, "gpu", copies(), Some(state.clone()), waker)));
            let e = p.announce(2, 6 << 20).unwrap();
            peer.scan(|_| {});
            p.poll().unwrap();
            e
        };
        // The daemon is gone; a new run.
        let mut peer = peer(&r);
        let fake = Arc::new(Fake::default());
        let (waker, _rx) = peerloop::channel();
        let mut m = BrokerMappers::new(fake.clone(), 1000, "gpu", copies(), Some(state.clone()), waker);
        m.inherit(&peer);
        assert_eq!(m.published().len(), 1);
        assert!(fake.log.lock().unwrap()[0].starts_with("publish 1000 gpu.2 "), "{:?}", fake.log.lock().unwrap());
        peer.set_mappers(Box::new(m));
        p.retire(e);
        for _ in 0..3 {
            let ev = peer.scan(|_| {});
            assert!(!ev.iter().any(|e| matches!(e, PeerEvent::Unmapped { .. })), "{ev:?}");
        }
        assert!(p.substrate.free_bytes() < 32 * UNIT, "still held by the guest");
    }
}
