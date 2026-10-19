//! The broker's serving loop, the same on every platform (§111.3).
//!
//! One thread per connection reads requests and answers them; one watcher
//! per platform reports process exits. Both take the one lock on [`Core`],
//! carry out its notices, and let go. Nothing here is on a data path: a
//! session asks a handful of questions when it starts, and the doorbells,
//! rings and regions it is given are its own from then on.

use crate::core::{Core, ConnId, DeviceSpec, Notice, ObjId, Peer, Pid, Refusal, Release};
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
}

/// How the broker is configured.
pub struct Config {
    pub devices: Vec<DeviceSpec>,
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

struct State<P: Platform> {
    core: Core,
    objs: BTreeMap<ObjId, Vec<Held<P::Object>>>,
    writers: BTreeMap<ConnId, Arc<Mutex<P::Writer>>>,
    on_drained: Option<Box<dyn FnMut(ObjId, u64) + Send>>,
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
}

impl<P: Platform> Clone for Broker<P> {
    fn clone(&self) -> Self {
        Broker {
            platform: Arc::clone(&self.platform),
            state: Arc::clone(&self.state),
            next_conn: Arc::clone(&self.next_conn),
            release: self.release,
            run: self.run,
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
        Broker {
            platform: Arc::new(platform),
            state: Arc::new(Mutex::new(State {
                core,
                objs: BTreeMap::new(),
                writers: BTreeMap::new(),
                on_drained: None,
            })),
            next_conn: Arc::new(AtomicU64::new(1)),
            release: config.release,
            run: config.run,
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
                    if let Some(f) = st.on_drained.as_mut() {
                        f(id, epoch);
                    }
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
            let released = st.objs.get(&id).and_then(|v| v.first()).is_some_and(|h| self.platform.released(&h.obj));
            if released {
                log(&format!("region {id}: its release word is set; freed"));
                st.core.released(id);
            }
        }
        self.settle(&mut st);
    }

    /// Serve one connection until it ends.
    pub fn client(&self, conn: P::Conn, writer: P::Writer, peer: Peer) {
        let id = self.next_conn.fetch_add(1, Ordering::Relaxed);
        let writer = Arc::new(Mutex::new(writer));
        {
            let mut st = self.lock();
            st.core.connect(id, peer.clone());
            st.writers.insert(id, Arc::clone(&writer));
        }
        let me = self.clone();
        let pid = peer.pid;
        if !self.platform.watch(pid, Box::new(move || me.exited(pid))) {
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
        st.core.process_exited(pid, Instant::now());
        self.settle(&mut st);
    }

    fn request(&self, id: ConnId, peer: &Peer, req: &Request, w: &mut P::Writer) -> io::Result<()> {
        let p = &*self.platform;
        let refused = |w: &mut P::Writer, r: Refusal| p.push(w, &format!("err {r}"));
        let mut st = self.lock();
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
                        let r = p.send(
                            w,
                            &|h| proto::region_line(&info, &[Piece { handle: h[0].clone(), offset: 0, len: info.len }]),
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
            Request::Open { id: rid } => match st.core.open(id, *rid) {
                Err(r) => refused(w, r),
                Ok(info) => {
                    let Some(held) = st.objs.get(rid) else {
                        let _ = st.core.close(id, *rid);
                        return p.push(w, &format!("err region {rid} has no object here"));
                    };
                    let pieces: Vec<(u64, u64)> = held.iter().flat_map(|h| h.pieces.iter().copied()).collect();
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
                        let _ = st.core.close(id, *rid);
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

    /// Called with each retiring region whose last lease has ended.
    pub fn on_drained(&self, f: Box<dyn FnMut(ObjId, u64) + Send>) {
        self.lock().on_drained = Some(f);
    }

    pub fn stats(&self) -> crate::core::Stats {
        self.lock().core.stats()
    }
}
