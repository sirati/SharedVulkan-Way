//! THE SHARED DOORBELL HUB (waycompose-design.md §99).
//!
//! QEMU serves each doorbell port's chardev (`svbell.sock` for the GPU rings,
//! `wcbell.sock` for the Wayland rings) to **one** connection. Until §99 the
//! first session's `sv-hostd` (and, for Wayland, its `wc-proxy`) took that
//! connection and kept it for as long as its processes lived, so a second
//! session's connection sat in QEMU's backlog unaccepted. Measured on the
//! guest (§97.10): a crashed session's host units outlived it, the next
//! session's hub could write but never be read, and Firefox's renderer thread
//! blocked in `send()` until every call it was making had timed out.
//!
//! So the port belongs to **no session**. One process per user, started by
//! systemd and not by any session, holds each upstream for as long as the VM
//! runs (reconnecting when QEMU restarts) and serves it on a socket of its own
//! to every session of every installation at once:
//!
//! * a byte from the guest goes to **every** client. A doorbell byte names a
//!   ring slot and lane, not a session (`sv_transport::park::lane_token`), so
//!   every client rings that lane in its own sessions: the right one wakes and
//!   the rest see a spurious wake, which the doorbell's framing argument prices
//!   at one syscall;
//! * a byte from any client goes to the guest;
//! * **nothing blocks on anybody.** Every client has its own bounded queue and
//!   writer thread, and so does the upstream. A full queue drops the byte and
//!   counts it: a bell is a hint, and a lost one costs a wait its slice. A
//!   client that stops reading, crashes, or hangs costs only its own bells.
//!
//! No descriptor crosses here either: the hub carries bytes.
use std::io::{Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, TrySendError, sync_channel};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Writes queued per client, and toward the guest, before a byte is dropped.
pub const QUEUE: usize = 256;

/// Counters, for the log and for tests.
#[derive(Default, Debug)]
pub struct Stats {
    pub down: AtomicU64,
    pub down_dropped: AtomicU64,
    pub up: AtomicU64,
    pub up_dropped: AtomicU64,
    pub clients: AtomicU64,
    pub upstream_connects: AtomicU64,
}

struct Client {
    id: u64,
    tx: SyncSender<Vec<u8>>,
    /// §105.13: the client's socket, written directly by the upstream's
    /// reader when nothing is queued for it, so a wake costs no thread hop;
    /// the queue and its writer take over only when a write would block.
    direct: UnixStream,
    pending: Arc<AtomicU64>,
}

/// The lanes a guest's byte names, woken by the hub itself
/// (waycompose-design.md §105.14).
///
/// A byte from the guest used to reach the thread parked on its lane in three
/// wakes on the host: this hub's reader, the proxy's doorman (a client of this
/// hub), and the doorman's futex wake of the waiting thread. Measured on the
/// Windows guest with the host idle, the last two cost 45–60 µs of a request
/// that crossed by doorbell. The lanes' wake words are in the ring region at
/// the tail of the shared-memory file, which this hub can map as well as the
/// proxy can; a `FUTEX_WAKE` on a shared file mapping reaches a waiter in any
/// process that maps the same file. So the hub rings the lane first, and the
/// waiter wakes on the hub's own thread's wake. The clients still get every
/// byte, as before: their wake of the same lane is a spurious one, and a
/// process that waits some other way (an eventfd per connection) still
/// depends on it.
///
/// The file's header names the ring region and the run that made it
/// (`wc_handle::ivshmem`); a new run is attached again, and a file with no
/// rings, or rings not made yet, rings nothing.
pub struct DirectRings {
    path: std::path::PathBuf,
    state: Mutex<DirectState>,
}

#[derive(Default)]
struct DirectState {
    head: Option<sv_transport::region::SharedRegion>,
    run: u64,
    control: Option<Arc<sv_transport::control::Control>>,
}

impl DirectRings {
    pub fn new(path: impl Into<std::path::PathBuf>) -> DirectRings {
        DirectRings { path: path.into(), state: Mutex::new(DirectState::default()) }
    }

    fn open(&self) -> Option<std::fs::File> {
        std::fs::OpenOptions::new().read(true).write(true).open(&self.path).ok()
    }

    /// The control region of the run the header names now, if it has one.
    fn current(&self, st: &mut DirectState) -> Option<Arc<sv_transport::control::Control>> {
        use std::os::fd::AsFd;
        use wc_handle::ivshmem::{ENCODED_LEN, Header};
        if st.head.is_none() {
            let f = self.open()?;
            st.head = sv_transport::region::SharedRegion::map_fd(f.as_fd(), 0, 4096).ok();
        }
        let head = st.head.as_ref()?;
        // The run id is at offset 16 (`wc_handle::ivshmem`), checked again by
        // the decode below whenever it changes.
        let run = head.atomic_u64(16).ok()?.load(Ordering::Acquire);
        if run == 0 {
            return None;
        }
        if run == st.run {
            return st.control.clone();
        }
        let mut b = [0u8; ENCODED_LEN];
        head.read_bytes(0, &mut b).ok()?;
        let h = Header::decode(&b).ok()?;
        if h.run_id != run {
            return None;
        }
        st.control = None;
        if h.rings.1 == 0 {
            st.run = run;
            return None;
        }
        let f = self.open()?;
        let region = sv_transport::region::SharedRegion::map_fd(f.as_fd(), h.rings.0, h.rings.1 as usize).ok()?;
        match sv_transport::control::Control::attach_for_bells(Arc::new(region)) {
            Ok(c) => {
                st.run = run;
                st.control = Some(Arc::new(c));
            }
            // The compositor publishes the header before it makes the rings:
            // look again at the next byte.
            Err(sv_transport::Error::NotReady) => {}
            Err(_) => st.run = run,
        }
        st.control.clone()
    }

    /// Wake the waiters of every lane `bytes` name; on `RESCAN_TOKEN`, every lane.
    /// Returns whether a lane it rang had a waiter parked on it.
    pub fn ring(&self, bytes: &[u8]) -> bool {
        use sv_transport::vdoorbell::BellTarget;
        let c = {
            let Ok(mut st) = self.state.lock() else { return false };
            self.current(&mut st)
        };
        let Some(c) = c else { return false };
        let woken = AtomicU64::new(0);
        let mut waiter = false;
        for &t in bytes {
            if t == sv_transport::park::RESCAN_TOKEN {
                c.rescan(&woken);
            } else if t & 0x80 == 0 {
                let (slot, lane) = sv_transport::park::lane_from_token(t);
                waiter |= sv_transport::ring::RingCtrl::open(&c, slot)
                    .and_then(|r| r.bell(lane).map(|b| b.parked.load(Ordering::Acquire) > 0))
                    .unwrap_or(false);
                c.ring_token(t, &woken);
            }
        }
        waiter
    }
}

/// One port: an upstream (QEMU's chardev) and the clients that share it.
pub struct Hub {
    /// §105.14: the rings this port's guest bytes name, rung before the
    /// clients are told.
    direct: std::sync::OnceLock<DirectRings>,
    name: String,
    clients: Mutex<Vec<Client>>,
    up: Mutex<Option<SyncSender<Vec<u8>>>>,
    /// §105.13: the upstream socket, for the same direct write toward the
    /// guest; `None` for a guest port (role b), which keeps the queue.
    up_direct: Mutex<Option<UnixStream>>,
    up_pending: Arc<AtomicU64>,
    next: AtomicU64,
    pub stats: Stats,
}

impl Hub {
    pub fn new(name: &str) -> Arc<Hub> {
        Arc::new(Hub {
            direct: std::sync::OnceLock::new(),
            name: name.to_string(),
            clients: Mutex::new(Vec::new()),
            up: Mutex::new(None),
            up_direct: Mutex::new(None),
            up_pending: Arc::new(AtomicU64::new(0)),
            next: AtomicU64::new(1),
            stats: Stats::default(),
        })
    }

    /// §105.14: ring the lanes of the rings in `file` directly as well, for
    /// every byte from the guest (and every rescan). Once; later calls are
    /// ignored.
    pub fn set_direct_rings(&self, file: impl Into<std::path::PathBuf>) {
        let _ = self.direct.set(DirectRings::new(file));
    }

    /// Bytes from the guest, to every client. Never blocks.
    fn fan_out(&self, b: &[u8]) {
        // §105.14: ring the lanes first, and if that woke a waiter, let it
        // run before the clients are told. Linux puts a thread this one wakes on this CPU (the wake
        // is "affine"), and on the VM host's two user cores the proxy's
        // control thread waited there for the fan-out below: 60 µs (median
        // of 41 traced requests, 40 of them on the hub's own CPU). One yield
        // costs a microsecond when nobody waits.
        if let Some(d) = self.direct.get()
            && d.ring(b)
        {
            std::thread::yield_now();
        }
        self.stats.down.fetch_add(b.len() as u64, Ordering::Relaxed);
        let Ok(mut c) = self.clients.lock() else { return };
        c.retain(|cl| {
            let at = if cl.pending.load(Ordering::Acquire) == 0 {
                match send_dontwait(&cl.direct, b) {
                    None => return false,
                    Some(n) if n == b.len() => return true,
                    Some(n) => n,
                }
            } else {
                0
            };
            cl.pending.fetch_add(1, Ordering::AcqRel);
            match cl.tx.try_send(b[at..].to_vec()) {
                Ok(()) => true,
                Err(TrySendError::Full(_)) => {
                    cl.pending.fetch_sub(1, Ordering::AcqRel);
                    self.stats.down_dropped.fetch_add(b.len() as u64, Ordering::Relaxed);
                    true
                }
                Err(TrySendError::Disconnected(_)) => false,
            }
        });
    }

    /// Bytes from a client, to the guest. Never blocks; with no upstream
    /// (the VM is down) they are dropped.
    fn up(&self, b: &[u8]) {
        // One writer at a time, so the direct write and the queue keep order.
        let direct = self.up_direct.lock().ok();
        let mut at = 0;
        if let Some(Some(s)) = direct.as_deref()
            && self.up_pending.load(Ordering::Acquire) == 0
        {
            match send_dontwait(s, b) {
                Some(n) if n == b.len() => {
                    self.stats.up.fetch_add(n as u64, Ordering::Relaxed);
                    return;
                }
                Some(n) => at = n,
                None => {}
            }
        }
        let sent = self
            .up
            .lock()
            .ok()
            .and_then(|u| {
                u.as_ref().map(|tx| {
                    self.up_pending.fetch_add(1, Ordering::AcqRel);
                    let ok = tx.try_send(b[at..].to_vec()).is_ok();
                    if !ok {
                        self.up_pending.fetch_sub(1, Ordering::AcqRel);
                    }
                    ok
                })
            })
            .unwrap_or(false);
        drop(direct);
        let n = b.len() as u64;
        if sent {
            self.stats.up.fetch_add(n, Ordering::Relaxed);
        } else {
            self.stats.up_dropped.fetch_add(n, Ordering::Relaxed);
        }
    }

    /// Serve one client: a writer thread fed by its own queue, and a reader
    /// that forwards what it sends.
    pub fn add_client(self: &Arc<Self>, s: UnixStream) -> std::io::Result<u64> {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = sync_channel::<Vec<u8>>(QUEUE);
        let mut w = s.try_clone()?;
        let direct = s.try_clone()?;
        let pending = Arc::new(AtomicU64::new(0));
        let p2 = Arc::clone(&pending);
        std::thread::Builder::new().name(format!("bell-{}-w{id}", self.name)).spawn(move || {
            while let Ok(b) = rx.recv() {
                let ok = w.write_all(&b).is_ok();
                p2.fetch_sub(1, Ordering::AcqRel);
                if !ok {
                    break;
                }
            }
            let _ = w.shutdown(std::net::Shutdown::Both);
        })?;
        self.clients.lock().map_err(|_| std::io::Error::other("poisoned"))?.push(Client { id, tx, direct, pending });
        self.stats.clients.fetch_add(1, Ordering::Relaxed);
        let hub = Arc::clone(self);
        let mut r = s;
        std::thread::Builder::new().name(format!("bell-{}-r{id}", self.name)).spawn(move || {
            let mut buf = [0u8; 256];
            loop {
                match r.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => hub.up(&buf[..n]),
                }
            }
            hub.remove(id);
        })?;
        Ok(id)
    }

    fn remove(&self, id: u64) {
        if let Ok(mut c) = self.clients.lock() {
            c.retain(|cl| cl.id != id);
        }
    }

    pub fn client_count(&self) -> usize {
        self.clients.lock().map(|c| c.len()).unwrap_or(0)
    }

    /// Run one upstream connection until it closes: a reader fanning out and a
    /// writer draining the up queue. Returns when the upstream is gone.
    pub fn serve_upstream(self: &Arc<Self>, up: UnixStream) -> std::io::Result<()> {
        self.serve_upstream_fresh(up, true)
    }

    /// [`Self::serve_upstream`]; `fresh` is a connection QEMU has just
    /// accepted, as opposed to one inherited from a previous hub that QEMU
    /// never saw close (§99.6). On a fresh one, bytes sent while there was
    /// none are lost in both directions (a virtio-serial port drops what
    /// arrives while nobody holds it), so the hub tells the guest to look at
    /// every ring again too (`park::RESCAN_TOKEN`, waycompose-design.md §105).
    pub fn serve_upstream_fresh(self: &Arc<Self>, up: UnixStream, fresh: bool) -> std::io::Result<()> {
        let w = up.try_clone()?;
        let closer = up.try_clone()?;
        *self.up_direct.lock().map_err(|_| std::io::Error::other("poisoned"))? = Some(up.try_clone()?);
        let r = self.serve_link_fresh(Box::new(up), Box::new(w), move || {
            let _ = closer.shutdown(std::net::Shutdown::Both);
        }, fresh);
        if let Ok(mut d) = self.up_direct.lock() {
            *d = None;
        }
        r
    }

    /// [`Hub::serve_upstream`] over any reader and writer: a chardev socket's
    /// two halves, or a guest port's (role (b)). `close` runs once the reader
    /// has ended, so a writer stuck on a dead upstream gives up.
    pub fn serve_link(
        self: &Arc<Self>,
        r: Box<dyn Read + Send>,
        w: Box<dyn Write + Send>,
        close: impl FnOnce(),
    ) -> std::io::Result<()> {
        self.serve_link_fresh(r, w, close, true)
    }

    /// [`Self::serve_link`], saying whether the link is fresh (see
    /// [`Self::serve_upstream_fresh`]): a guest port's open always is.
    pub fn serve_link_fresh(
        self: &Arc<Self>,
        mut r: Box<dyn Read + Send>,
        mut w: Box<dyn Write + Send>,
        close: impl FnOnce(),
        fresh: bool,
    ) -> std::io::Result<()> {
        let (tx, rx): (SyncSender<Vec<u8>>, Receiver<Vec<u8>>) = sync_channel(QUEUE);
        let up_pending = Arc::clone(&self.up_pending);
        let writer = std::thread::Builder::new().name(format!("bell-{}-up", self.name)).spawn(move || {
            while let Ok(b) = rx.recv() {
                let ok = w.write_all(&b).is_ok();
                up_pending.fetch_sub(1, Ordering::AcqRel);
                if !ok {
                    break;
                }
            }
        })?;
        *self.up.lock().map_err(|_| std::io::Error::other("poisoned"))? = Some(tx);
        self.stats.upstream_connects.fetch_add(1, Ordering::Relaxed);
        // §103.15: whatever the guest rang while this hub had no upstream is
        // gone, so every client rescans its rings (RESCAN_TOKEN).
        self.fan_out(&[sv_transport::park::RESCAN_TOKEN]);
        // §105: and on a connection QEMU has just accepted, the guest too: what
        // the host rang while there was none was dropped on its way in.
        if fresh {
            self.up(&[sv_transport::park::RESCAN_TOKEN]);
        }
        let mut buf = [0u8; 256];
        loop {
            match r.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => self.fan_out(&buf[..n]),
            }
        }
        // Dropping the sender ends the writer.
        if let Ok(mut u) = self.up.lock() {
            *u = None;
        }
        close();
        let _ = writer.join();
        Ok(())
    }
}

/// §105.13: write what the socket takes now, never waiting. `Some(n)` is
/// how much was taken (0 if it would block); `None` means the peer is gone.
fn send_dontwait(s: &UnixStream, b: &[u8]) -> Option<usize> {
    use std::os::fd::AsRawFd;
    // SAFETY: a live socket and a buffer of `b.len()` bytes.
    let r = unsafe { libc::send(s.as_raw_fd(), b.as_ptr().cast(), b.len(), libc::MSG_DONTWAIT | libc::MSG_NOSIGNAL) };
    if r >= 0 {
        return Some(r as usize);
    }
    match std::io::Error::last_os_error().kind() {
        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted => Some(0),
        _ => None,
    }
}

/// Bind `path` for this user alone: a stale socket file from an earlier run is
/// replaced, and the socket is 0600 in a directory this function makes 0700.
pub fn bind_private(path: &std::path::Path) -> std::io::Result<UnixListener> {
    use std::os::unix::fs::PermissionsExt;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    }
    let _ = std::fs::remove_file(path);
    let l = UnixListener::bind(path)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    Ok(l)
}

/// Where a port's bytes come from. On a Linux host that is QEMU's chardev
/// socket; inside a Linux guest whose host is a Mac running libkrun (role (b),
/// §103.6) there is no socket to share, only the guest's own
/// `/dev/virtio-ports/NAME`, which permits one opener, so the hub holds it for
/// every session there too. Linux discards bytes that reach a port nobody has
/// open, which is what the rescan after every (re)open covers.
#[derive(Clone, Debug)]
pub enum Upstream {
    Socket(std::path::PathBuf),
    GuestPort(String),
}

impl Upstream {
    /// `dev:NAME` is a guest port; anything else a chardev socket, as before.
    pub fn parse(spec: &str) -> Upstream {
        match spec.strip_prefix("dev:") {
            Some(name) => Upstream::GuestPort(name.to_owned()),
            None => Upstream::Socket(spec.into()),
        }
    }

    /// The name the log and systemd's store know this port by.
    pub fn name(&self) -> String {
        match self {
            Upstream::Socket(p) => p.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| "port".into()),
            Upstream::GuestPort(n) => n.clone(),
        }
    }

    fn connect(&self) -> std::io::Result<std::os::fd::OwnedFd> {
        match self {
            Upstream::Socket(p) => UnixStream::connect(p).map(Into::into),
            Upstream::GuestPort(n) => match sv_transport::vdoorbell::PortHandle::guest_port(n)? {
                sv_transport::vdoorbell::PortHandle::File(f) => Ok(f.into()),
                _ => Err(std::io::Error::other(format!("{n}: not a guest port"))),
            },
        }
    }

    /// Serve `fd` (from [`Upstream::connect`] or systemd's store) on `hub`.
    fn serve(&self, hub: &Arc<Hub>, fd: std::os::fd::OwnedFd, fresh: bool) -> std::io::Result<()> {
        match self {
            Upstream::Socket(_) => hub.serve_upstream_fresh(UnixStream::from(fd), fresh),
            // A port has no shutdown; the writer ends when its queue does.
            Upstream::GuestPort(_) => {
                let r = std::fs::File::from(fd);
                let w = r.try_clone()?;
                hub.serve_link_fresh(Box::new(r), Box::new(w), || {}, fresh)
            }
        }
    }
}

impl std::fmt::Display for Upstream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Upstream::Socket(p) => write!(f, "{}", p.display()),
            Upstream::GuestPort(n) => write!(f, "/dev/virtio-ports/{n}"),
        }
    }
}

impl From<std::path::PathBuf> for Upstream {
    fn from(p: std::path::PathBuf) -> Upstream {
        Upstream::Socket(p)
    }
}

/// Serve one port for ever: accept clients on `listener`, and keep an upstream
/// connection to `upstream` (QEMU's chardev), reconnecting whenever it closes
/// or is absent (the VM is stopped or restarting). Clients stay connected
/// across upstream reconnects. Spawns its threads and returns the hub.
pub fn run(name: &str, upstream: impl Into<Upstream>, listener: UnixListener) -> std::io::Result<Arc<Hub>> {
    run_kept(name, upstream.into(), listener, None, false)
}

/// Serve a broker-lent port without reopening its raw device or reconnecting
/// outside the broker. The existing bounded hub queues and rescan rule apply.
pub fn run_port(name: &str, port: sv_transport::vdoorbell::PortHandle,
    listener: UnixListener) -> std::io::Result<Arc<Hub>> {
    let hub = Hub::new(name);
    let (reader, writer) = port.split()?;
    let accepts = Arc::clone(&hub);
    std::thread::Builder::new().name(format!("bell-{name}-accept")).spawn(move || {
        for connection in listener.incoming() {
            match connection {
                Ok(client) => { let _ = accepts.add_client(client); }
                Err(_) => break,
            }
        }
    })?;
    let upstream = Arc::clone(&hub);
    std::thread::Builder::new().name(format!("bell-{name}-port")).spawn(move || {
        let _ = upstream.serve_link(reader, writer, || {});
    })?;
    Ok(hub)
}

/// [`run`], starting from an upstream connection a previous hub process left
/// open (§99.6), and, with `keep`, giving systemd a copy of every upstream
/// connection it makes (`{name}-up` in the unit's file-descriptor store) so
/// that the next restart of the hub inherits it and QEMU never sees the
/// connection close. A connection QEMU closed is taken out of the store.
pub fn run_kept(
    name: &str,
    upstream: Upstream,
    listener: UnixListener,
    inherited: Option<std::os::fd::OwnedFd>,
    keep: bool,
) -> std::io::Result<Arc<Hub>> {
    let hub = Hub::new(name);
    {
        let hub = Arc::clone(&hub);
        std::thread::Builder::new().name(format!("bell-{name}-accept")).spawn(move || {
            for conn in listener.incoming() {
                match conn {
                    Ok(s) => {
                        if let Err(e) = hub.add_client(s) {
                            eprintln!("bell-hub {}: a client could not be served: {e}", hub.name);
                        }
                    }
                    Err(e) => {
                        eprintln!("bell-hub {}: accept: {e}", hub.name);
                        std::thread::sleep(Duration::from_millis(200));
                    }
                }
            }
        })?;
    }
    {
        let hub = Arc::clone(&hub);
        std::thread::Builder::new().name(format!("bell-{name}-upstream")).spawn(move || {
            let mut said_absent = false;
            let mut next = inherited;
            let key = format!("{}-up", hub.name);
            loop {
                let inherited = next.is_some();
                let got = match next.take() {
                    Some(s) => {
                        eprintln!("bell-hub {}: kept {} open across the restart", hub.name, upstream);
                        Ok(s)
                    }
                    None => upstream.connect().inspect(|s| {
                        if keep && let Err(e) = crate::sdfd::store(&key, s) {
                            eprintln!("bell-hub {}: could not keep the connection in systemd's store: {e}", hub.name);
                        }
                    }),
                };
                match got {
                    Ok(s) => {
                        said_absent = false;
                        eprintln!(
                            "bell-hub {}: holding {} for {} client(s)",
                            hub.name,
                            upstream,
                            hub.client_count()
                        );
                        let _ = upstream.serve(&hub, s, !inherited);
                        if keep {
                            let _ = crate::sdfd::remove(&key);
                        }
                        eprintln!(
                            "bell-hub {}: {} closed (the VM stopped?); {} byte(s) down ({} dropped), {} up ({} \
                             dropped); reconnecting",
                            hub.name,
                            upstream,
                            hub.stats.down.load(Ordering::Relaxed),
                            hub.stats.down_dropped.load(Ordering::Relaxed),
                            hub.stats.up.load(Ordering::Relaxed),
                            hub.stats.up_dropped.load(Ordering::Relaxed),
                        );
                    }
                    Err(e) => {
                        if !said_absent {
                            eprintln!("bell-hub {}: {} not reachable yet ({e}); retrying", hub.name, upstream);
                            said_absent = true;
                        }
                    }
                }
                std::thread::sleep(Duration::from_millis(500));
            }
        })?;
    }
    Ok(hub)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    #[test]
    fn broker_lent_file_port_uses_bounded_hub_in_both_directions() {
        use std::os::fd::OwnedFd;
        let dir = tmpdir("lent-port");
        let path = dir.join("hub.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let (port, mut peer) = UnixStream::pair().unwrap();
        let file = std::fs::File::from(OwnedFd::from(port));
        let hub = run_port("lent-port", sv_transport::vdoorbell::PortHandle::File(file), listener).unwrap();
        let mut client = UnixStream::connect(&path).unwrap();
        client.write_all(&[7]).unwrap();
        assert_eq!(read_some(&mut peer, 1, Duration::from_secs(2)), [7]);
        peer.write_all(&[9]).unwrap();
        assert_eq!(read_some(&mut client, 1, Duration::from_secs(2)), [9]);
        assert!(hub.stats.upstream_connects.load(Ordering::Relaxed) >= 1);
        drop(peer);
        drop(client);
        std::fs::remove_dir_all(dir).unwrap();
    }

    fn tmpdir(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("bellhub-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// Bytes other than the hub's own `RESCAN_TOKEN`, which it sends on every
    /// fresh upstream (§105) and which [`a_fresh_upstream_tells_both_sides_to_rescan`]
    /// checks on its own.
    fn read_some(s: &mut UnixStream, want: usize, within: Duration) -> Vec<u8> {
        let mut v = read_raw(s, want + 1, within.min(Duration::from_millis(300)));
        v.retain(|b| *b != sv_transport::park::RESCAN_TOKEN);
        if v.len() >= want {
            return v;
        }
        v.extend(read_raw(s, want - v.len(), within).into_iter().filter(|b| *b != sv_transport::park::RESCAN_TOKEN));
        v
    }

    fn read_raw(s: &mut UnixStream, want: usize, within: Duration) -> Vec<u8> {
        s.set_read_timeout(Some(Duration::from_millis(50))).unwrap();
        let t = Instant::now();
        let mut got = Vec::new();
        let mut b = [0u8; 64];
        while got.len() < want && t.elapsed() < within {
            if let Ok(n) = s.read(&mut b) {
                if n == 0 {
                    break;
                }
                got.extend_from_slice(&b[..n]);
            }
        }
        got
    }

    /// QEMU's chardev, as the hub sees it: a listener that serves one
    /// connection at a time.
    fn fake_qemu(path: &std::path::Path) -> std::sync::mpsc::Receiver<UnixStream> {
        let l = UnixListener::bind(path).unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            for c in l.incoming().flatten() {
                if tx.send(c).is_err() {
                    break;
                }
            }
        });
        rx
    }

    /// Every client gets the guest's bytes, the guest gets every client's,
    /// and a client that never reads stops nobody: the others still get
    /// every byte and the guest still gets theirs.
    #[test]
    fn a_stalled_client_blocks_no_other_and_not_the_guest() {
        let d = tmpdir("stall");
        let qemu = fake_qemu(&d.join("qemu.sock"));
        let hub = run("t", d.join("qemu.sock"), bind_private(&d.join("hub.sock")).unwrap()).unwrap();
        let mut guest = qemu.recv_timeout(Duration::from_secs(5)).unwrap();
        let mut a = UnixStream::connect(d.join("hub.sock")).unwrap();
        let _stalled = UnixStream::connect(d.join("hub.sock")).unwrap(); // never reads
        let mut c = UnixStream::connect(d.join("hub.sock")).unwrap();
        let t = Instant::now();
        while hub.client_count() < 3 && t.elapsed() < Duration::from_secs(5) {
            std::thread::sleep(Duration::from_millis(10));
        }
        // Far more than a socket buffer and the stalled client's queue hold.
        let burst = vec![7u8; 4 << 20];
        let t = Instant::now();
        guest.write_all(&burst).unwrap();
        assert!(t.elapsed() < Duration::from_secs(10), "the guest's writes blocked");
        let got_a = read_some(&mut a, 1, Duration::from_secs(5));
        let got_c = read_some(&mut c, 1, Duration::from_secs(5));
        assert!(!got_a.is_empty() && !got_c.is_empty(), "readers starved by a stalled client");
        assert!(hub.stats.down_dropped.load(Ordering::Relaxed) > 0, "the stalled client's bytes were dropped");
        // And upward: a client's byte still reaches the guest.
        a.write_all(&[42]).unwrap();
        let up = read_some(&mut guest, 1, Duration::from_secs(5));
        assert_eq!(up, vec![42]);
        let _ = std::fs::remove_dir_all(&d);
    }

    /// The VM restarting closes the upstream; the hub reconnects by itself
    /// and its clients, still connected, carry on.
    #[test]
    fn clients_survive_the_upstream_reconnecting() {
        let d = tmpdir("reconnect");
        let qemu = fake_qemu(&d.join("qemu.sock"));
        let hub = run("t", d.join("qemu.sock"), bind_private(&d.join("hub.sock")).unwrap()).unwrap();
        let g1 = qemu.recv_timeout(Duration::from_secs(5)).unwrap();
        let mut a = UnixStream::connect(d.join("hub.sock")).unwrap();
        let t = Instant::now();
        while hub.client_count() < 1 && t.elapsed() < Duration::from_secs(5) {
            std::thread::sleep(Duration::from_millis(10));
        }
        drop(g1); // the VM went away
        let mut g2 = qemu.recv_timeout(Duration::from_secs(5)).expect("the hub reconnected");
        // §103.15: the client is told to rescan (bells in the gap are lost),
        // and then gets the new upstream's bytes. One token per upstream
        // connect it was there for: the first connect's may also reach it.
        g2.write_all(&[9]).unwrap();
        let mut got = Vec::new();
        let t = Instant::now();
        while !got.ends_with(&[9]) && t.elapsed() < Duration::from_secs(5) {
            got.extend(read_raw(&mut a, 1, Duration::from_secs(5)));
        }
        let (last, before) = got.split_last().expect("bytes from the hub");
        assert_eq!(*last, 9);
        assert!(!before.is_empty() && before.iter().all(|b| *b == sv_transport::park::RESCAN_TOKEN), "{got:?}");
        a.write_all(&[3]).unwrap();
        assert_eq!(read_some(&mut g2, 1, Duration::from_secs(5)), vec![3]);
        assert!(hub.stats.upstream_connects.load(Ordering::Relaxed) >= 2);
        let _ = std::fs::remove_dir_all(&d);
    }

    /// §105: a virtio-serial port drops what arrives while nobody holds it, so
    /// on every fresh upstream connection the hub tells the guest and every
    /// client to look at all their rings again; on one inherited across its own
    /// restart (QEMU never saw it close) it does not.
    #[test]
    fn a_fresh_upstream_tells_both_sides_to_rescan() {
        use sv_transport::park::RESCAN_TOKEN;
        let d = tmpdir("rescan");
        let qemu = fake_qemu(&d.join("qemu.sock"));
        let hub = run("t", d.join("qemu.sock"), bind_private(&d.join("hub.sock")).unwrap()).unwrap();
        let mut g1 = qemu.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(read_raw(&mut g1, 1, Duration::from_secs(5)), vec![RESCAN_TOKEN], "the guest is told at once");
        let mut a = UnixStream::connect(d.join("hub.sock")).unwrap();
        let t = Instant::now();
        while hub.client_count() < 1 && t.elapsed() < Duration::from_secs(5) {
            std::thread::sleep(Duration::from_millis(10));
        }
        drop(g1);
        let mut g2 = qemu.recv_timeout(Duration::from_secs(5)).expect("the hub reconnected");
        assert_eq!(read_raw(&mut g2, 1, Duration::from_secs(5)), vec![RESCAN_TOKEN]);
        assert_eq!(read_raw(&mut a, 1, Duration::from_secs(5)), vec![RESCAN_TOKEN], "and so is every client");
        // An inherited connection is not fresh: nothing was lost on it.
        let (mine, mut theirs) = UnixStream::pair().unwrap();
        let l = bind_private(&d.join("hub2.sock")).unwrap();
        let _hub2 = run_kept("t2", Upstream::Socket(d.join("absent.sock")), l, Some(mine.into()), false).unwrap();
        assert!(read_raw(&mut theirs, 1, Duration::from_millis(300)).is_empty());
        let _ = std::fs::remove_dir_all(&d);
    }

    /// A client that goes away is forgotten, and the rest are unaffected.
    #[test]
    fn a_client_leaving_affects_no_other() {
        let d = tmpdir("leave");
        let qemu = fake_qemu(&d.join("qemu.sock"));
        let hub = run("t", d.join("qemu.sock"), bind_private(&d.join("hub.sock")).unwrap()).unwrap();
        let mut g = qemu.recv_timeout(Duration::from_secs(5)).unwrap();
        let a = UnixStream::connect(d.join("hub.sock")).unwrap();
        let mut b = UnixStream::connect(d.join("hub.sock")).unwrap();
        let t = Instant::now();
        while hub.client_count() < 2 && t.elapsed() < Duration::from_secs(5) {
            std::thread::sleep(Duration::from_millis(10));
        }
        drop(a);
        let t = Instant::now();
        while hub.client_count() > 1 && t.elapsed() < Duration::from_secs(5) {
            g.write_all(&[1]).unwrap();
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(hub.client_count(), 1);
        g.write_all(&[5]).unwrap();
        let t = Instant::now();
        let mut seen = false;
        while !seen && t.elapsed() < Duration::from_secs(5) {
            seen = read_some(&mut b, 4096, Duration::from_millis(300)).contains(&5);
        }
        assert!(seen, "the remaining client still gets the guest's bytes");
        // 0600 in a 0700 directory: this user's sessions only.
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(std::fs::metadata(d.join("hub.sock")).unwrap().permissions().mode() & 0o777, 0o600);
        let _ = std::fs::remove_dir_all(&d);
    }

    /// §105.14: a thread parked on a lane of rings in a shared file is woken
    /// by the hub's own reading of the guest's byte, with no client at all:
    /// the hub maps the file itself and its futex wake reaches the waiter's
    /// mapping. The waiter's slice is 30 s, so only a delivered wake ends it.
    #[test]
    fn the_hub_wakes_a_lane_waiter_in_a_shared_file_with_no_client() {
        use std::os::fd::AsFd;
        use std::os::unix::fs::FileExt;
        use sv_transport::control::{Control, Geometry, ProtocolIds, RingStatus};
        use sv_transport::park::{Deadline, Futex, ParkOutcome, Parker, WaitSite};
        use sv_transport::ring::{Lane, RingCtrl, RingId};
        let d = tmpdir("direct");
        let file = d.join("arena");
        let g = Geometry::for_wayland();
        let (off, len) = (8192u64, g.control_size);
        let f = std::fs::OpenOptions::new().read(true).write(true).create(true).truncate(true).open(&file).unwrap();
        f.set_len(off + len).unwrap();
        let h = wc_handle::ivshmem::Header {
            run_id: 0x77,
            generation: 1,
            region_len: off + len,
            arena_id: 0,
            outbound: (0, 0),
            rings: (off, len),
        };
        f.write_all_at(&h.encode(), 0).unwrap();
        let region = Arc::new(sv_transport::region::SharedRegion::map_fd(f.as_fd(), off, len as usize).unwrap());
        let control = Arc::new(Control::create(region, g, ProtocolIds::new(1, 0xabcd), 1).unwrap());

        let qemu = fake_qemu(&d.join("qemu.sock"));
        let hub = run("t", d.join("qemu.sock"), bind_private(&d.join("hub.sock")).unwrap()).unwrap();
        hub.set_direct_rings(&file);
        let mut guest = qemu.recv_timeout(Duration::from_secs(5)).unwrap();
        // Past the fresh upstream's rescan, which rings every lane.
        std::thread::sleep(Duration::from_millis(200));
        let c = Arc::clone(&control);
        let waiter = std::thread::spawn(move || {
            let ctrl = RingCtrl::open(&c, 3).unwrap();
            let bell = ctrl.bell(Lane::Command).unwrap();
            let start = bell.wake.load(Ordering::Acquire);
            let ready = || bell.wake.load(Ordering::Acquire) != start;
            let status = || RingStatus::Idle;
            let site = WaitSite::new(RingId(3), &ready, &status).with_bell(bell);
            let p = Futex { spins: 0, yields: 0, slice: Duration::from_secs(30), first_slice: Duration::from_secs(30) };
            let t0 = Instant::now();
            (p.park(&site, Deadline::after(Duration::from_secs(30))), t0.elapsed())
        });
        // Parked by now.
        let ctrl = RingCtrl::open(&control, 3).unwrap();
        let t = Instant::now();
        while ctrl.bell(Lane::Command).unwrap().parked.load(Ordering::Acquire) == 0 && t.elapsed() < Duration::from_secs(5) {
            std::thread::sleep(Duration::from_millis(5));
        }
        guest.write_all(&[sv_transport::park::lane_token(3, Lane::Command)]).unwrap();
        let (outcome, took) = waiter.join().unwrap();
        assert_eq!(outcome, ParkOutcome::Woken);
        assert!(took < Duration::from_secs(5), "the waiter waited {took:?}: the hub's wake never reached it");
        assert_eq!(hub.client_count(), 0, "a client could have done the waking");
    }

    #[test]
    fn upstream_spellings() {
        let p = Upstream::parse("dev:org.sirati.wcbell.0");
        assert!(matches!(&p, Upstream::GuestPort(n) if n == "org.sirati.wcbell.0"));
        assert_eq!(p.name(), "org.sirati.wcbell.0");
        assert_eq!(p.to_string(), "/dev/virtio-ports/org.sirati.wcbell.0");
        let s = Upstream::parse("/run/vm/wcbell.sock");
        assert!(matches!(&s, Upstream::Socket(p) if p == std::path::Path::new("/run/vm/wcbell.sock")));
        assert_eq!(s.name(), "wcbell");
    }

    /// A guest port is a file, not a socket: the hub serves it the same way,
    /// rescan token first on every (re)open, bytes both ways.
    #[test]
    fn a_link_that_is_not_a_socket_is_served_the_same() {
        let d = tmpdir("link");
        let hub = Hub::new("t");
        let l = bind_private(&d.join("hub.sock")).unwrap();
        {
            let hub = Arc::clone(&hub);
            std::thread::spawn(move || {
                for c in l.incoming().flatten() {
                    let _ = hub.add_client(c);
                }
            });
        }
        let mut a = UnixStream::connect(d.join("hub.sock")).unwrap();
        let t = Instant::now();
        while hub.client_count() < 1 && t.elapsed() < Duration::from_secs(5) {
            std::thread::sleep(Duration::from_millis(10));
        }
        for round in 0..2u8 {
            let (r, mut guest_w) = std::io::pipe().unwrap();
            let (mut guest_r, w) = std::io::pipe().unwrap();
            let closed = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let link = {
                let (hub, closed) = (Arc::clone(&hub), Arc::clone(&closed));
                std::thread::spawn(move || {
                    hub.serve_link(Box::new(r), Box::new(w), move || closed.store(true, Ordering::Relaxed))
                })
            };
            guest_w.write_all(&[10 + round]).unwrap();
            let mut got = Vec::new();
            let t = Instant::now();
            while !got.ends_with(&[10 + round]) && t.elapsed() < Duration::from_secs(5) {
                got.extend(read_raw(&mut a, 1, Duration::from_secs(5)));
            }
            assert_eq!(got, vec![sv_transport::park::RESCAN_TOKEN, 10 + round], "round {round}");
            a.write_all(&[20 + round]).unwrap();
            // §105: a fresh link tells the guest to rescan first, then the byte.
            let mut b = [0u8; 2];
            guest_r.read_exact(&mut b).unwrap();
            assert_eq!(b, [sv_transport::park::RESCAN_TOKEN, 20 + round], "round {round}");
            drop(guest_w); // the guest closed the port
            let _ = link.join().unwrap();
            assert!(closed.load(Ordering::Relaxed), "round {round}: close ran");
        }
        assert_eq!(hub.stats.upstream_connects.load(Ordering::Relaxed), 2);
        let _ = std::fs::remove_dir_all(&d);
    }
}
