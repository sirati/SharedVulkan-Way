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
}

/// One port: an upstream (QEMU's chardev) and the clients that share it.
pub struct Hub {
    name: String,
    clients: Mutex<Vec<Client>>,
    up: Mutex<Option<SyncSender<Vec<u8>>>>,
    next: AtomicU64,
    pub stats: Stats,
}

impl Hub {
    pub fn new(name: &str) -> Arc<Hub> {
        Arc::new(Hub {
            name: name.to_string(),
            clients: Mutex::new(Vec::new()),
            up: Mutex::new(None),
            next: AtomicU64::new(1),
            stats: Stats::default(),
        })
    }

    /// Bytes from the guest, to every client. Never blocks.
    fn fan_out(&self, b: &[u8]) {
        self.stats.down.fetch_add(b.len() as u64, Ordering::Relaxed);
        let Ok(mut c) = self.clients.lock() else { return };
        c.retain(|cl| match cl.tx.try_send(b.to_vec()) {
            Ok(()) => true,
            Err(TrySendError::Full(_)) => {
                self.stats.down_dropped.fetch_add(b.len() as u64, Ordering::Relaxed);
                true
            }
            Err(TrySendError::Disconnected(_)) => false,
        });
    }

    /// Bytes from a client, to the guest. Never blocks; with no upstream
    /// (the VM is down) they are dropped.
    fn up(&self, b: &[u8]) {
        let sent = self
            .up
            .lock()
            .ok()
            .and_then(|u| u.as_ref().map(|tx| tx.try_send(b.to_vec()).is_ok()))
            .unwrap_or(false);
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
        std::thread::Builder::new().name(format!("bell-{}-w{id}", self.name)).spawn(move || {
            while let Ok(b) = rx.recv() {
                if w.write_all(&b).is_err() {
                    break;
                }
            }
            let _ = w.shutdown(std::net::Shutdown::Both);
        })?;
        self.clients.lock().map_err(|_| std::io::Error::other("poisoned"))?.push(Client { id, tx });
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
        self.serve_link_fresh(Box::new(up), Box::new(w), move || {
            let _ = closer.shutdown(std::net::Shutdown::Both);
        }, fresh)
    }

    /// [`Hub::serve_upstream`] over any reader and writer: a chardev socket's
    /// two halves, or a guest port's (role (b)). `close` runs once the reader
    /// has ended, so a writer stuck on a dead upstream gives up.
    pub fn serve_link(
        self: &Arc<Self>,
        mut r: Box<dyn Read + Send>,
        mut w: Box<dyn Write + Send>,
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
        let writer = std::thread::Builder::new().name(format!("bell-{}-up", self.name)).spawn(move || {
            while let Ok(b) = rx.recv() {
                if w.write_all(&b).is_err() {
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
