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
    pub fn accept(&mut self) -> Vec<(u32, wc_transport::Stream)> {
        let live = match ByteChannel::live_connections(&self.control) {
            Ok(v) => v,
            Err(_) => return Vec::new(),
        };
        let mut out = Vec::new();
        for slot in live {
            if self.seen.contains(&slot) {
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
                channel,
                local,
                to_local: Vec::new(),
                to_ring: Vec::new(),
                ring_eof: false,
                dead: false,
            });
            out.push((slot, theirs));
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

/// What a byte on the rings' port does in the compositor (§89): end the
/// Wayland thread's wait. The byte names a slot and a lane, and which one does
/// not matter — the turn it starts pumps every ring.
pub struct WakeOnBell(pub crate::tick::Waker);

impl sv_transport::vdoorbell::BellTarget for WakeOnBell {
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

/// The ring region at the tail of ivshmem arena `id` (§89): create the control
/// region there, install the port's writer as this process's cross-VM
/// doorbell, and hand back the listener, the port's reader — which the
/// Wayland thread turns into wakes of its own loop — and the slot count.
///
/// The region is the compositor's to create, as every ring region has been
/// (this module's header): it zeroes the tail and publishes a control header
/// the proxy attaches to after it has checked the arena header's run id.
pub fn over_ivshmem(
    id: u32,
    port: sv_transport::vdoorbell::PortHandle,
) -> Result<(RingLink, Box<dyn Read + Send>, u32), String> {
    let (base, len) = wc_handle::arena::ivshmem_rings(id).map_err(|e| e.to_string())?;
    // SAFETY: `ivshmem_rings` returns the tail of the ivshmem view, which the
    // arena registry keeps mapped for the life of the process, `len` bytes
    // long; nothing else in this process uses those bytes.
    let region = Arc::new(unsafe { sv_transport::region::SharedRegion::from_raw(base, len) });
    let geometry = Geometry::for_wayland();
    let ids = ProtocolIds::new(1, sv_proto::PROTOCOL_HASH);
    let control = Control::create(region, geometry, ids, 1).map_err(|e| e.to_string())?;
    control.stamp_epoch().map_err(|e| e.to_string())?;
    let (reader, writer) = port.split().map_err(|e| e.to_string())?;
    // The writer is what `Bell::ring` pokes when the proxy is parked on a lane
    // this process writes. One per process, and this process has no other
    // rings.
    sv_transport::park::install_cross_wake(Some(
        Arc::new(sv_transport::vdoorbell::PortWake::new(writer)) as Arc<dyn sv_transport::park::CrossWake>,
    ));
    Ok((RingLink::over(Arc::new(control)), reader, geometry.ring_slot_count))
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
