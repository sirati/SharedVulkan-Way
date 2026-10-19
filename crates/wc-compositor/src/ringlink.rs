//! The compositor's half of §36: `accept()` is a poll of the ring directory.
//!
//! waycompose-design.md §36.2's table says a Wayland connection is a ring slot,
//! `connect()` is `Control::claim_ring` and `accept()` is polling the directory
//! for a `Live` ring of that kind. `wc-proxy --upstream-ring` does the claiming.
//! This is the accepting, and it is the last piece of the data path.
//!
//! # The bridge, and exactly where the socket that remains lives
//!
//! smithay's `DisplayHandle::insert_client` takes a `wc_transport::Stream`,
//! which is a `UnixStream` on Linux and a `uds_windows::UnixStream` on the
//! guest. It is a concrete type rather than a trait — `wc-transport`'s header
//! explains that this is deliberate, so the vendored `wayland-backend` can say
//! `wc_transport::Stream` where it used to say `std::os::unix::net::UnixStream`
//! — and a ring is not one and cannot become one. `wepoll` needs a real socket
//! handle for the `AsSocket` impls, so even an enum would not satisfy it.
//!
//! So each accepted ring gets a **`Stream::pair()`**: one end goes to
//! `insert_client` and looks to smithay exactly like a client socket, and this
//! module pumps bytes between the other end and the [`ByteChannel`].
//!
//! **That socket is worth being exact about, because "no socket in the data
//! path" was the goal.** It is a socketpair *inside the guest*, between two
//! threads of one process. What crosses the VM boundary is the ring, and only
//! the ring: no TCP, no ssh, no `probe-relay.exe`, and nothing that leaves the
//! guest's kernel. The remaining hop is a memcpy through a loopback buffer, next
//! to a hop that was a WiFi round trip. It is not zero and this module does not
//! pretend it is; §36.8 carries it as a known cost with the one way to remove it
//! (teaching the vendored `wayland-backend` to take a `Read + Write` client
//! rather than a socket, which is a change to vendored code and a `wepoll`
//! problem, and is not worth doing before a client has ever driven a window
//! through a ring).
//!
//! # Why the compositor owns the region
//!
//! In the cross-VM topology the guest allocates and the host locates — the guest
//! owns its RAM and `sv-hostd` finds it (§3, D20). The compositor is the guest
//! side, so it calls `Control::create` and the proxy calls `Control::attach`,
//! which is the same direction the Vulkan half already runs in and means the two
//! halves of one guest are not disagreeing about who owns what.

use std::io::{Read, Write};
use std::sync::Arc;
use sv_transport::control::{Control, Geometry, ProtocolIds};
use sv_transport::wlbytes::{ByteChannel, Endpoint, ReadOutcome};

/// How much of one connection to move per turn.
///
/// The compositor's Wayland thread runs this between `dispatch_clients` and
/// `flush_clients`, on the 5 ms tick, so a turn that moved one packet would cap
/// a connection at one packet per 5 ms. 64 KiB is `wc-proxy`'s `READ_CHUNK` and
/// the same reasoning applies: drain what is there.
const PUMP_CHUNK: usize = 64 * 1024;

/// One accepted connection: the ring, and the socket end smithay does not have.
struct Link {
    /// Which directory slot this is: the number both ends use for it, and
    /// since §89 what `RingLink::reap` checks before giving a slot back.
    slot: u32,
    /// §107: the session the proxy that claimed it said it is (0: none).
    session: u8,
    /// The owner id it was claimed with: which client connection it is, or
    /// which one it is a feed for (§107.10).
    owner: u64,
    accepted_at: std::time::Instant,
    channel: ByteChannel,
    /// Our end of the pair. smithay holds the other.
    local: wc_transport::Stream,
    /// Bytes read from the ring that the local socket would not take yet, and
    /// vice versa. A short write on either side is ordinary and the remainder
    /// must not be dropped — a Wayland stream that loses eight bytes is a
    /// protocol error attributed to whoever sent them.
    to_local: Vec<u8>,
    to_ring: Vec<u8>,
    /// The ring reported end of stream. The local socket is shut down once
    /// `to_local` has drained, so smithay sees the disconnect *after* the bytes
    /// that preceded it, which is the ordering §36.4 built EOF as a packet for.
    ring_eof: bool,
    dead: bool,
}

/// The compositor's ring listener.
pub struct RingLink {
    control: Arc<Control>,
    links: Vec<Link>,
    /// Slots already accepted, so the directory poll does not accept twice.
    seen: Vec<u32>,
}

impl RingLink {
    /// Create the control region and start listening on its ring directory.
    ///
    /// The file is created and sized here because the compositor owns it — see
    /// the module header. An existing file is truncated: a stale region from a
    /// previous run has a stale generation and stale ring states, and attaching
    /// to it would be attaching to a session whose peers are gone.
    pub fn create(name: &str) -> Result<RingLink, String> {
        let geometry = Geometry::for_arena(256 << 20);
        // The region is a **file** on Unix and a **named section** on Windows,
        // and that is the only platform difference in this module. It is the
        // same split the arena already lives with: §1a made the arena a *named*
        // object on both sides precisely so that nothing but text has to cross,
        // and `winshm::Section` is the Windows half of that. Reused here rather
        // than written again — a second mapper is a second place for the
        // geometry to be wrong.
        #[cfg(unix)]
        let region = {
            let f = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(true)
                .open(name)
                .map_err(|e| format!("{name}: {e}"))?;
            f.set_len(geometry.control_size).map_err(|e| format!("{name}: {e}"))?;
            Arc::new(
                sv_transport::region::SharedRegion::map_fd(
                    std::os::fd::AsFd::as_fd(&f),
                    0,
                    geometry.control_size as usize,
                )
                .map_err(|e| e.to_string())?,
            )
        };
        #[cfg(windows)]
        let region = {
            let section = sv_transport::winshm::Section::create(name, geometry.control_size)
                .map_err(|e| format!("{name}: {e}"))?;
            Arc::new(
                sv_transport::region::SharedRegion::map_section(section)
                    .map_err(|e| e.to_string())?,
            )
        };
        // The same ids the Vulkan half uses. R13: two builds that disagree must
        // fail to connect rather than desynchronise.
        let ids = ProtocolIds::new(1, sv_proto::PROTOCOL_HASH);
        let control = Control::create(region, geometry, ids, 1).map_err(|e| e.to_string())?;
        control.stamp_epoch().map_err(|e| e.to_string())?;
        Ok(RingLink { control: Arc::new(control), links: Vec::new(), seen: Vec::new() })
    }

    /// Publish the control region out of **guest RAM**, over the cold path, and
    /// listen on its directory. The cross-VM form of [`RingLink::create`].
    ///
    /// This is `win_guest`'s bring-up with the arena left out, and it is the same
    /// sequence for the same reasons (§4.3, §5.1, and the ordering note below):
    ///
    /// 1. commit a window of guest RAM and **stamp every page**, so the host can
    ///    find it by content — the guest cannot enumerate its own frames, so the
    ///    locator is `Scan`;
    /// 2. publish an `ExtentDesc` in a `Hello` down the virtio-serial port;
    /// 3. wait for `Accept`, which is the host saying it has located the pages;
    /// 4. **only then** `Control::create`.
    ///
    /// Step 4 comes last and that is load-bearing: `Control::create` zeroes the
    /// region, which would erase exactly the stamps step 2 asked the host to
    /// sweep for. `sv-gpu-server` learned this by measurement — "located 8192 of
    /// 12288 pages", the arena's present and every control page missing — and the
    /// comment there is the one to read. Publishing first costs nothing because
    /// `magic` is written last anyway, so an early attacher sees `NotReady` and
    /// retries.
    ///
    /// **The arena is committed and never used**, and that is worth a sentence
    /// because it looks like waste. `commit_and_stamp(control_size, 0)` is
    /// refused — "an extent with no arena" — and rightly: an extent is the unit
    /// the host locates and `sv-arena`'s geometry has no meaning without one.
    /// The Wayland rings carry a byte stream and need no arena; the proxy's pools
    /// are a separate mechanism with their own region. So the smallest legal
    /// arena is committed, stamped along with everything else, and left alone.
    /// The cost is guest RAM the host will map and nobody will write to, and the
    /// alternative — a second shape of extent that means "control only" — would
    /// put a special case in the locator to save 64 MiB in a fixture.
    #[cfg(target_os = "linux")]
    pub fn publish(port: &str) -> Result<RingLink, String> {
        use sv_arena::guestram::{GuestRamProvider, Pages};
        use sv_transport::cold::{ColdMessage, Hello};
        use sv_transport::control::ENDIAN_MARKER;
        use sv_transport::stream::{open_virtio_port, read_message, write_message};

        let geometry = Geometry::for_arena(64 << 20);
        let mut provider = GuestRamProvider::new(Pages::Small);
        let extent = provider
            .commit_and_stamp(geometry.control_size, geometry.arena_size)
            .map_err(|e| format!("cannot commit guest RAM: {e}"))?;
        println!(
            "wc-compositor: {} MiB of guest RAM for the ring region, run 0x{:016x}, {} pages stamped",
            extent.total_len() >> 20,
            extent.desc.run_id,
            extent.desc.control_pages + extent.desc.arena_pages
        );
        let window = provider.take_window().ok_or("the provider kept no window")?;
        let base = unsafe { window.base_ptr() };
        let region = Arc::new(unsafe {
            sv_transport::region::SharedRegion::from_raw(base, geometry.control_size as usize)
        });

        let mut chan: Box<dyn sv_transport::stream::ColdChannel> =
            if port.starts_with("/dev/virtio-ports/") {
                Box::new(
                    open_virtio_port(port.rsplit('/').next().unwrap_or(port))
                        .map_err(|e| format!("{port}: {e}"))?,
                )
            } else {
                Box::new(
                    std::os::unix::net::UnixStream::connect(port)
                        .map_err(|e| format!("{port}: {e}"))?,
                )
            };
        let ids = ProtocolIds::new(1, sv_proto::PROTOCOL_HASH);
        write_message(
            &mut chan,
            &ColdMessage::Hello(Box::new(Hello {
                ids,
                generation: 1,
                extent: extent.desc.clone(),
                geometry,
                endianness: ENDIAN_MARKER,
                pointer_width: 64,
            })),
        )
        .map_err(|e| format!("cannot publish the extent: {e}"))?;
        let mut scratch = Vec::new();
        match read_message(&mut chan, &mut scratch).map_err(|e| e.to_string())? {
            ColdMessage::Accept { generation } => {
                println!("wc-compositor: the host located the ring region, generation {generation}");
            }
            ColdMessage::Refuse { code, reason } => {
                return Err(format!("the host refused {code}: {reason}"));
            }
            other => return Err(format!("unexpected cold-path reply {other:?}")),
        }

        let control = Control::create(region, geometry, ids, 1).map_err(|e| e.to_string())?;
        control.stamp_epoch().map_err(|e| e.to_string())?;
        // The window is the region's now; dropping it here would unmap the pages
        // the host has just located.
        std::mem::forget(window);
        Ok(RingLink { control: Arc::new(control), links: Vec::new(), seen: Vec::new() })
    }

    /// Listen on a control region this process has already created — the
    /// tail of the ivshmem arena (waycompose-design.md §89). The region was
    /// zeroed and published by `Control::create`; what is left is the
    /// directory poll.
    pub fn over(control: Arc<Control>) -> RingLink {
        RingLink { control, links: Vec::new(), seen: Vec::new() }
    }

    /// The doorbell half of the loop's wait (§89): before it sleeps, set
    /// `parked` on the lane every live connection's proxy writes, so a write
    /// rings the port instead of waiting for the next turn. Then look once
    /// more: returns whether a lane already has something, in which case the
    /// caller should not sleep at all. Always followed by [`Self::unpark`].
    pub fn park(&self) -> bool {
        let mut ready = false;
        for link in self.links.iter().filter(|l| !l.dead && !l.ring_eof) {
            if let Ok(bell) = link.channel.rx_bell() {
                bell.parked.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }
            // A lane whose bytes are held back by the local socket (smithay not
            // reading yet) is not a reason to skip the wait: the next turn
            // could not take them either, and counting it would spin.
            ready |= link.to_local.is_empty() && link.channel.readable().unwrap_or(true);
        }
        ready
    }

    /// Undo [`Self::park`] for the same set of connections: nothing between
    /// the two calls changes `links`.
    pub fn unpark(&self) {
        for link in self.links.iter().filter(|l| !l.dead && !l.ring_eof) {
            if let Ok(bell) = link.channel.rx_bell() {
                bell.parked.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
            }
        }
    }

    /// Give back the slots of connections both ends have finished with (§89).
    ///
    /// `Control::kill_ring`'s rule kept a dead slot dead for the life of the
    /// region, and the region now lives as long as the session — twelve hours
    /// of Firefox starts, each taking several connections. So the slot comes
    /// back, by the protocol `Control::recycle_ring` documents: the proxy marks
    /// `DEAD` as its last touch, and this side
    ///
    /// * treats `DEAD` with nothing left in the lane as the end of the stream
    ///   (a proxy that went without writing its end is still gone), so the
    ///   client is disconnected after its last bytes, by `Link::pump`;
    /// * recycles a `DEAD` slot no link of its own is still using, and forgets
    ///   it was seen, because the number will next mean a new connection.
    pub fn reap(&mut self) -> usize {
        use sv_transport::control::{RingKind, RingState};
        for link in self.links.iter_mut().filter(|l| !l.dead && !l.ring_eof) {
            if self.control.ring_state(link.slot).ok() == Some(RingState::Dead)
                && !link.channel.readable().unwrap_or(true)
            {
                link.ring_eof = true;
            }
        }
        let mut recycled = 0;
        for slot in 0..self.control.layout().geometry.ring_slot_count {
            if self.control.ring_state(slot).ok() != Some(RingState::Dead)
                || self.control.ring_kind(slot).ok() != Some(RingKind::Wayland)
                || self.links.iter().any(|l| l.slot == slot)
                || quarantined(slot)
                || !crate::session_svc::may_recycle(self.control.ring_owner(slot).unwrap_or(0))
            {
                continue;
            }
            if self.control.recycle_ring(slot).is_ok() {
                self.seen.retain(|s| *s != slot);
                recycled += 1;
            }
        }
        recycled
    }

    /// New connections since the last call — **this is `accept()`**.
    ///
    /// Returns the stream to hand to `insert_client` for each. A slot is only
    /// ever accepted once; `seen` is not pruned, because a `Dead` slot is never
    /// reused within a generation (`Control::kill_ring`'s own rule) and so a
    /// slot number cannot come back meaning something else.
    pub fn accept(&mut self) -> Vec<(u32, wc_transport::Stream, u8, bool)> {
        let live = match ByteChannel::live_connections(&self.control) {
            Ok(v) => v,
            Err(_) => return Vec::new(),
        };
        let mut out = Vec::new();
        for slot in live {
            if self.seen.contains(&slot) {
                continue;
            }
            // §103.14: a session service's channel (control, sound) is not a
            // client; the launcher opens it.
            let owner = self.control.ring_owner(slot).unwrap_or(0);
            if sv_transport::wlbytes::is_service(owner) {
                continue;
            }
            // §107: a claim of a session this compositor does not serve (one
            // that has ended, or never booted) is not a client: closed, so its
            // proxy sees the end, and its slot comes back after a quarantine.
            let session = sv_transport::wlbytes::session_of(owner);
            if !crate::session_svc::accepts(session) {
                println!("wc-compositor: ring slot {slot} claimed for session {session}, which is not being served; closed");
                let _ = self.control.kill_ring(slot);
                quarantine(slot);
                continue;
            }
            // §107: one session's connections may not take the slots every
            // other installation needs; a proxy past its share is refused a
            // connection, never given another session's.
            if session != 0 && self.links.iter().filter(|l| l.session == session && !l.dead).count() >= SLOTS_PER_SESSION {
                println!(
                    "wc-compositor: ring slot {slot}: session {session} already has {SLOTS_PER_SESSION} connections; refused"
                );
                let _ = self.control.kill_ring(slot);
                quarantine(slot);
                continue;
            }
            let Ok(channel) = ByteChannel::open(&self.control, slot, Endpoint::Peer) else {
                continue;
            };
            let Ok((local, theirs)) = wc_transport::Stream::pair() else { continue };
            if local.set_nonblocking(true).is_err() {
                continue;
            }
            self.seen.push(slot);
            self.links.push(Link {
                slot,
                session,
                owner,
                accepted_at: std::time::Instant::now(),
                channel,
                local,
                to_local: Vec::new(),
                to_ring: Vec::new(),
                ring_eof: false,
                dead: false,
            });
            out.push((slot, theirs, session, sv_transport::wlbytes::is_feed(owner)));
        }
        out
    }

    /// Move bytes both ways for every live connection.
    ///
    /// Called once a turn from the Wayland thread, **between `dispatch_clients`
    /// and `flush_clients`**: bytes arriving from the ring have to be in the
    /// local socket before smithay reads it, and bytes smithay wrote have to be
    /// picked up after it wrote them. The same ordering argument the clipboard's
    /// `pump_transfers` makes for the same reason.
    pub fn pump(&mut self) {
        for link in &mut self.links {
            link.pump();
        }
        self.links.retain(|l| !l.dead);
        self.close_orphan_feeds();
    }

    /// §107.10: a feed whose client connection has gone is closed, in case
    /// its proxy did not close it. A proxy's icon and clipboard feeds live
    /// and die with the connection they serve; one left behind is a Wayland
    /// client that is no application, and it used to keep its session up for
    /// ever (the wcns session of 2026-10-19, after the bridge returned on an
    /// error without ending its ring). The proxy claims the connection before
    /// its feeds, so a feed is given [`ORPHAN_GRACE`] to have been seen with
    /// it before it counts as left behind.
    fn close_orphan_feeds(&mut self) {
        use sv_transport::wlbytes::{client_ordinal, is_feed};
        let apps: Vec<(u8, u64)> = self
            .links
            .iter()
            .filter(|l| !l.dead && !is_feed(l.owner))
            .map(|l| (l.session, client_ordinal(l.owner)))
            .collect();
        for link in self.links.iter_mut().filter(|l| !l.dead && is_feed(l.owner)) {
            if link.accepted_at.elapsed() < ORPHAN_GRACE || apps.contains(&(link.session, client_ordinal(link.owner))) {
                continue;
            }
            println!(
                "wc-compositor: ring slot {}: a feed of client connection {} outlived it; closed",
                link.slot,
                client_ordinal(link.owner)
            );
            let _ = link.channel.write_eof();
            let _ = self.control.kill_ring(link.slot);
            quarantine(link.slot);
            link.dead = true;
        }
        self.links.retain(|l| !l.dead);
    }


    /// §107: end every connection of `session`: its clients are disconnected
    /// (smithay sees their sockets close, after what they had already sent),
    /// and their slots are closed and quarantined so that a proxy still
    /// running cannot write into a slot somebody else has claimed since.
    /// Returns how many there were.
    pub fn drop_session(&mut self, session: u8) -> usize {
        let mut n = 0;
        for link in self.links.iter_mut().filter(|l| l.session == session && !l.dead) {
            let _ = self.control.kill_ring(link.slot);
            quarantine(link.slot);
            link.dead = true;
            n += 1;
        }
        self.links.retain(|l| !l.dead);
        n
    }

    /// §107: how many application connections each session has now. Feeds
    /// are not counted (§107.10): they keep no session alive.
    pub fn connections_by_session(&self) -> std::collections::HashMap<u8, usize> {
        let mut m = std::collections::HashMap::new();
        for l in self.links.iter().filter(|l| !l.dead && !sv_transport::wlbytes::is_feed(l.owner)) {
            *m.entry(l.session).or_insert(0) += 1;
        }
        m
    }

}

impl Link {
    fn pump(&mut self) {
        // --- ring -> client -------------------------------------------------
        if !self.ring_eof {
            let mut buf = [0u8; PUMP_CHUNK];
            let mut got = 0usize;
            loop {
                match self.channel.read(&mut buf[got..]) {
                    Ok(ReadOutcome::Bytes(n)) => {
                        got += n;
                        if got == buf.len() {
                            break;
                        }
                    }
                    Ok(ReadOutcome::Empty) => break,
                    Ok(ReadOutcome::Eof) => {
                        self.ring_eof = true;
                        break;
                    }
                    Err(_) => {
                        self.dead = true;
                        return;
                    }
                }
            }
            if got > 0 {
                self.to_local.extend_from_slice(&buf[..got]);
            }
        }
        if !self.to_local.is_empty() {
            match self.local.write(&self.to_local) {
                Ok(0) => self.dead = true,
                Ok(n) => drop(self.to_local.drain(..n)),
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(_) => self.dead = true,
            }
        }
        // The disconnect reaches smithay only once everything before it has.
        if self.ring_eof && self.to_local.is_empty() {
            self.dead = true;
            return;
        }

        // --- client -> ring -------------------------------------------------
        let mut buf = [0u8; PUMP_CHUNK];
        match self.local.read(&mut buf) {
            Ok(0) => {
                // smithay closed: tell the peer, in order, behind the bytes.
                let _ = self.channel.write_eof();
                self.dead = true;
                return;
            }
            Ok(n) => self.to_ring.extend_from_slice(&buf[..n]),
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(_) => {
                self.dead = true;
                return;
            }
        }
        while !self.to_ring.is_empty() {
            match self.channel.write(&self.to_ring) {
                // Zero is the ring being full, which is the proxy applying
                // backpressure and not an error. The remainder stays queued.
                Ok(0) => break,
                Ok(n) => drop(self.to_ring.drain(..n)),
                Err(_) => {
                    self.dead = true;
                    return;
                }
            }
        }
    }
}

// ------------------------------------------------- §103.14: guest-RAM rings ---

static WANT_GUEST_RAM: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
static LOCATED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
static SHARED: std::sync::Mutex<Option<Arc<Control>>> = std::sync::Mutex::new(None);

/// `--wayland-rings-guest-ram`: the session's ring region is a window of this
/// process's own RAM that the host locates (§103.14), for a guest with no
/// shared-memory device (the macOS VM). The same region carries the Wayland
/// connections, their feeds, and the session's control and sound channels.
pub fn want_guest_ram_rings() {
    WANT_GUEST_RAM.store(true, std::sync::atomic::Ordering::SeqCst);
}

pub fn wants_guest_ram_rings() -> bool {
    WANT_GUEST_RAM.load(std::sync::atomic::Ordering::SeqCst)
}

/// The launcher, once the host has located the ring region: the stamps may
/// now be overwritten by the control region.
pub fn guest_ram_rings_located() {
    LOCATED.store(true, std::sync::atomic::Ordering::SeqCst);
}

/// The ring region, once made: the launcher's control and sound channels are
/// ring slots of it.
pub fn shared_control() -> Option<Arc<Control>> {
    SHARED.lock().unwrap_or_else(|e| e.into_inner()).clone()
}

/// §103.15: what page 0 of the ring window tells the host, beyond the stamp.
/// The launcher sets the session; the arena is noted when it is provided.
static BOOT_SESSION: std::sync::Mutex<Option<(String, u64, bool)>> = std::sync::Mutex::new(None);
static ARENA0: std::sync::Mutex<Option<(u64, u32)>> = std::sync::Mutex::new(None);
/// The host's answer, for the launcher: `None` until there is one.
static BOOT_STATUS: std::sync::Mutex<Option<Result<(), String>>> = std::sync::Mutex::new(None);

/// The launcher, before the compositor starts (§103.15): put a boot record
/// for session `id` in page 0 of the ring window, so the host's session
/// service starts the session with no other channel.
/// `gpu`: the launcher has started the session's GPU server, labelled
/// `wc_handle::macboot::gpu_label(id)`, for the host to serve.
pub fn set_boot_session(id: &str, outbound: u64, gpu: bool) {
    *BOOT_SESSION.lock().unwrap_or_else(|e| e.into_inner()) = Some((id.to_string(), outbound, gpu));
}

/// The compositor, as it provides arena 0 from guest RAM.
pub fn note_guest_ram_arena(run: u64, pages: u32) {
    let mut g = ARENA0.lock().unwrap_or_else(|e| e.into_inner());
    if g.is_none() {
        *g = Some((run, pages));
    }
}

static BELL_LIVE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// How long the launcher's ring threads sleep on a lane before looking again
/// (§103.15): long once the rings have a doorbell, which ends the sleep; a
/// millisecond without one.
pub fn ring_wait_slice() -> std::time::Duration {
    if BELL_LIVE.load(std::sync::atomic::Ordering::Relaxed) {
        std::time::Duration::from_millis(500)
    } else {
        std::time::Duration::from_millis(1)
    }
}

/// The host service's answer to the boot record: `Some(Ok(()))` once it has
/// located both windows and started the session, `Some(Err(why))` if it
/// refused, `None` while it has said nothing.
pub fn boot_status() -> Option<Result<(), String>> {
    BOOT_STATUS.lock().unwrap_or_else(|e| e.into_inner()).clone()
}

/// The rings' doorbell before the control region exists: the reader, kept for
/// the doorman `into_link` starts, and the writer, which says hello now and
/// rings lanes later.
pub struct GuestRamBell {
    pub name: String,
    reader: Box<dyn Read + Send>,
    writer: Arc<sv_transport::vdoorbell::PortWake<Box<dyn Write + Send>>>,
}

impl GuestRamBell {
    pub fn new(name: &str, port: sv_transport::vdoorbell::PortHandle) -> std::io::Result<GuestRamBell> {
        let (reader, writer) = port.split()?;
        Ok(GuestRamBell { name: name.to_string(), reader, writer: Arc::new(sv_transport::vdoorbell::PortWake::new(writer)) })
    }
}

static GUEST_RAM_BELL: std::sync::Mutex<Option<GuestRamBell>> = std::sync::Mutex::new(None);

/// Keep the doorbell, opened at startup, for the ring window made later on
/// the Wayland thread.
pub fn keep_guest_ram_bell(bell: GuestRamBell) {
    *GUEST_RAM_BELL.lock().unwrap_or_else(|e| e.into_inner()) = Some(bell);
}

/// A ring region of guest RAM, stamped and waiting for the host to locate it.
pub struct PendingRings {
    window: wc_handle::arena::GuestRamWindow,
    geometry: Geometry,
    bell: Option<GuestRamBell>,
    /// Whether page 0 carries a boot record, so the answer is read there.
    booting: bool,
    last_hello: Option<std::time::Instant>,
}

impl PendingRings {
    pub fn alloc() -> Result<PendingRings, String> {
        let bell = GUEST_RAM_BELL.lock().unwrap_or_else(|e| e.into_inner()).take();
        let geometry = Geometry::for_wayland();
        // §103.15: one page more than the control region. Page 0 is the boot
        // page (the boot record, the host's answer and its release word) for
        // the whole session; the control region starts at page 1.
        let (window, run, pages) =
            wc_handle::arena::stamped_guest_ram_window(geometry.control_size + wc_handle::arena::REGION_PAGE as u64)
                .map_err(|e| e.to_string())?;
        println!(
            "wc-compositor: wayland rings in guest RAM: run 0x{run:016x}, {pages} rings pages, {} bytes, pinning={}",
            window.len, window.pinning
        );
        let session = BOOT_SESSION.lock().unwrap_or_else(|e| e.into_inner()).clone();
        let arena = *ARENA0.lock().unwrap_or_else(|e| e.into_inner());
        let mut booting = false;
        match (session, arena) {
            (Some((id, outbound, gpu)), Some((arena_run, arena_pages))) => {
                let rec = wc_handle::macboot::Record {
                    session: id.clone(),
                    arena_run,
                    arena_pages,
                    outbound,
                    rings_run: run,
                    rings_pages: pages,
                    gpu,
                };
                // SAFETY: page 0 of the window this value owns, mapped and
                // pinned, and not yet shared with anything but the host's sweep.
                let page0 = unsafe { std::slice::from_raw_parts_mut(window.base(), wc_handle::arena::REGION_PAGE) };
                rec.write(page0);
                booting = true;
                println!(
                    "wc-compositor: boot record for session {id} in page 0 of the ring window (arena run 0x{arena_run:016x}, \
                     {arena_pages} pages); waiting for the host's session service{}",
                    match &bell {
                        Some(b) => format!(", hello on {}", b.name),
                        None => " (no doorbell: its periodic sweep finds it)".into(),
                    }
                );
            }
            (Some(_), None) => println!("wc-compositor: no boot record: arena 0 is not in guest RAM"),
            _ => {}
        }
        Ok(PendingRings { window, geometry, bell, booting, last_hello: None })
    }

    /// One look at the host's answer, and a hello at most every three seconds
    /// until there is one (a hello lost, or sent before the service ran, is
    /// sent again). Returns whether the window may be made a control region.
    pub fn poll(&mut self) -> bool {
        if !self.booting {
            return LOCATED.load(std::sync::atomic::Ordering::SeqCst);
        }
        // SAFETY: page 0 of this window, which the host writes the state word of.
        match unsafe { wc_handle::macboot::load_state(self.window.base()) } {
            wc_handle::macboot::State::Located => {
                *BOOT_STATUS.lock().unwrap_or_else(|e| e.into_inner()) = Some(Ok(()));
                LOCATED.store(true, std::sync::atomic::Ordering::SeqCst);
                return true;
            }
            wc_handle::macboot::State::Failed => {
                // SAFETY: as above; the host wrote the message before the state.
                let page0 = unsafe { std::slice::from_raw_parts(self.window.base(), wc_handle::arena::REGION_PAGE) };
                let why = wc_handle::macboot::message(page0);
                let mut st = BOOT_STATUS.lock().unwrap_or_else(|e| e.into_inner());
                if st.is_none() {
                    println!("wc-compositor: the host's session service refused the session: {why}");
                    *st = Some(Err(why));
                }
                return false;
            }
            wc_handle::macboot::State::Waiting => {}
        }
        if let Some(b) = &self.bell
            && self.last_hello.is_none_or(|t| t.elapsed() >= std::time::Duration::from_secs(3))
        {
            sv_transport::park::CrossWake::poke(&*b.writer, sv_transport::park::HELLO_TOKEN);
            self.last_hello = Some(std::time::Instant::now());
        }
        false
    }

    /// Make the control region in the located window. With a doorbell, also
    /// install its writer as this process's cross-VM wake and hand back its
    /// reader, for a doorman whose target is [`GuestRamWake`].
    pub fn into_link(self) -> Result<(RingLink, Option<Box<dyn Read + Send>>), String> {
        // SAFETY: the window is leaked below, so the region outlives every
        // user of it; its length is at least the geometry's control size.
        let region = Arc::new(unsafe {
            sv_transport::region::SharedRegion::from_raw(
                self.window.base().add(wc_handle::arena::REGION_PAGE),
                self.geometry.control_size as usize,
            )
        });
        std::mem::forget(self.window);
        let ids = ProtocolIds::new(1, sv_proto::PROTOCOL_HASH);
        let control = Arc::new(Control::create(region, self.geometry, ids, 1).map_err(|e| e.to_string())?);
        control.stamp_epoch().map_err(|e| e.to_string())?;
        *SHARED.lock().unwrap_or_else(|e| e.into_inner()) = Some(Arc::clone(&control));
        let reader = match self.bell {
            Some(b) => {
                println!(
                    "wc-compositor: wayland rings created in guest RAM ({} slots), doorbell {}",
                    self.geometry.ring_slot_count, b.name
                );
                sv_transport::park::install_cross_wake(Some(b.writer as Arc<dyn sv_transport::park::CrossWake>));
                BELL_LIVE.store(true, std::sync::atomic::Ordering::Relaxed);
                Some(b.reader)
            }
            None => {
                println!(
                    "wc-compositor: wayland rings created in guest RAM ({} slots); no doorbell, polled every millisecond",
                    self.geometry.ring_slot_count
                );
                None
            }
        };
        Ok((RingLink::over(control), reader))
    }
}

/// A byte on the guest-RAM rings' port (§103.15): end the Wayland thread's
/// wait, as [`WakeOnBell`] does, and also wake whoever in this process is
/// parked on the lane it names: the launcher's control and sound threads,
/// which block on their rings' bells rather than polling.
pub struct GuestRamWake {
    pub waker: crate::tick::Waker,
    pub control: Arc<Control>,
}

impl sv_transport::vdoorbell::BellTarget for GuestRamWake {
    fn rescan(&self, woken: &std::sync::atomic::AtomicU64) {
        self.waker.wake();
        sv_transport::vdoorbell::BellTarget::rescan(&*self.control, woken);
    }

    fn ring_token(&self, token: u8, woken: &std::sync::atomic::AtomicU64) -> bool {
        self.waker.wake();
        sv_transport::vdoorbell::BellTarget::ring_token(&*self.control, token, woken);
        woken.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        true
    }

}

/// What a byte on the rings' port does in the compositor (§89): end the
/// Wayland thread's wait. The byte names a slot and a lane, and which one does
/// not matter — the turn it starts pumps every ring.
pub struct WakeOnBell(pub crate::tick::Waker);

impl sv_transport::vdoorbell::BellTarget for WakeOnBell {
    /// The loop's turn pumps every ring, so one wake is a rescan.
    fn rescan(&self, _woken: &std::sync::atomic::AtomicU64) {
        self.0.wake();
    }

    fn ring_token(&self, _token: u8, woken: &std::sync::atomic::AtomicU64) -> bool {
        woken.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.0.wake();
        true
    }

}

/// Open the Wayland rings' doorbell (waycompose-design.md §89): the
/// virtio-serial port `name` on the guest, or — when `name` is a path — the
/// Unix socket a test stands in for it with.
///
/// Half a second, not the ten `PortHandle::guest_port` waits: that wait is for
/// a port being hot-added under a harness, and here a missing port is a VM
/// without the device, whose answer is the socket path, now rather than in ten
/// seconds of a user's click. A port another process holds is refused at once
/// by the driver (one opener per port), which is the same answer.
pub fn open_bell_port(name: &str) -> std::io::Result<sv_transport::vdoorbell::PortHandle> {
    // §105.4's spellings: `fd:R,W` is how a parent passes a port it opened
    // (the macOS launcher in role (b), §103.17: the hypervisor's FIFO pair).
    if ["fd:", "dev:", "socket:"].iter().any(|p| name.starts_with(p)) {
        return sv_transport::vdoorbell::PortHandle::open(name);
    }
    #[cfg(unix)]
    if name.contains('/') {
        return sv_transport::vdoorbell::PortHandle::host_socket(name);
    }
    #[cfg(windows)]
    {
        sv_transport::winpipe::open_virtio_port(name, 500)
            .map(|p| sv_transport::vdoorbell::PortHandle::Pipe(Arc::new(p)))
            .map_err(|e| std::io::Error::other(format!("{e}")))
    }
    #[cfg(not(windows))]
    sv_transport::vdoorbell::PortHandle::guest_port(name)
}

/// The port's reader, when there is a port (§105: the rings are polled without).
pub type BellReader = Option<Box<dyn Read + Send>>;

/// The ring region at the tail of ivshmem arena `id` (§89): create the control
/// region there, install the port's writer as this process's cross-VM
/// doorbell, and hand back the listener, the port's reader — which the
/// Wayland thread turns into wakes of its own loop — and the slot count.
///
/// The region is the compositor's to create, as every ring region has been
/// (this module's header): it zeroes the tail and publishes a control header
/// the proxy attaches to after it has checked the arena header's run id.
pub fn over_ivshmem(id: u32, port: Option<sv_transport::vdoorbell::PortHandle>) -> Result<(RingLink, BellReader, u32), String> {
    let (base, len) = wc_handle::arena::ivshmem_rings(id).map_err(|e| e.to_string())?;
    // SAFETY: `ivshmem_rings` returns the tail of the ivshmem view, which the
    // arena registry keeps mapped for the life of the process, `len` bytes
    // long; nothing else in this process uses those bytes.
    let region = Arc::new(unsafe { sv_transport::region::SharedRegion::from_raw(base, len) });
    let geometry = ivshmem_geometry();
    let ids = ProtocolIds::new(1, sv_proto::PROTOCOL_HASH);
    let control = Control::create(region, geometry, ids, 1).map_err(|e| e.to_string())?;
    control.stamp_epoch().map_err(|e| e.to_string())?;
    let control = Arc::new(control);
    // §105: the session's service threads (control, sound) use the same region.
    *SHARED.lock().unwrap_or_else(|e| e.into_inner()) = Some(Arc::clone(&control));
    let reader = match port {
        Some(port) => {
            let (reader, writer) = port.split().map_err(|e| e.to_string())?;
            // The writer is what `Bell::ring` pokes when the proxy is parked on a lane
            // this process writes. One per process, and this process has no other
            // rings.
            sv_transport::park::install_cross_wake(Some(
                Arc::new(sv_transport::vdoorbell::PortWake::new(writer)) as Arc<dyn sv_transport::park::CrossWake>,
            ));
            BELL_LIVE.store(true, std::sync::atomic::Ordering::Relaxed);
            Some(reader)
        }
        None => None,
    };
    Ok((RingLink::over(control), reader, geometry.ring_slot_count))
}

/// Why `--wayland-cold` is Linux-only, in one place.
///
/// The message names the asymmetry rather than the platform, because "not
/// supported on Windows" would invite someone to add a `cfg` and move on.
// Dead on Linux by construction: it is the message for the platforms that
// cannot honour the flag, and Linux can. Kept unconditional rather than
// `cfg`-ed so there is exactly one copy of the wording, which is the whole
// point of hoisting it out of the two places that print it.
#[cfg_attr(target_os = "linux", allow(dead_code))]
pub const COLD_NEEDS_LINUX: &str = "--wayland-cold publishes a control region out of guest RAM, \
     which needs the Linux guest-RAM provider (sv_arena::guestram). The Windows guest commits its \
     window through awe_probe's VirtualAlloc path instead — that is what `sv-gpu-server`'s \
     win_guest does — and no compositor has been taught to do it yet. Use --wayland-ring with a \
     section name, or run the Linux guest fixture.";

/// Refuse a flag this build cannot honour, **at parse**.
///
/// # Why this exists at all
///
/// `--wayland-ring` and `--wayland-cold` used to sit behind a `#[cfg(unix)]`
/// block. On Windows they were parsed, accepted, and silently ignored, and the
/// compositor fell through to the socket listener — so the claim that Wayland
/// rides shared_vulkan's channel was true on the machine the tests run on and
/// false on the machine the thing ships to, and **the only artefact that said so
/// was an `unused variable` warning**.
///
/// That is §35's failure mode in its purest form: a gate that reports success
/// while something is wrong. The rule this encodes is that a flag which cannot
/// be honoured must fail loudly where the user typed it, never be quietly
/// dropped and never merely warn at compile time — a compile warning is
/// addressed to whoever built the binary, and the person who needs to know is
/// whoever ran it.
pub fn check_platform_flags(
    wayland_ring: Option<&str>,
    wayland_cold: Option<&str>,
) -> Result<(), String> {
    let _ = wayland_ring;
    #[cfg(not(target_os = "linux"))]
    if wayland_cold.is_some() {
        return Err(COLD_NEEDS_LINUX.to_string());
    }
    let _ = wayland_cold;
    Ok(())
}

#[cfg(test)]
mod platform_flag_tests {
    use super::*;

    /// The regression this file's `check_platform_flags` exists for.
    ///
    /// On Windows `--wayland-cold` cannot be honoured, and the old code said so
    /// only by not using the variable. A build warning is addressed to whoever
    /// compiled the binary; the person who needs to know is whoever ran it.
    #[test]
    fn a_flag_this_platform_cannot_honour_is_refused_at_parse() {
        let got = check_platform_flags(None, Some("/dev/virtio-ports/x"));
        if cfg!(target_os = "linux") {
            assert!(got.is_ok(), "Linux can publish guest RAM; it must not refuse");
        } else {
            let e = got.expect_err("a flag that will be ignored must not be accepted");
            // The message must name the *asymmetry*, not the platform: "not
            // supported on Windows" invites somebody to add a cfg and move on.
            assert!(e.contains("guest RAM"), "the reason is missing from: {e}");
            assert!(e.contains("--wayland-ring"), "the way out is missing from: {e}");
        }
    }

    /// `--wayland-ring` is honoured everywhere now — a file on Unix, a named
    /// section on Windows — so it must never be refused on either.
    #[test]
    fn the_ring_flag_is_accepted_on_every_platform() {
        assert!(check_platform_flags(Some("wc-ring"), None).is_ok());
    }

    /// And the region really can be created and its directory polled, whichever
    /// kind of object it is underneath. This is the part that was `#[cfg(unix)]`.
    #[test]
    fn a_ring_region_can_be_created_and_starts_empty() {
        let name = if cfg!(windows) {
            format!("wc-ringlink-test-{}", std::process::id())
        } else {
            let d = std::env::temp_dir().join(format!("wc-ringlink-{}", std::process::id()));
            d.to_string_lossy().into_owned()
        };
        let mut rl = RingLink::create(&name).expect("the region must be creatable");
        assert!(rl.accept().is_empty(), "a fresh directory has no connections");
        rl.pump();
        if !cfg!(windows) {
            let _ = std::fs::remove_file(&name);
        }
    }
}

// ------------------------------------------- §107: slots of ended sessions ---

/// How long a slot this side closed stays out of [`RingLink::reap`]: longer
/// than a proxy's lease (five seconds without a ping end it), so a proxy that
/// was still running when its session was ended has stopped before its slot
/// can mean somebody else's connection.
/// §107: the most connections one session of a shared compositor may hold:
/// a little under half of the 63 slots, so that one installation's flood
/// leaves room for the others. A Firefox session is twelve to fifteen.
pub const SLOTS_PER_SESSION: usize = 28;

pub const QUARANTINE: std::time::Duration = std::time::Duration::from_secs(10);

/// §107.10: how long a feed may be connected without the client connection
/// it serves before [`RingLink::pump`] closes it.
pub const ORPHAN_GRACE: std::time::Duration = std::time::Duration::from_secs(2);

static QUARANTINED: std::sync::Mutex<Vec<(u32, std::time::Instant)>> = std::sync::Mutex::new(Vec::new());

/// Keep `slot` from being recycled for [`QUARANTINE`].
pub fn quarantine(slot: u32) {
    let mut q = QUARANTINED.lock().unwrap_or_else(|e| e.into_inner());
    q.retain(|(s, _)| *s != slot);
    q.push((slot, std::time::Instant::now() + QUARANTINE));
}

/// Whether `slot` is still in quarantine (expired entries are dropped).
pub fn quarantined(slot: u32) -> bool {
    let mut q = QUARANTINED.lock().unwrap_or_else(|e| e.into_inner());
    let now = std::time::Instant::now();
    q.retain(|(_, until)| *until > now);
    q.iter().any(|(s, _)| *s == slot)
}

/// The ring region's geometry for the ivshmem tail: §107's 64 slots when this
/// compositor serves several installations, §89's 32 otherwise.
pub fn ivshmem_geometry() -> Geometry {
    if crate::session_svc::multi() {
        Geometry::for_wayland_shared()
    } else {
        Geometry::for_wayland()
    }
}

#[cfg(test)]
mod quarantine_tests {
    #[test]
    fn a_quarantined_slot_is_held_and_others_are_not() {
        super::quarantine(61);
        assert!(super::quarantined(61));
        assert!(!super::quarantined(62));
    }
}

#[cfg(test)]
mod feed_tests {
    use super::*;
    use sv_transport::wlbytes::{feed_owner, with_session};

    fn link_pair() -> (RingLink, Control) {
        let g = Geometry::small();
        let r = Arc::new(sv_transport::region::SharedRegion::heap(g.control_size as usize).unwrap());
        let ids = ProtocolIds::new(1, 2);
        let owner = Control::create(Arc::clone(&r), g, ids, 1).unwrap();
        (RingLink::over(Arc::new(owner)), Control::attach(r, ids).unwrap())
    }

    /// §107.10: a feed is a client to smithay and nothing more. It does not
    /// count as a session's connection, and once the connection it serves
    /// has gone it is closed even if its proxy never closes it -- the wcns
    /// session of 2026-10-19 that stayed up past its linger for ever.
    #[test]
    fn a_feed_keeps_no_session_up_and_does_not_outlive_its_client() {
        let (mut rl, proxy) = link_pair();
        let app = ByteChannel::claim(&proxy, with_session(5, 0)).unwrap();
        let icons = ByteChannel::claim(&proxy, with_session(feed_owner(5, false), 0)).unwrap();
        let mut accepted = rl.accept();
        accepted.sort_by_key(|a| a.0);
        let feeds: Vec<bool> = accepted.iter().map(|a| a.3).collect();
        assert_eq!(feeds, [false, true], "the connection, then its feed");
        assert_eq!(rl.connections_by_session().get(&0), Some(&1), "the feed is not counted");

        // The application goes, as a proxy ends a connection.
        let mut app = app;
        app.write_eof().unwrap();
        app.kill(&proxy).unwrap();
        rl.pump();
        rl.reap();
        assert_eq!(rl.connections_by_session().get(&0), None);
        assert_eq!(rl.links.len(), 1, "the feed is still within its grace");

        std::thread::sleep(ORPHAN_GRACE + std::time::Duration::from_millis(50));
        rl.pump();
        assert!(rl.links.is_empty(), "the feed outlived its client and was closed");
        // And its proxy is told: the end of stream, and the slot marked dead.
        let mut icons = icons;
        let mut buf = [0u8; 8];
        assert_eq!(icons.read(&mut buf).unwrap(), ReadOutcome::Eof);
        assert_eq!(
            proxy.ring_state(icons.slot()).unwrap(),
            sv_transport::control::RingState::Dead
        );
    }

    /// A feed whose client is still connected is left alone, however long.
    #[test]
    fn a_feed_of_a_live_client_stays() {
        let (mut rl, proxy) = link_pair();
        let _app = ByteChannel::claim(&proxy, with_session(8, 0)).unwrap();
        let _clip = ByteChannel::claim(&proxy, with_session(feed_owner(8, true), 0)).unwrap();
        // Another client's connection does not make this feed's.
        let _other = ByteChannel::claim(&proxy, with_session(9, 0)).unwrap();
        // Held: they are smithay's ends, and dropping one is a disconnect.
        let accepted = rl.accept();
        assert_eq!(accepted.len(), 3);
        std::thread::sleep(ORPHAN_GRACE + std::time::Duration::from_millis(50));
        rl.pump();
        assert_eq!(rl.links.len(), 3);
        assert_eq!(rl.connections_by_session().get(&0), Some(&2));
    }
}
