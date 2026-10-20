//! The broker's serving loop, the same on every platform (§111.3).
//!
//! One thread per connection reads requests and answers them; one watcher
//! per platform reports process exits. Both take the one lock on [`Core`],
//! carry out its notices, and let go. Nothing here is on a data path: a
//! session asks a handful of questions when it starts, and the doorbells,
//! rings and regions it is given are its own from then on.

use crate::core::{Core, ConnId, DeviceSpec, Notice, ObjId, Peer, Pid, Refusal, RegionInfo, Release, User};
use crate::proto::{self, Piece, Request};
use std::collections::BTreeMap;
use std::io::{self, BufRead, BufReader};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// A handle the broker gives a client: moved into it (a device, which has
/// one holder), or a copy of one the broker keeps (a region).
pub enum Give<'a, O> {
    Move(O),
    Copy(&'a O),
}

/// What a platform half provides (`sys::unix`, `sys::windows`).
pub trait Platform: Send + Sync + 'static {
    /// One connection. Reads lines; [`Platform::send`] writes them.
    type Conn: io::Read + Send + 'static;
    /// The writing half of a connection, shared with pushes.
    type Writer: Send + 'static;
    /// A kernel object the broker holds or gives: an fd, a HANDLE.
    type Object: Send + Sync + 'static;

    /// Wait for the next connection and say who it is.
    fn accept(&self) -> io::Result<(Self::Conn, Self::Writer, Peer)>;
    /// Call `exited` once, when process `pid` has exited. Returns false if
    /// the process could not be watched (it is already gone).
    fn watch(&self, pid: Pid, exited: Box<dyn FnOnce() + Send>) -> bool;
    /// Watch the exact authenticated connection process when the platform has
    /// a retained kernel identity, instead of looking it up again by PID.
    fn watch_for(&self, _writer: &Self::Writer, pid: Pid, exited: Box<dyn FnOnce() + Send>) -> bool {
        self.watch(pid, exited)
    }
    /// Open device `name` (the platform half knows where it is).
    fn open_device(&self, name: &str) -> io::Result<Self::Object>;
    /// Make the object of a new region.
    fn create_region(&self, len: u64, large: bool) -> io::Result<Self::Object>;
    /// Write `line` (made by `make` from the handle texts) and give `give`.
    fn send(
        &self,
        w: &mut Self::Writer,
        make: &dyn Fn(&[String]) -> String,
        give: Vec<Give<'_, Self::Object>>,
    ) -> io::Result<()>;
    /// Write a pushed line (no handles).
    fn push(&self, w: &mut Self::Writer, line: &str) -> io::Result<()>;
    /// Whether region `obj`, an orphan, has been released (the macOS keeper's
    /// release word, §103.15). Platforms without such a word say no.
    fn released(&self, _obj: &Self::Object) -> bool {
        false
    }
    /// Whether region `obj`, an orphan, carries its whole pool's release and
    /// says it is set (core rule 6: the macOS session's boot page, §111.9).
    /// Asked before [`Platform::released`]. Platforms without one say no.
    fn released_pool(&self, _obj: &Self::Object) -> bool {
        false
    }
    /// End process `pid`, which tied itself to a pool now released. Only
    /// called while the broker still watches the process.
    fn end(&self, _pid: Pid) {}
    /// A region was just made for `user` (§111.8): the platform may stamp it
    /// (§108.8, through its region-init hook) and record it where a restarted
    /// broker finds it again (the Windows holder). What it returns is one
    /// word for the end of the `ok region` reply; an error undoes the alloc.
    fn region_made(&self, _info: &RegionInfo, _user: &User, _obj: &Self::Object) -> io::Result<Option<String>> {
        Ok(None)
    }
    /// Region `id` is gone; the broker's own object is dropped after this.
    fn region_destroyed(&self, _id: ObjId) {}
    /// Regions a previous run of the broker held that still exist (the
    /// Windows holder's copies), with their users: each comes back as an
    /// orphan of its pool, for its user's next provider to adopt.
    fn restore(&self) -> Vec<(RegionInfo, User, Self::Object)> {
        Vec::new()
    }
    /// Who the caller is, as the boot record's identity (§111.6): the
    /// platform reads it from the kernel's credentials or token, never from
    /// anything the caller sent. `None`: this platform does not sign.
    fn identity(&self, _peer: &Peer) -> Option<wc_handle::macboot::Identity> {
        None
    }
    /// A fresh connection to local service `name` (the doorbell hub), for
    /// `connect`. Unlike a device, any number may be open at once.
    /// Connect using the exact authenticated connection target when a platform
    /// needs to retain process identity beyond its numeric PID.
    fn open_service_for(&self, _writer: &Self::Writer, _peer: &Peer, name: &str) -> io::Result<Self::Object> {
        self.open_service(name)
    }
    fn open_service(&self, name: &str) -> io::Result<Self::Object> {
        Err(io::Error::new(io::ErrorKind::Unsupported, format!("no service {name} on this platform")))
    }
}

/// How the broker is configured.
pub struct Config {
    pub devices: Vec<DeviceSpec>,
    /// Local services `connect` may open, and for whom.
    pub services: Vec<DeviceSpec>,
    /// Where the installations' boot-record keys are (`<dir>/<install>.key`),
    /// readable by the broker's account only. `None`: `sign` is refused.
    pub keys: Option<std::path::PathBuf>,
    /// The release rule of every region a client allocates.
    pub release: Release,
    pub budget: Option<u64>,
    /// Logged on start, and the `RUN` of `ok hello`.
    pub run: u64,
}

struct Held<O> {
    obj: O,
    pieces: Vec<(u64, u64)>,
}

/// A durable lease check, run before any region handle reaches a client.
pub type LeaseCheck = Box<dyn FnMut(&RegionInfo, Pid) -> io::Result<()> + Send>;

struct State<P: Platform> {
    core: Core,
    objs: BTreeMap<ObjId, Vec<Held<P::Object>>>,
    writers: BTreeMap<ConnId, Arc<Mutex<P::Writer>>>,
    on_drained: Vec<Box<dyn FnMut(ObjId, u64) + Send>>,
    quiescing: bool,
    on_leased: Vec<Box<dyn FnMut(ObjId, Pid) + Send>>,
    on_leased_checked: Vec<LeaseCheck>,
}

/// A running broker. Cloned into every connection's thread; also what a
/// process that embeds the broker (the host's substrate, §108.4) uses to
/// publish and retire regions of its own.
pub struct Broker<P: Platform> {
    platform: Arc<P>,
    state: Arc<Mutex<State<P>>>,
    next_conn: Arc<AtomicU64>,
    release: Release,
    run: u64,
    services: Arc<Vec<DeviceSpec>>,
    keys: Option<Arc<std::path::PathBuf>>,
}

impl<P: Platform> Clone for Broker<P> {
    fn clone(&self) -> Self {
        Broker {
            platform: Arc::clone(&self.platform),
            state: Arc::clone(&self.state),
            next_conn: Arc::clone(&self.next_conn),
            release: self.release,
            run: self.run,
            services: Arc::clone(&self.services),
            keys: self.keys.clone(),
        }
    }
}

fn log(s: &str) {
    eprintln!("[wc-broker] {s}");
}

impl<P: Platform> Broker<P> {
    pub fn new(platform: P, config: Config) -> Broker<P> {
        let mut core = Core::new(config.devices);
        core.set_budget(config.budget);
        let mut objs = BTreeMap::new();
        for (info, user, obj) in platform.restore() {
            log(&format!("region {} of pool {} ({} bytes, {user}): kept across a restart, now an orphan", info.id, info.pool, info.len));
            core.restore_orphan(&info, user, config.release, Instant::now());
            objs.insert(info.id, vec![Held { obj, pieces: vec![(0, info.len)] }]);
        }
        Broker {
            platform: Arc::new(platform),
            state: Arc::new(Mutex::new(State {
                core,
                objs,
                writers: BTreeMap::new(),
                on_drained: Vec::new(),
                quiescing: false,
                on_leased: Vec::new(),
                on_leased_checked: Vec::new(),
            })),
            next_conn: Arc::new(AtomicU64::new(1)),
            release: config.release,
            run: config.run,
            services: Arc::new(config.services),
            keys: config.keys.map(Arc::new),
        }
    }

    pub fn platform(&self) -> &P {
        &self.platform
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State<P>> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Carry out the core's notices. Called with the lock held.
    fn settle(&self, st: &mut State<P>) {
        for n in st.core.drain() {
            match n {
                Notice::Destroy(id) => {
                    self.platform.region_destroyed(id);
                    if st.objs.remove(&id).is_some() {
                        log(&format!("region {id}: the broker's handle is closed"));
                    }
                }
                Notice::Push(conn, line) => {
                    if let Some(w) = st.writers.get(&conn) {
                        let mut w = w.lock().unwrap_or_else(|e| e.into_inner());
                        let _ = self.platform.push(&mut w, &line);
                    }
                }
                Notice::Drained(id, epoch) => {
                    for f in st.on_drained.iter_mut() {
                        f(id, epoch);
                    }
                }
                Notice::End(pid) => {
                    log(&format!("process {pid}: its pool is released; ending it"));
                    self.platform.end(pid);
                }
            }
        }
    }

    /// Accept connections for ever, a thread each, plus a timer for the
    /// release rules.
    pub fn serve(&self) -> io::Result<()> {
        let tick = self.clone();
        std::thread::Builder::new().name("wc-broker-tick".into()).spawn(move || {
            loop {
                std::thread::sleep(Duration::from_secs(1));
                tick.tick();
            }
        })?;
        log(&format!("run {:016x}: serving", self.run));
        loop {
            let (conn, writer, peer) = match self.platform.accept() {
                Ok(c) => c,
                Err(e) => {
                    log(&format!("accept: {e}"));
                    std::thread::sleep(Duration::from_millis(100));
                    continue;
                }
            };
            let me = self.clone();
            std::thread::Builder::new().name("wc-broker-conn".into()).spawn(move || me.client(conn, writer, peer))?;
        }
    }

    /// The release rules' timer and probe.
    pub fn tick(&self) {
        let mut st = self.lock();
        st.core.tick(Instant::now());
        for id in st.core.probe_candidates() {
            // An earlier candidate's pool may have taken this one with it.
            let Some(obj) = st.objs.get(&id).and_then(|v| v.first()).map(|h| &h.obj) else { continue };
            if self.platform.released_pool(obj) {
                let pool = st.core.region(id).map(|r| r.pool.clone()).unwrap_or_default();
                log(&format!("region {id}: the release word of pool {pool} is set; freeing the pool's orphans"));
                st.core.pool_released(id);
            } else if self.platform.released(obj) {
                log(&format!("region {id}: its release word is set; freed"));
                st.core.released(id);
            }
        }
        self.settle(&mut st);
    }

    /// Refuse new client allocations before waiting for all regions to drain.
    /// The same lock protects allocation and this flag, so an empty region count
    /// cannot race a new allocation during normal daemon shutdown.
    pub fn quiesce(&self) {
        self.lock().quiescing = true;
    }

    /// A shutdown (§103.15): keep the release rules running for at most
    /// `limit` while orphans are held, so a host that is releasing them is
    /// waited for, and a shutdown never waits longer. Returns the orphans
    /// still held.
    pub fn drain_orphans(&self, limit: Duration) -> usize {
        let t0 = Instant::now();
        loop {
            self.tick();
            let left = self.stats().orphans;
            if left == 0 || t0.elapsed() >= limit {
                return left;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// Serve one connection until it ends.
    pub fn client(&self, conn: P::Conn, writer: P::Writer, peer: Peer) {
        let Ok(id) = self.next_conn.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| n.checked_add(1)) else {
            log("connection generations exhausted");
            return;
        };
        let writer = Arc::new(Mutex::new(writer));
        {
            let mut st = self.lock();
            st.core.connect(id, peer.clone());
            st.writers.insert(id, Arc::clone(&writer));
        }
        let me = self.clone();
        let pid = peer.pid;
        let watched = {
            let writer = writer.lock().unwrap_or_else(|e| e.into_inner());
            self.platform.watch_for(&writer, pid, Box::new(move || me.exited(pid)))
        };
        self.lock().core.owner_watch_ready(id, watched);
        if !watched {
            log(&format!("connection {id}: process {pid} cannot be watched; its connection stands for it"));
        }
        let mut rd = BufReader::new(conn);
        let mut line = String::new();
        loop {
            line.clear();
            match rd.read_line(&mut line) {
                Ok(0) | Err(_) => break,
                Ok(n) if n > proto::MAX_LINE => break,
                Ok(_) => {}
            }
            let mut w = writer.lock().unwrap_or_else(|e| e.into_inner());
            let r = match Request::parse(&line) {
                Ok(req) => self.request(id, &peer, &req, &mut w),
                Err(e) => self.platform.push(&mut w, &format!("err {e}")),
            };
            if r.is_err() {
                break;
            }
        }
        let mut st = self.lock();
        st.core.conn_closed(id, Instant::now());
        st.writers.remove(&id);
        self.settle(&mut st);
    }

    fn exited(&self, pid: Pid) {
        let mut st = self.lock();
        let role = st.core.role(pid).unwrap_or("?").to_string();
        let before = st.core.stats().orphans;
        st.core.process_exited(pid, Instant::now());
        let after = st.core.stats().orphans;
        log(&format!(
            "process {pid} ({role}) exited{}",
            if after > before { format!("; {} region(s) of it held as orphans", after - before) } else { String::new() }
        ));
        self.settle(&mut st);
    }

    fn request(&self, id: ConnId, peer: &Peer, req: &Request, w: &mut P::Writer) -> io::Result<()> {
        let p = &*self.platform;
        let refused = |w: &mut P::Writer, r: Refusal| p.push(w, &format!("err {r}"));
        let mut st = self.lock();
        if st.quiescing && matches!(req, Request::Alloc { .. }) {
            return p.push(w, "err broker is shutting down; new regions refused");
        }
        let out = match req {
            Request::Hello { role } => match st.core.hello(id, role) {
                Ok(()) => {
                    log(&format!("connection {id}: {role}, process {} of {}", peer.pid, peer.user));
                    p.push(w, &format!("ok hello {:016x} {} {}", self.run, peer.user, peer.pid))
                }
                Err(r) => refused(w, r),
            },
            Request::Device { name } => match st.core.device(id, name) {
                Err(r) => {
                    log(&format!("device {name} refused to process {} of {}: {r}", peer.pid, peer.user));
                    refused(w, r)
                }
                Ok(()) => match p.open_device(name) {
                    Ok(obj) => {
                        let n = name.clone();
                        let r = p.send(w, &|h| format!("ok device {n} {}", h[0]), vec![Give::Move(obj)]);
                        match &r {
                            Ok(()) => log(&format!("device {name} lent to process {} of {}", peer.pid, peer.user)),
                            Err(e) => {
                                log(&format!("device {name}: giving it to process {} failed: {e}", peer.pid));
                                st.core.device_failed(name, peer.pid);
                            }
                        }
                        r
                    }
                    Err(e) => {
                        st.core.device_failed(name, peer.pid);
                        p.push(w, &format!("err cannot open device {name}: {e}"))
                    }
                },
            },
            Request::Alloc { pool, len, large } => match st.core.alloc(id, pool, *len, self.release) {
                Err(r) => refused(w, r),
                Ok(info) => match p.create_region(*len, *large) {
                    Ok(obj) => {
                        let held = Held { obj, pieces: vec![(0, *len)] };
                        let note = match p.region_made(&info, &peer.user, &held.obj) {
                            Ok(n) => n,
                            Err(e) => {
                                st.core.alloc_failed(info.id);
                                return p.push(w, &format!("err cannot prepare region {}: {e}", info.id));
                            }
                        };
                        let r = p.send(
                            w,
                            &|h| {
                                let piece = [Piece { handle: h[0].clone(), offset: 0, len: info.len }];
                                proto::region_line_noted(&info, &piece, note.as_deref())
                            },
                            vec![Give::Copy(&held.obj)],
                        );
                        st.objs.insert(info.id, vec![held]);
                        log(&format!("region {} of pool {pool}: {len} bytes for process {} of {}", info.id, peer.pid, peer.user));
                        r
                    }
                    Err(e) => {
                        st.core.alloc_failed(info.id);
                        p.push(w, &format!("err cannot make a region of {len} bytes: {e}"))
                    }
                },
            },
            Request::Open { .. } | Request::Find { .. } => match match req {
                Request::Find { pool } => st.core.find(id, pool),
                Request::Open { id: rid } => st.core.open(id, *rid),
                _ => unreachable!(),
            } {
                Err(r) => refused(w, r),
                Ok(info) => {
                    let rid = info.id;
                    let Some(held) = st.objs.get(&rid) else {
                        let _ = st.core.close(id, rid);
                        return p.push(w, &format!("err region {rid} has no object here"));
                    };
                    let pieces: Vec<(u64, u64)> = held.iter().flat_map(|h| h.pieces.iter().copied()).collect();
                    // Before the fds leave: an embedder that records holders
                    // has this one before it can map anything.
                    for f in st.on_leased_checked.iter_mut() {
                        if let Err(e) = f(&info, peer.pid) {
                            let _ = st.core.close(id, rid);
                            return p.push(w, &format!("err lease record: {e}"));
                        }
                    }
                    for f in st.on_leased.iter_mut() {
                        f(rid, peer.pid);
                    }
                    let held = st.objs.get(&rid).expect("looked up above, under the same lock");
                    let give: Vec<Give<'_, P::Object>> = held.iter().map(|h| Give::Copy(&h.obj)).collect();
                    let r = p.send(
                        w,
                        &|h| {
                            let ps: Vec<Piece> = h
                                .iter()
                                .zip(&pieces)
                                .map(|(h, (o, l))| Piece { handle: h.clone(), offset: *o, len: *l })
                                .collect();
                            proto::region_line(&info, &ps)
                        },
                        give,
                    );
                    if r.is_err() {
                        let _ = st.core.close(id, rid);
                    }
                    r
                }
            },
            Request::Close { id: rid } => match st.core.close(id, *rid) {
                Ok(()) => p.push(w, "ok"),
                Err(r) => refused(w, r),
            },
            Request::Free { id: rid } => match st.core.free(id, *rid) {
                Ok(()) => p.push(w, "ok"),
                Err(r) => refused(w, r),
            },
            Request::Adopt { pool } => match st.core.adopt(id, pool) {
                Ok(v) => {
                    let list: Vec<String> = v.iter().map(|r| format!("{}:{}", r.id, r.len)).collect();
                    if !v.is_empty() {
                        log(&format!("pool {pool}: {} orphan(s) adopted by process {}", v.len(), peer.pid));
                    }
                    p.push(w, &format!("ok adopted{}{}", if list.is_empty() { "" } else { " " }, list.join(" ")))
                }
                Err(r) => refused(w, r),
            },
            Request::Tie { pool } => match st.core.tie(id, pool) {
                Ok(()) => {
                    log(&format!("process {} of {} ends with pool {pool}", peer.pid, peer.user));
                    p.push(w, "ok")
                }
                Err(r) => refused(w, r),
            },
            Request::Connect { name } => {
                let allowed = self.services.iter().find(|s| s.name == *name).map(|s| match &s.allow {
                    crate::core::Allow::AnyUser => true,
                    crate::core::Allow::Users(u) => u.contains(&peer.user) || peer.admin,
                });
                match allowed {
                    None => p.push(w, &format!("err no service {name} here")),
                    Some(false) => p.push(w, &format!("err service {name} is not for {}", peer.user)),
                    Some(true) => match p.open_service_for(w, peer, name) {
                        Ok(obj) => {
                            let n = name.clone();
                            p.send(w, &|h| format!("ok connect {n} {}", h[0]), vec![Give::Move(obj)])
                        }
                        Err(e) => p.push(w, &format!("err cannot connect to {name}: {e}")),
                    },
                }
            }
            Request::Sign(s) => match self.sign(peer, s) {
                Ok(line) => {
                    log(&format!("signed session {} of {} for process {} of {}", s.session, s.install, peer.pid, peer.user));
                    p.push(w, &line)
                }
                Err(e) => {
                    log(&format!("sign refused to process {} of {}: {e}", peer.pid, peer.user));
                    p.push(w, &format!("err {e}"))
                }
            },
            Request::Owner => match st.core.owner(id) {
                Ok(connection) => p.push(w, &format!("ok owner {} {connection}", self.run)),
                Err(r) => refused(w, r),
            },
            Request::ObserveOwner { run, connection, pid } => {
                if *run != self.run { p.push(w, "err owner belongs to another broker run") }
                else { match st.core.observe_owner(id, *connection, *pid) {
                    Ok(()) => p.push(w, "ok"), Err(r) => refused(w, r),
                } }
            },
            Request::Subscribe => match st.core.subscribe(id) {
                Ok(()) => p.push(w, "ok"),
                Err(r) => refused(w, r),
            },
            Request::Stats => {
                let s = st.core.stats();
                p.push(
                    w,
                    &format!(
                        "ok stats connections={} devices-held={} regions={} region-bytes={} orphans={} orphan-bytes={}",
                        s.connections, s.devices_held, s.regions, s.region_bytes, s.orphans, s.orphan_bytes
                    ),
                )
            }
        };
        self.settle(&mut st);
        out
    }

    /// `sign` (§111.6): the installation's key never leaves the broker, and
    /// the identity is the caller's as the platform read it.
    fn sign(&self, peer: &Peer, s: &proto::SignReq) -> Result<String, String> {
        use wc_handle::macboot::{Auth, Record, parse_key, valid_id, valid_installation};
        let dir = self.keys.as_ref().ok_or("this broker holds no installation keys")?;
        if !valid_installation(&s.install) || !valid_id(&s.session) {
            return Err(format!("bad installation {:?} or session {:?}", s.install, s.session));
        }
        let identity = self.platform.identity(peer).ok_or("this broker cannot tell who the caller is")?;
        let path = dir.join(format!("{}.key", s.install));
        let text = std::fs::read_to_string(&path).map_err(|e| format!("no key for installation {}: {e}", s.install))?;
        let key = parse_key(&text).ok_or_else(|| format!("the key of installation {} is malformed", s.install))?;
        let rec = Record {
            session: s.session.clone(),
            arena_run: s.arena_run,
            arena_pages: s.arena_pages,
            outbound: s.outbound,
            rings_run: s.rings_run,
            rings_pages: s.rings_pages,
            gpu: s.gpu,
        };
        let auth = Auth::sign_identity(&rec, s.index, Some(&identity), &s.install, s.seq, &key);
        let mac: String = auth.mac.iter().map(|b| format!("{b:02x}")).collect();
        Ok(format!("ok signed {} {mac} {}", s.seq, identity.text()))
    }

    // ------------------------------------------- the embedding process's own

    /// A region of the embedding process (the host's substrate): `user` may
    /// open it, and receives `pieces` (each an object and the range of the
    /// region it covers).
    pub fn publish(&self, user: crate::core::User, pool: &str, epoch: u64, pieces: Vec<(P::Object, u64, u64)>) -> ObjId {
        let len = pieces.iter().map(|p| p.2).sum();
        let mut st = self.lock();
        let info = st.core.publish(user, pool, len, epoch);
        let held = pieces.into_iter().map(|(obj, o, l)| Held { obj, pieces: vec![(o, l)] }).collect();
        st.objs.insert(info.id, held);
        info.id
    }

    pub fn retire(&self, id: ObjId) {
        let mut st = self.lock();
        st.core.retire(id);
        self.settle(&mut st);
    }

    pub fn unpublish(&self, id: ObjId) {
        let mut st = self.lock();
        st.core.unpublish(id);
        self.settle(&mut st);
    }

    /// Add a callback for each retiring region whose last lease has ended.
    /// Every callback added is called, in order. **They run under the
    /// broker's lock**: a callback must only hand the event on (send it on a
    /// channel), never call back into the broker.
    pub fn on_drained(&self, f: Box<dyn FnMut(ObjId, u64) + Send>) {
        self.lock().on_drained.push(f);
    }

    /// Add a callback for each lease (`open`, `find`), called with the
    /// region and the holder's pid **before** the region's handles are sent,
    /// so a holder record written from it cannot miss a lease taken just
    /// before a crash. Under the broker's lock, like [`Self::on_drained`].
    pub fn on_leased(&self, f: Box<dyn FnMut(ObjId, Pid) + Send>) {
        self.lock().on_leased.push(f);
    }

    /// Add a fallible lease check under the broker lock. A failure closes
    /// the new lease and sends no handles. Checks must never call the broker.
    pub fn on_leased_checked(&self, f: LeaseCheck) {
        self.lock().on_leased_checked.push(f);
    }

    /// Redeem one piece over another OS-authenticated local channel. The
    /// existing core lease is checked under the same lock that holds the
    /// objects alive and serializes close/retire. The callback must not call
    /// the broker. It must finish transferring the capability before return.
    pub fn with_region_lease<T>(&self, peer: &Peer, id: ObjId, epoch: u64, piece: usize,
        transfer: impl FnOnce(&P::Object, u64, u64) -> io::Result<T>) -> io::Result<T> {
        let st = self.lock();
        st.core.leased_region(peer, id, epoch).map_err(|e| io::Error::new(io::ErrorKind::PermissionDenied, e.0))?;
        let held = st.objs.get(&id).ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "region object missing"))?;
        let (obj, (offset, len)) = held.iter().flat_map(|h| h.pieces.iter().map(move |p| (&h.obj, *p)))
            .nth(piece).ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "region piece missing"))?;
        transfer(obj, offset, len)
    }

    /// The processes holding a lease on region `id` now, so that a
    /// restarted embedder can watch them again.
    pub fn holders(&self, id: ObjId) -> Vec<Pid> {
        self.lock().core.holders(id)
    }

    pub fn stats(&self) -> crate::core::Stats {
        self.lock().core.stats()
    }
}
