//! The GPU pool grows and shrinks (waycompose-design.md §108.6).
//!
//! The pool's region 0 is the boot device. When a lease does not fit, the
//! region manager asks its substrate (the keeper's large-page sections on
//! Windows, a stand-in file on Linux) for a region, announces it in the region
//! table, waits for the host's `MAPPED`, and adds it to the pool. Then the
//! lease is tried again. A region that has held no lease for the idle time is
//! drained, removed from the pool and retired. The substrate frees it only
//! after the host's `UNMAPPED` (rule 3, in `sv_ram::provider`).
//!
//! The table is a 64 KiB lease of region 0, and its offset is published in the
//! pool header (`poolhdr::OFF_REGION_TABLE`), which is where the host's peer
//! looks for it.

use crate::pool::PoolServer;
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use sv_arena::pool::Purpose;
use sv_ram::policy::{Policy, Usage};
use sv_ram::provider::{Event, Provider, Substrate};
use sv_transport::region::SharedRegion;

struct Live {
    entry: u32,
    len: u64,
    mapped: bool,
    idle_since: Option<Instant>,
}

pub struct RegionManager<S: Substrate + Send + 'static> {
    prov: Mutex<Provider<S>>,
    server: Arc<PoolServer>,
    policy: Policy,
    budget: u64,
    live: Mutex<BTreeMap<u32, Live>>,
    last_grow: Mutex<Instant>,
}

fn now_ms(t: Instant) -> u64 {
    t.elapsed().as_millis() as u64
}

impl<S: Substrate + Send + 'static> RegionManager<S> {
    /// Lease the table, publish where it is, take the table over (adopting
    /// what a previous GPU process left), and start growing on demand.
    pub fn start(boot: Arc<SharedRegion>, server: Arc<PoolServer>, substrate: S, budget: u64) -> Result<Arc<Self>, String> {
        let acct = server.open_session("regions");
        let lease = server.lease_raw(acct.session, sv_transport::regiontab::TABLE_LEN.max(64 << 10), Purpose::Control).map_err(|e| format!("the region table: {e}"))?;
        let (_, table_off) = sv_arena::pool::split(lease.offset);
        // SAFETY: `boot` lives for the process; the provider's view of it is
        // a borrow of the same mapping.
        let pboot = unsafe { SharedRegion::from_raw(boot.base_ptr(), boot.len()) };
        let run = u64::from_le_bytes(*b"svgpurgn") ^ crate::pool_guest::nonce();
        let prov = Provider::start(pboot, table_off, substrate, run)?;
        let adopted = prov.regions.len();
        prov.table().set_budget(budget);
        sv_transport::poolhdr::PoolHeader::new(&boot).map_err(|e| e.to_string())?.set_region_table(table_off);
        eprintln!(
            "[gpu] regions: the pool may grow by {} MiB from guest RAM; table at +{} KiB; {adopted} region(s) of a previous run adopted, freed when the host lets go",
            budget >> 20,
            table_off >> 10
        );
        let m = Arc::new(RegionManager {
            prov: Mutex::new(prov),
            server: Arc::clone(&server),
            policy: Policy::from_env(),
            budget,
            live: Mutex::new(BTreeMap::new()),
            last_grow: Mutex::new(Instant::now() - Duration::from_secs(3600)),
        });
        MANAGERS.lock().unwrap().push(Arc::clone(&m) as Arc<dyn RetireAll>);
        let g = Arc::clone(&m);
        server.set_grower(Box::new(move |need| g.grow(need)));
        let t = Arc::clone(&m);
        std::thread::Builder::new()
            .name("sv-pool-regions".into())
            .spawn(move || {
                loop {
                    std::thread::sleep(Duration::from_millis(250));
                    t.tick();
                }
            })
            .map_err(|e| e.to_string())?;
        Ok(m)
    }

    fn usage(&self) -> Usage {
        let (cap, free) = self.server.capacity_and_free();
        let added: u64 = self.live.lock().unwrap().values().map(|l| l.len).sum();
        Usage { used: cap - free, free, budget: self.budget.saturating_sub(added) }
    }

    /// Handle what the provider reports; called with the provider locked.
    fn absorb(&self, prov: &mut Provider<S>, evs: Vec<Event>) {
        for ev in evs {
            match ev {
                Event::Mapped { k, entry, rtt_us } => {
                    let r = &prov.regions[&entry];
                    let view = r.view.as_ref().expect("a region this run announced has a view");
                    // SAFETY: the provider keeps the view until the region is
                    // freed, which is after the pool has given it up.
                    let v = Arc::new(unsafe { SharedRegion::from_raw(view.base_ptr(), view.len()) });
                    match self.server.add_region(k, v) {
                        Ok(()) => {
                            if let Some(l) = self.live.lock().unwrap().get_mut(&k) {
                                l.mapped = true;
                            }
                            eprintln!("[gpu] regions: region {k} ({} MiB) mapped by the host in {:.2} ms, added to the pool; {}", r.len >> 20, rtt_us as f64 / 1e3, self.server.report());
                        }
                        Err(e) => {
                            eprintln!("[gpu] regions: region {k} could not join the pool ({e}); retiring it");
                            prov.retire(entry);
                        }
                    }
                }
                Event::Refused { k, reason } => {
                    self.live.lock().unwrap().remove(&k);
                    eprintln!("[gpu] regions: the host refused region {k}: {reason:?}");
                }
                Event::Freed { k, wait_us } => {
                    self.live.lock().unwrap().remove(&k);
                    eprintln!("[gpu] regions: region {k} given back to the guest ({:.2} ms after its retire)", wait_us as f64 / 1e3);
                }
            }
        }
    }

    /// A lease of `need` bytes did not fit. Add a region, waiting for the
    /// host, and say whether the lease should be tried again.
    pub fn grow(&self, need: u64) -> bool {
        let mut prov = self.prov.lock().unwrap();
        // Another thread may have grown while this one waited for the lock.
        if self.server.largest_free() >= need {
            return true;
        }
        let u = self.usage();
        let Some(len) = self.policy.on_demand(need, u) else {
            eprintln!("[gpu] regions: {} MiB more would pass the budget ({} MiB left)", need >> 20, u.budget >> 20);
            return false;
        };
        let Some(k) = self.server.next_region_id() else {
            eprintln!("[gpu] regions: every region slot is in use");
            return false;
        };
        let t = Instant::now();
        let entry = match prov.announce(k, len) {
            Ok(e) => e,
            Err(e) => {
                eprintln!("[gpu] regions: no region of {} MiB: {e}", len >> 20);
                return false;
            }
        };
        self.live.lock().unwrap().insert(k, Live { entry, len, mapped: false, idle_since: None });
        *self.last_grow.lock().unwrap() = Instant::now();
        eprintln!("[gpu] regions: region {k} of {} MiB announced for a lease of {} KiB (substrate {:.2} ms)", len >> 20, need >> 10, prov.regions[&entry].alloc_us as f64 / 1e3);
        loop {
            match prov.poll() {
                Ok(evs) => self.absorb(&mut prov, evs),
                Err(e) => {
                    eprintln!("[gpu] regions: {e}");
                    return false;
                }
            }
            match self.live.lock().unwrap().get(&k) {
                Some(l) if l.mapped => return true,
                None => return false,
                _ => {}
            }
            if t.elapsed() > Duration::from_secs(10) {
                // Left announced: a late MAPPED still adds it (tick).
                eprintln!("[gpu] regions: no answer for region {k} in 10 s; the lease fails, the region joins when the host answers");
                return false;
            }
            std::thread::sleep(Duration::from_micros(200));
        }
    }

    fn tick(&self) {
        let mut ahead = None;
        {
            let mut prov = self.prov.lock().unwrap();
            prov.beat();
            match prov.poll() {
                Ok(evs) => self.absorb(&mut prov, evs),
                Err(e) => eprintln!("[gpu] regions: {e}"),
            }
            let empty: Vec<u32> = self.server.empty_regions();
            let since_grow = now_ms(*self.last_grow.lock().unwrap());
            let retiring = prov.regions.values().any(|r| r.phase == sv_ram::provider::Phase::Retiring);
            let mut retire = None;
            {
                let mut live = self.live.lock().unwrap();
                let added = live_len(&live);
                let usage = self.usage_locked(&added);
                for (&k, l) in live.iter_mut() {
                    if !l.mapped {
                        continue;
                    }
                    if empty.contains(&k) {
                        if l.idle_since.is_none() {
                            eprintln!("[gpu] regions: region {k} holds no lease; it goes back after {} s idle ({} s inside the low-water margin)", self.policy.idle_ms / 1000, self.policy.deep_idle_ms / 1000);
                        }
                        let since = *l.idle_since.get_or_insert_with(Instant::now);
                        if retire.is_none() && self.policy.may_retire(l.len, now_ms(since), since_grow, retiring, usage) {
                            retire = Some((k, l.entry));
                        }
                    } else {
                        l.idle_since = None;
                    }
                }
            }
            if let Some((k, entry)) = retire {
                if self.server.remove_region(k).is_ok() {
                    if let Some(l) = self.live.lock().unwrap().get_mut(&k) {
                        l.mapped = false;
                    }
                    prov.retire(entry);
                    eprintln!("[gpu] regions: region {k} idle for {} s, retired; {}", self.policy.idle_ms / 1000, self.server.report());
                }
            }
            let u = self.usage();
            if u.used > 0 {
                ahead = self.policy.ahead(u);
            }
        }
        if let Some(len) = ahead {
            // Grow before the next lease has to wait.
            let _ = self.grow(len);
        }
    }

    fn usage_locked(&self, added: &u64) -> Usage {
        let (cap, free) = self.server.capacity_and_free();
        Usage { used: cap - free, free, budget: self.budget.saturating_sub(*added) }
    }
}

fn live_len(live: &BTreeMap<u32, Live>) -> u64 {
    live.values().map(|l| l.len).sum()
}

/// Something whose regions can all be given back at once.
trait RetireAll: Send + Sync {
    fn retire_everything(&self, timeout: Duration);
}

static MANAGERS: Mutex<Vec<Arc<dyn RetireAll>>> = Mutex::new(Vec::new());

/// An orderly exit (§108.5): every region that holds no lease is retired and,
/// once the host lets go, freed, waiting at most `timeout`. A region still
/// leased, or one the host does not answer for in time, stays with the
/// keeper, and the next GPU process adopts it.
pub fn retire_all(timeout: Duration) {
    let ms: Vec<Arc<dyn RetireAll>> = MANAGERS.lock().unwrap().clone();
    for m in ms {
        m.retire_everything(timeout);
    }
}

impl<S: Substrate + Send + 'static> RetireAll for RegionManager<S> {
    fn retire_everything(&self, timeout: Duration) {
        let mut prov = self.prov.lock().unwrap();
        let ks: Vec<(u32, u32)> = self.live.lock().unwrap().iter().filter(|(_, l)| l.mapped).map(|(&k, l)| (k, l.entry)).collect();
        let mut n = 0;
        for (k, entry) in ks {
            if self.server.remove_region(k).is_ok() {
                if let Some(l) = self.live.lock().unwrap().get_mut(&k) {
                    l.mapped = false;
                }
                prov.retire(entry);
                n += 1;
            }
        }
        if n == 0 {
            return;
        }
        eprintln!("[gpu] regions: exiting; {n} region(s) retired, waiting up to {} s for the host to let go", timeout.as_secs());
        let t = Instant::now();
        while t.elapsed() < timeout && prov.regions.values().any(|r| r.phase == sv_ram::provider::Phase::Retiring) {
            match prov.poll() {
                Ok(evs) => self.absorb(&mut prov, evs),
                Err(e) => {
                    eprintln!("[gpu] regions: {e}");
                    return;
                }
            }
            std::thread::sleep(Duration::from_millis(1));
        }
    }
}
