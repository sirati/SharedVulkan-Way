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
    /// Which directory slot this is. Carried so a stuck connection can be named
    /// in a log by the same number both ends use for it.
    #[allow(dead_code)]
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
    pub fn create(path: &str) -> Result<RingLink, String> {
        let geometry = Geometry::for_arena(256 << 20);
        let f = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(path)
            .map_err(|e| format!("{path}: {e}"))?;
        f.set_len(geometry.control_size).map_err(|e| format!("{path}: {e}"))?;
        let region = Arc::new(
            sv_transport::region::SharedRegion::map_fd(
                std::os::fd::AsFd::as_fd(&f),
                0,
                geometry.control_size as usize,
            )
            .map_err(|e| e.to_string())?,
        );
        // The same ids the Vulkan half uses. R13: two builds that disagree must
        // fail to connect rather than desynchronise.
        let ids = ProtocolIds::new(1, sv_proto::PROTOCOL_HASH);
        let control = Control::create(region, geometry, ids, 1).map_err(|e| e.to_string())?;
        control.stamp_epoch().map_err(|e| e.to_string())?;
        Ok(RingLink { control: Arc::new(control), links: Vec::new(), seen: Vec::new() })
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
