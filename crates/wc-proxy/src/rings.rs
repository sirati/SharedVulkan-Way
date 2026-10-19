//! The session's Wayland connections as rings in the ivshmem region, and the
//! doorbell that wakes a pump parked on one (docs/waycompose-design.md §89).
//!
//! # Why this exists
//!
//! On the launcher's deployment every Wayland byte from the guest crossed
//! `ssh.exe`, which holds a small message from the guest until the host next
//! sends something: §88 measured the hold, §89 measured an input event's
//! crossing at p50 11.4, p99 30.8, max 46.3 ms. §36 had already built the
//! design's own answer — a connection is a ring slot of a shared_vulkan control
//! region — and lacked only a region both machines map without root and a wake
//! the hypervisor carries. The ivshmem arena is the first (the compositor keeps
//! its tail for the rings, and the header says where); a second virtio-serial
//! port, `org.sirati.wcbell.0`, is the second, exactly as §87's port is for the
//! GPU sessions.
//!
//! # What crosses, and what does not (§14.7, docs/security-model.md R31)
//!
//! Bytes into a mapping, and one byte per wake into a socket QEMU made. The
//! ring's write takes `&[u8]`; the doorbell's takes a `u8`. Neither has a
//! parameter that could carry a descriptor. Nothing here listens: the port's
//! socket is QEMU's, `0660 root:wc-arena`, and this process *connects* to it,
//! as `sv-hostd` connects to §87's.
//!
//! # How a pump waits on a ring
//!
//! A pump waits in `poll`, so the doorbell has to become a descriptor. One
//! thread per proxy holds the port (it has one opener on each side) and turns
//! each byte that names a lane this process reads into a write to that
//! connection's `eventfd`, which is in the pump's `pollfd` set. The pump sets
//! the lane's `parked` word before it sleeps and looks at the ring once more
//! after setting it ([`RingWake::park`]), which is `sv_transport::park::Bell`'s
//! handshake with a `poll` where the futex was: a compositor write before the
//! `parked` store is seen by the look, one after it sees `parked` and rings.

use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use sv_transport::control::{Control, Geometry, ProtocolIds};
use sv_transport::ring::Lane;
use sv_transport::vdoorbell::{BellTarget, Doorman};
use sv_transport::wlbytes::ByteChannel;

/// Who is waiting on which slot. The doorbell thread's half.
#[derive(Default)]
pub struct BellHub {
    waiting: Mutex<Vec<(u32, Arc<OwnedFd>)>>,
    /// Bytes that named a lane this process reads, and bytes that named
    /// nothing it is waiting on (a connection that has ended, or the
    /// compositor's lane, which is not this side's to read). Both are
    /// diagnostics: a stray byte costs a syscall and nothing else.
    delivered: AtomicU64,
    stray: AtomicU64,
}

impl BellHub {
    fn register(&self, slot: u32, fd: Arc<OwnedFd>) {
        if let Ok(mut w) = self.waiting.lock() {
            w.retain(|(s, _)| *s != slot);
            w.push((slot, fd));
        }
    }

    fn unregister(&self, slot: u32) {
        if let Ok(mut w) = self.waiting.lock() {
            w.retain(|(s, _)| *s != slot);
        }
    }

    /// `(delivered, stray)`.
    pub fn stats(&self) -> (u64, u64) {
        (self.delivered.load(Ordering::Relaxed), self.stray.load(Ordering::Relaxed))
    }
}

impl BellTarget for BellHub {
    fn ring_token(&self, token: u8, woken: &AtomicU64) -> bool {
        let (slot, lane) = sv_transport::park::lane_from_token(token);
        // The compositor writes the reply lane; that is the one this side
        // reads and parks on. A command-lane byte would be this side's own
        // token come back, which the compositor never sends.
        let fd = if lane == Lane::Reply {
            self.waiting
                .lock()
                .ok()
                .and_then(|w| w.iter().find(|(s, _)| *s == slot).map(|(_, f)| Arc::clone(f)))
        } else {
            None
        };
        let Some(fd) = fd else {
            self.stray.fetch_add(1, Ordering::Relaxed);
            return false;
        };
        woken.fetch_add(1, Ordering::Relaxed);
        self.delivered.fetch_add(1, Ordering::Relaxed);
        let one: u64 = 1;
        // SAFETY: a live eventfd kept alive by the `Arc`, and an 8-byte buffer,
        // which is what an eventfd write takes. A full counter (EAGAIN) is a
        // wake already pending, which is all this is for.
        unsafe {
            libc::write(fd.as_raw_fd(), (&one as *const u64).cast(), 8);
        }
        true
    }
}

/// The doorbell, held for the life of the proxy.
pub struct Doorbell {
    pub hub: Arc<BellHub>,
    _doorman: Option<Doorman>,
}

impl Doorbell {
    /// Connect to QEMU's end of the rings' port and start answering it. The
    /// writer half is installed as this process's cross-VM wake, which is
    /// what `Bell::ring` pokes when the compositor is parked on a lane this
    /// process writes.
    pub fn connect(path: &Path) -> io::Result<Doorbell> {
        // §99: reconnecting, so a restarted doorbell hub costs this proxy
        // only the wakes in the gap. The first connect must succeed: a path
        // nothing listens on is a configuration error, said at once.
        let p = path.to_string_lossy();
        let hub = Arc::new(BellHub::default());
        let (doorman, connected) = sv_transport::vdoorbell::attach_host_reconnecting(&p, Arc::clone(&hub))?;
        if !connected {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("{p}: nothing listens (the VM has no Wayland rings' port, no doorbell hub runs, or this user is not in its group)"),
            ));
        }
        Ok(Doorbell { hub, _doorman: Some(doorman) })
    }

    /// §103.15: [`Self::connect`] for the guest-RAM rings, whose session
    /// services (control, sound, the feeds' bridges) wait on their lanes'
    /// own words rather than an eventfd: a byte also rings the lane it names
    /// in `control`, which is what [`ByteChannel::wait_readable`] sleeps on.
    pub fn connect_with(path: &Path, control: &Arc<Control>) -> io::Result<Doorbell> {
        let p = path.to_string_lossy();
        let hub = Arc::new(BellHub::default());
        let target = Arc::new(HubAndControl { hub: Arc::clone(&hub), control: Arc::clone(control) });
        let (doorman, connected) = sv_transport::vdoorbell::attach_host_reconnecting(&p, target)?;
        if !connected {
            return Err(io::Error::new(io::ErrorKind::NotFound, format!("{p}: nothing listens (no doorbell hub runs for this VM)")));
        }
        Ok(Doorbell { hub, _doorman: Some(doorman) })
    }

    /// §103.14: no doorbell port (the macOS guest under QEMU has none yet):
    /// every waiting pump is woken on a fixed period and looks at its ring.
    /// The deadline is the period; the cost is one `write` per waiting
    /// connection per period. The compositor's side polls on the same period.
    pub fn polling(every: std::time::Duration) -> Doorbell {
        let hub = Arc::new(BellHub::default());
        let h = Arc::clone(&hub);
        std::thread::Builder::new()
            .name("ring-poll".into())
            .spawn(move || {
                loop {
                    std::thread::sleep(every);
                    h.poke_all();
                }
            })
            .ok();
        Doorbell { hub, _doorman: None }
    }
}

/// A doorbell byte, for both kinds of waiter (§103.15).
struct HubAndControl {
    hub: Arc<BellHub>,
    control: Arc<Control>,
}

impl BellTarget for HubAndControl {
    fn ring_token(&self, token: u8, woken: &AtomicU64) -> bool {
        let a = self.hub.ring_token(token, woken);
        let b = self.control.ring_token(token, woken);
        a || b
    }
}

/// How long a session service's thread sleeps on its lane before looking
/// again: long with a doorbell, which ends the sleep; a millisecond without
/// one, which is the polling the doorbell replaces (§103.14).
static SERVICE_SLICE_US: AtomicU64 = AtomicU64::new(1000);

pub fn set_service_slice(d: std::time::Duration) {
    SERVICE_SLICE_US.store(d.as_micros() as u64, Ordering::Relaxed);
}

pub fn service_slice() -> std::time::Duration {
    std::time::Duration::from_micros(SERVICE_SLICE_US.load(Ordering::Relaxed))
}

impl BellHub {
    /// Wake every pump parked on a ring (the polling doorbell's tick).
    fn poke_all(&self) {
        let fds: Vec<Arc<OwnedFd>> = match self.waiting.lock() {
            Ok(w) => w.iter().map(|(_, f)| Arc::clone(f)).collect(),
            Err(_) => return,
        };
        for fd in fds {
            let one: u64 = 1;
            // SAFETY: an eventfd this hub holds, and an 8-byte value.
            unsafe {
                libc::write(fd.as_raw_fd(), (&raw const one).cast(), 8);
            }
        }
    }
}

/// §103.14: a byte channel of the ring region, presented as a local socket,
/// for code written against a socket (the icon and clipboard feeds). One
/// thread copies both ways and polls when idle; the feeds are small and rare.
/// Returns this process's end.
pub fn bridge(control: Arc<Control>, owner: u64) -> io::Result<std::os::unix::net::UnixStream> {
    use std::io::{Read, Write};
    let mut ch = ByteChannel::claim(&control, owner).map_err(|e| io::Error::other(format!("ring claim: {e}")))?;
    let (mine, mut theirs) = std::os::unix::net::UnixStream::pair()?;
    theirs.set_nonblocking(true)?;
    std::thread::Builder::new().name(format!("ring-bridge-{}", ch.slot())).spawn(move || {
        let _keep = control;
        let mut buf = vec![0u8; 64 * 1024];
        let mut to_ring: Vec<u8> = Vec::new();
        let mut to_sock: Vec<u8> = Vec::new();
        loop {
            let mut idle = true;
            if to_ring.is_empty() {
                match theirs.read(&mut buf) {
                    Ok(0) => {
                        let _ = ch.write_eof();
                        return;
                    }
                    Ok(n) => {
                        to_ring.extend_from_slice(&buf[..n]);
                        idle = false;
                    }
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
                    Err(_) => return,
                }
            }
            if !to_ring.is_empty() {
                match ch.write(&to_ring) {
                    Ok(n) if n > 0 => {
                        to_ring.drain(..n);
                        idle = false;
                    }
                    Ok(_) => {}
                    Err(_) => return,
                }
            }
            if to_sock.is_empty() {
                match ch.read(&mut buf) {
                    Ok(sv_transport::wlbytes::ReadOutcome::Bytes(n)) => {
                        to_sock.extend_from_slice(&buf[..n]);
                        idle = false;
                    }
                    Ok(sv_transport::wlbytes::ReadOutcome::Eof) => return,
                    Ok(_) => {}
                    Err(_) => return,
                }
            }
            if !to_sock.is_empty() {
                match theirs.write(&to_sock) {
                    Ok(n) => {
                        to_sock.drain(..n);
                        idle = false;
                    }
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
                    Err(_) => return,
                }
            }
            if idle {
                // The ring's side wakes this at once; the socket's is looked
                // at once a slice (the feeds are small and rare).
                let _ = ch.wait_readable(service_slice().min(std::time::Duration::from_millis(20)));
            }
        }
    })?;
    Ok(mine)
}

/// §103.14: attach the ring region a guest made in its own RAM, located by
/// `wc-arena-host` into `plan`. The compositor makes the control region only
/// after the host has located the stamps, so this waits for it.
pub fn attach_plan(plan: &Path, wait: std::time::Duration) -> io::Result<Arc<Control>> {
    let p = crate::guestram::Plan::read(plan)?;
    let region = crate::guestram::StitchedRegion::attach(&p)?;
    let (base, len) = {
        let s = region.as_slice();
        (s.as_ptr().cast_mut(), s.len())
    };
    // The stitched mapping lives as long as the process: the rings are the
    // session's, and every client thread holds the control region.
    std::mem::forget(region);
    // §103.15: page 0 is the boot page; the control region starts at page 1.
    let page = wc_handle::arena::REGION_PAGE;
    if len <= page {
        return Err(io::Error::other(format!("{}: a ring window of {len} bytes has no control region", plan.display())));
    }
    // SAFETY: a live mapping of `len` bytes, never unmapped (above); the
    // control region is its tail past the boot page.
    let shared = Arc::new(unsafe { sv_transport::region::SharedRegion::from_raw(base.add(page), len - page) });
    let ids = ProtocolIds::new(1, sv_proto::PROTOCOL_HASH);
    let t0 = std::time::Instant::now();
    loop {
        match Control::attach(Arc::clone(&shared), ids) {
            Ok(c) => return Ok(Arc::new(c)),
            Err(e) if t0.elapsed() > wait => {
                return Err(io::Error::other(format!("{}: no control region after {wait:?}: {e}", plan.display())));
            }
            Err(_) => std::thread::sleep(std::time::Duration::from_millis(50)),
        }
    }
}

/// One connection's wake: an `eventfd` the hub writes when the compositor
/// rings the lane this connection reads.
pub struct RingWake {
    fd: Arc<OwnedFd>,
    slot: u32,
    hub: Arc<BellHub>,
}

impl RingWake {
    pub fn new(hub: &Arc<BellHub>, slot: u32) -> io::Result<RingWake> {
        // SAFETY: no pointers; the result is checked.
        let raw = unsafe { libc::eventfd(0, libc::EFD_NONBLOCK | libc::EFD_CLOEXEC) };
        if raw < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: a descriptor `eventfd` just returned, owned by nothing else.
        let fd = Arc::new(unsafe { OwnedFd::from_raw_fd(raw) });
        hub.register(slot, Arc::clone(&fd));
        Ok(RingWake { fd, slot, hub: Arc::clone(hub) })
    }

    pub fn fd(&self) -> RawFd {
        self.fd.as_raw_fd()
    }

    /// Set `parked` on the lane `ch` reads, then look at it once more.
    /// Returns whether there is already something to read, in which case the
    /// caller must not sleep. Always followed by [`Self::unpark`].
    pub fn park(&self, ch: &ByteChannel) -> bool {
        if let Ok(bell) = ch.rx_bell() {
            bell.parked.fetch_add(1, Ordering::SeqCst);
        }
        ch.readable().unwrap_or(true)
    }

    /// Undo [`Self::park`] and empty the eventfd, so the next sleep is not
    /// ended by a wake this one already had.
    pub fn unpark(&self, ch: &ByteChannel) {
        if let Ok(bell) = ch.rx_bell() {
            bell.parked.fetch_sub(1, Ordering::SeqCst);
        }
        let mut v: u64 = 0;
        // SAFETY: a live nonblocking eventfd and an 8-byte buffer. EAGAIN, an
        // empty counter, is the common case and needs nothing.
        unsafe {
            libc::read(self.fd.as_raw_fd(), (&mut v as *mut u64).cast(), 8);
        }
    }
}

impl Drop for RingWake {
    fn drop(&mut self) {
        self.hub.unregister(self.slot);
    }
}

/// Attach to the rings in the ivshmem region at `path` (§89): the header's
/// `rings`, checked against the run id the compositor printed, mapped from the
/// same file the arena is.
///
/// `Ok(None)` is a header with no rings — a compositor that was not asked for
/// them or could not open its port — which the caller reports and answers by
/// using the socket. Every other failure is an error with the reason in it.
pub fn attach_ivshmem(path: &Path, run: u64) -> io::Result<Option<Arc<Control>>> {
    let h = crate::guestram::ivshmem_header(path, run)?;
    let (off, len) = h.rings;
    if len == 0 {
        return Ok(None);
    }
    let want = Geometry::for_wayland().control_size;
    if len < want {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{}: rings of {len} bytes, and this proxy's are {want}", path.display()),
        ));
    }
    let f = std::fs::OpenOptions::new().read(true).write(true).open(path)?;
    let region = sv_transport::region::SharedRegion::map_fd(
        std::os::fd::AsFd::as_fd(&f),
        off,
        len as usize,
    )
    .map_err(|e| io::Error::other(format!("{}: mapping the rings: {e}", path.display())))?;
    // The same ids the compositor created the region with. R13: two builds
    // that disagree fail here, by name, instead of misreading a ring.
    let ids = ProtocolIds::new(1, sv_proto::PROTOCOL_HASH);
    let control = Control::attach(Arc::new(region), ids)
        .map_err(|e| io::Error::other(format!("{}: the rings' control region: {e}", path.display())))?;
    Ok(Some(Arc::new(control)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use sv_transport::region::SharedRegion;
    use sv_transport::wlbytes::{Endpoint, ReadOutcome};

    fn region() -> (Control, Control) {
        let g = Geometry::small();
        let r = Arc::new(SharedRegion::heap(g.control_size as usize).unwrap());
        let ids = ProtocolIds::new(1, 2);
        let owner = Control::create(Arc::clone(&r), g, ids, 1).unwrap();
        (owner, Control::attach(r, ids).unwrap())
    }

    fn readable(fd: RawFd) -> bool {
        let mut p = libc::pollfd { fd, events: libc::POLLIN, revents: 0 };
        // SAFETY: one live pollfd.
        unsafe { libc::poll(&mut p, 1, 0) == 1 }
    }

    /// The hub turns a byte naming a connection's reply lane into that
    /// connection's eventfd becoming readable, and nobody else's.
    #[test]
    fn a_byte_naming_a_reply_lane_wakes_that_connection_only() {
        let hub = Arc::new(BellHub::default());
        let a = RingWake::new(&hub, 3).unwrap();
        let b = RingWake::new(&hub, 4).unwrap();
        let woken = AtomicU64::new(0);
        assert!(hub.ring_token(sv_transport::park::lane_token(3, Lane::Reply), &woken));
        assert!(readable(a.fd()) && !readable(b.fd()));
        // The command lane is the compositor's to read: not ours to wake on.
        assert!(!hub.ring_token(sv_transport::park::lane_token(4, Lane::Command), &woken));
        assert!(!readable(b.fd()));
        // A connection that has gone is a stray byte, not an error.
        drop(b);
        assert!(!hub.ring_token(sv_transport::park::lane_token(4, Lane::Reply), &woken));
        assert_eq!(hub.stats(), (1, 2));
    }

    /// The park handshake with a writer on the other end: a write before the
    /// park is seen by its last look; a write after it finds `parked` set,
    /// which is what makes `Bell::ring` poke the port.
    #[test]
    fn park_sees_what_was_written_and_tells_the_writer_it_is_parked() {
        let (owner, peer) = region();
        let host = ByteChannel::claim(&peer, 1).unwrap();
        let mut guest = ByteChannel::open(&owner, host.slot(), Endpoint::Peer).unwrap();
        let hub = Arc::new(BellHub::default());
        let w = RingWake::new(&hub, host.slot()).unwrap();

        assert!(!w.park(&host), "nothing written yet");
        // The writer's view of the reader's lane: parked.
        let ctrl = sv_transport::ring::RingCtrl::open(&owner, host.slot()).unwrap();
        assert_eq!(ctrl.bell(Lane::Reply).unwrap().parked.load(Ordering::SeqCst), 1);
        w.unpark(&host);
        assert_eq!(ctrl.bell(Lane::Reply).unwrap().parked.load(Ordering::SeqCst), 0);

        guest.write(b"event").unwrap();
        assert!(w.park(&host), "a write before the park must be seen by its last look");
        w.unpark(&host);
        let mut host = host;
        let mut buf = [0u8; 16];
        assert_eq!(host.read(&mut buf).unwrap(), ReadOutcome::Bytes(5));
    }

    /// A header without rings is `None`, not an error: the compositor said it
    /// has none, and the socket is the answer.
    #[test]
    fn no_rings_in_the_header_is_none() {
        use wc_handle::ivshmem::Header;
        let dir = std::env::temp_dir().join(format!("wc-rings-test-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("arena");
        let region = 4096 * 2048u64;
        let h = Header {
            run_id: 7,
            generation: 1,
            region_len: region,
            arena_id: 0,
            outbound: (0, 0),
            rings: (0, 0),
        };
        let mut bytes = vec![0u8; region as usize];
        // SAFETY: a live buffer of a whole region.
        unsafe { wc_handle::ivshmem::write_into(bytes.as_mut_ptr(), &h) };
        std::fs::write(&path, &bytes).unwrap();
        assert!(attach_ivshmem(&path, 7).unwrap().is_none());
        assert!(attach_ivshmem(&path, 8).is_err(), "another run's header is refused");

        // With rings in the header but no control region in them (the
        // compositor kept the tail and could not create it), the attach fails
        // by name, and the caller falls back.
        let rings = Geometry::for_wayland().control_size;
        let h = Header { rings: (region - rings, rings), ..h };
        // SAFETY: as above.
        unsafe { wc_handle::ivshmem::write_into(bytes.as_mut_ptr(), &h) };
        std::fs::write(&path, &bytes).unwrap();
        let e = attach_ivshmem(&path, 7).unwrap_err().to_string();
        assert!(e.contains("control region"), "{e}");

        // And with one, it attaches and a claim works across the two mappings.
        let owner_region = Arc::new(SharedRegion::heap(rings as usize).unwrap());
        let owner = Control::create(
            Arc::clone(&owner_region),
            Geometry::for_wayland(),
            ProtocolIds::new(1, sv_proto::PROTOCOL_HASH),
            1,
        )
        .unwrap();
        let mut tail = vec![0u8; rings as usize];
        owner_region.read_bytes(0, &mut tail).unwrap();
        bytes[(region - rings) as usize..].copy_from_slice(&tail);
        std::fs::write(&path, &bytes).unwrap();
        let c = attach_ivshmem(&path, 7).unwrap().expect("rings");
        assert_eq!(c.layout().geometry.ring_slot_count, Geometry::for_wayland().ring_slot_count);
        drop(owner);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
