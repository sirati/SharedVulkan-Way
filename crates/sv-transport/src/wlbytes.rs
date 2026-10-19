//! A bidirectional byte stream over **one ring slot** of an existing session.
//!
//! waycompose-design.md §36. This is what carries the Wayland wire protocol
//! across the VM boundary now, in place of `wc-proxy → ssh -L → TCP →
//! probe-relay.exe → afunix.sys`.
//!
//! # The whole design, which is mostly a list of things not built
//!
//! A Wayland connection is a bidirectional byte stream. A ring slot is *already*
//! a bidirectional byte stream: [`crate::control::Layout`] gives every slot two
//! independent data buffers with two head/tail pairs and two doorbells, which is
//! §4.4's command lane and reply lane. So:
//!
//! | Wayland | is | shared_vulkan |
//! |---|---|---|
//! | a connection | is | a ring slot of [`RingKind::Wayland`] |
//! | `connect()` | is | [`Control::claim_ring`] — a CAS from `Free` to `Claimed` |
//! | `accept()` | is | polling the ring directory for a `Live` ring of that kind |
//! | client→server bytes | are | the **command** lane |
//! | server→client bytes | are | the **reply** lane |
//! | `close()` | is | a zero-length [`PacketKind::Bytes`] packet |
//! | the socket buffer | is | the ring, and its backpressure is `RingFull` |
//!
//! **Nothing in that table is new code.** The claim, the directory, the lanes,
//! the framing, the seqnos, the doorbells and the wrap handling were all built
//! for Vulkan and are used unchanged. What this module adds is the two hundred
//! lines that turn "packets of at most `max_payload` bytes" into "a byte stream",
//! which is chunking on the way in and reassembly on the way out.
//!
//! That is the point of the correction §36 records. The previous design would
//! have grown a second locator on the cold path, a second control region and a
//! second doorbell convention for Wayland, in a tree that already had one of
//! each working. The cold path already delivered this session's control region;
//! a ring claim needs no bootstrap beyond it, because the directory is *in* the
//! region being located.
//!
//! # One ring per connection, and why that is §0.12 again
//!
//! Firefox opens four Wayland connections. They get four ring slots.
//!
//! The temptation is one ring with a connection id in each packet, and it is the
//! same temptation `--clients N` gave in to: §0.11's bug was N clients sharing
//! one session, and §0.12's fix was N sessions. One ring carrying four
//! connections would have four streams sharing one head/tail pair, so a
//! connection that stopped reading would stall the other three — head-of-line
//! blocking between clients that have nothing to do with each other, which is
//! precisely the failure `--clients N` produced one level up. It would also need
//! the id-rewriting layer `wc-proxy` deliberately does not have
//! (`crates/wc-proxy/src/run.rs`, the note at the accept loop: "multiplexing two
//! clients onto one would need an id-rewriting layer that this deliberately is
//! not (§9.3)").
//!
//! With `ring_slot_count = 16`, slot 0 permanently `Reverse` and three claimed
//! by Vulkan, twelve remain. Firefox wants four.
//!
//! # No descriptor crosses, and the count is still unrepresentable (§14.7)
//!
//! §14.7 argues that the fd count at the boundary is *unrepresentable* rather
//! than zero, because the crossing hop was `write_some(upstream.as_raw_fd(),
//! &to_server)` — a plain `write`, with no argument that could carry one.
//!
//! **That argument survives this change, and gets stronger.** The crossing hop
//! is now [`ByteChannel::write`], whose only parameter is `&[u8]`. There is no
//! ancillary-data path to leave unused, because there is no path at all: the
//! bytes reach the peer through [`crate::region::SharedRegion::write_bytes`],
//! which takes an offset and a slice and writes into a mapping. A descriptor is
//! a per-process integer index into a kernel table; there is no encoding of one
//! that survives being memcpy'd into a page the other kernel maps, and this
//! module could not send one if it were asked to. The `write` at least had a
//! `sendmsg` sibling one refactor away. This has none — the type system, not
//! review, is what enforces it, and the enforcement is stronger than it was.
//!
//! `wl_shm.create_pool`'s `h` argument is unaffected because it never reached
//! this layer: `wc-proxy`'s session translates it into an arena handle of
//! integers before the bytes are handed over, and on Windows an `h` is encoded
//! in band as a length-prefixed descriptor (§2.3) — the same property
//! `scripts/probe-relay.c` relied on to be safe over TCP, relied on here for the
//! same reason and with one fewer hop to rely on it.
//!
//! # Blocking, and why nothing here parks
//!
//! [`crate::park`]'s own header is explicit that a futex or `WaitOnAddress` word
//! is a same-kernel mechanism and that **across the hypervisor the doorbell
//! reaches nobody** — cross-VM, `Futex` degrades to `SpinYield` with a 20 ms
//! sleep slice. So this module does not park and does not spin. Every method is
//! non-blocking and returns "nothing yet" as a value, so that each side can
//! service the ring from the event loop it already runs:
//!
//! - the **guest** compositor's Wayland thread already ticks at 5 ms on a
//!   `CreateWaitableTimerExW` high-resolution timer (§13.8: a `Sleep(1)` really
//!   costs 15.8 ms, the waitable timer 5.1 ms), and already calls
//!   `dispatch_clients` and `flush_clients` once a turn. The ring is serviced in
//!   the same turn, and costs no wakeup that was not already happening.
//! - the **host** proxy already runs `libc::poll` per client thread. Its timeout
//!   becomes the ring's poll interval.
//!
//! The doorbell is still rung — [`RingProducer::push_bytes`] does it
//! unconditionally, via `Bell::ring`, which is an acquire load and a syscall
//! only when someone is parked. Nobody is parked here, so it is an acquire load.
//! It costs nothing and it means that the day this runs on one kernel — Wine,
//! the same-machine loop, or the day `sv-guest` runs both halves in one process
//! — the wakeup already works with no change.

use crate::control::{Control, RingKind, RingState};
use crate::ring::{Lane, PacketKind, RingConsumer, RingProducer};
use crate::{Error, Result};

/// Which end of the ring slot this channel is.
///
/// The lanes are named from the *owner's* point of view, exactly as they are for
/// a Vulkan ring: the side that claimed the slot produces on the command lane.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Endpoint {
    /// The side that called [`Control::claim_ring`]. Writes the command lane,
    /// reads the reply lane. This is the **host proxy**, because the host is
    /// where a Wayland client connects and therefore where a connection begins.
    Owner,
    /// The side that found the ring in the directory. Writes the reply lane,
    /// reads the command lane. This is the **guest compositor**.
    Peer,
}

impl Endpoint {
    fn tx(self) -> Lane {
        match self {
            Endpoint::Owner => Lane::Command,
            Endpoint::Peer => Lane::Reply,
        }
    }
    fn rx(self) -> Lane {
        match self {
            Endpoint::Owner => Lane::Reply,
            Endpoint::Peer => Lane::Command,
        }
    }
}

/// What a read produced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadOutcome {
    /// Bytes were copied out. Never zero.
    Bytes(usize),
    /// The ring is empty. Ask again; this is `EWOULDBLOCK`, not an end.
    Empty,
    /// The peer sent its zero-length packet. This is `read() == 0`: no further
    /// bytes will arrive on this lane, ever, and everything before it has
    /// already been returned.
    Eof,
}

/// One Wayland connection, carried on one ring slot.
pub struct ByteChannel {
    tx: RingProducer,
    rx: RingConsumer,
    slot: u32,
    endpoint: Endpoint,
    /// Bytes copied out of a packet that the caller's buffer was too small to
    /// take in one go. Drained before the ring is looked at again, because a
    /// byte stream that reordered around a short read would not be one.
    held: Vec<u8>,
    held_at: usize,
    /// The peer's zero-length packet has been seen and reported.
    rx_eof: bool,
    /// Our zero-length packet has been sent.
    tx_eof: bool,
}

/// §103.14: owner-id tags for byte channels that carry a session service
/// instead of a Wayland client. A compositor accepting connections skips any
/// slot whose owner id has [`SERVICE_TAG`] in its top byte; the macOS main
/// process opens them by their tag.
pub const SERVICE_TAG: u64 = 0x5C << 56;
/// The session's control channel: launch requests, replies and the liveness
/// heartbeat.
pub const OWNER_CONTROL: u64 = SERVICE_TAG | 1;
/// The session's sound: raw s16le 48 kHz stereo PCM, host to guest.
pub const OWNER_AUDIO: u64 = SERVICE_TAG | 2;
/// Host integration (`wc-hostint`): requests either side may start, such as
/// opening a URL or a file on the other side, and the host's audio devices
/// and file-type handlers, pushed to Linux. One line per message.
pub const OWNER_HOSTINT: u64 = SERVICE_TAG | 3;

/// §107: the session's arena: the proxy asks the compositor for regions of
/// the shared BAR (`grow`) and gives them back (`release`). Its own number:
/// §107 and §106 were written apart and both took 3, so on a guest with host
/// integration on, a session's `grow` was answered with hostint's hello.
pub const OWNER_ARENA: u64 = SERVICE_TAG | 4;

/// Every session service's owner id, each claimed once per session. A new
/// service goes here, and the test below keeps the numbers apart.
pub const SERVICE_OWNERS: [u64; 4] = [OWNER_CONTROL, OWNER_AUDIO, OWNER_HOSTINT, OWNER_ARENA];

/// Whether an owner id names a session service rather than a client.
pub fn is_service(owner_id: u64) -> bool {
    owner_id >> 56 == SERVICE_TAG >> 56
}

/// §107: where a claim says which session of a shared compositor it is.
/// Bits 48..56 of every owner id a proxy claims with: the session index the
/// compositor gave the session in its boot record. Zero is "no index", the
/// single-session compositor every guest had before §107, so an owner id
/// from an older proxy reads as session 0 and nothing changes for it.
pub const SESSION_SHIFT: u32 = 48;
/// The bits [`SESSION_SHIFT`] names.
pub const SESSION_MASK: u64 = 0xFF << SESSION_SHIFT;

/// `owner` tagged with session `index`. The bits are cleared first, so a
/// caller's own numbering never leaks into them.
pub fn with_session(owner: u64, index: u8) -> u64 {
    (owner & !SESSION_MASK) | (u64::from(index) << SESSION_SHIFT)
}

/// The session index an owner id carries (0 for none).
pub fn session_of(owner: u64) -> u8 {
    ((owner & SESSION_MASK) >> SESSION_SHIFT) as u8
}

/// The owner id with its session index taken out: what the claim is
/// (a client's ordinal, a feed, [`OWNER_CONTROL`] ...).
pub fn without_session(owner: u64) -> u64 {
    owner & !SESSION_MASK
}

#[cfg(test)]
mod session_tag_tests {
    use super::*;

    #[test]
    fn a_session_index_rides_in_the_owner_id_and_leaves_the_rest_alone() {
        for base in [1u64, 7, (1 << 40) | 3, (1 << 40) | (1 << 39) | 3, OWNER_CONTROL, OWNER_AUDIO, OWNER_HOSTINT, OWNER_ARENA] {
            for idx in [0u8, 1, 2, 200, 255] {
                let o = with_session(base, idx);
                assert_eq!(session_of(o), idx);
                assert_eq!(without_session(o), base);
                assert_eq!(is_service(o), is_service(base), "the service tag survives the index");
            }
        }
        // An untagged id is session 0: every proxy from before §107.
        assert_eq!(session_of(OWNER_CONTROL), 0);
        assert_eq!(session_of(5), 0);
        // Re-tagging replaces, never ORs two sessions together.
        assert_eq!(session_of(with_session(with_session(9, 3), 4)), 4);
    }

    /// A compositor finds a session's service slot by its owner id alone, so
    /// two services with one number share whichever slot is found first.
    #[test]
    fn every_session_service_has_its_own_owner_id() {
        for (i, a) in SERVICE_OWNERS.iter().enumerate() {
            assert!(is_service(*a));
            assert_eq!(session_of(*a), 0, "a service id carries no session index of its own");
            for b in &SERVICE_OWNERS[i + 1..] {
                assert_ne!(a, b, "two session services share owner id {a:#x}");
            }
        }
    }
}

/// See [`ByteChannel::kicker`].
pub struct Kicker {
    word: *const std::sync::atomic::AtomicU32,
    _control: std::sync::Arc<Control>,
}

// SAFETY: the word is an atomic in a shared mapping that `_control` keeps
// mapped; touching it from any thread is what it is for.
unsafe impl Send for Kicker {}
unsafe impl Sync for Kicker {}

impl Kicker {
    pub fn kick(&self) {
        // SAFETY: see the Send/Sync impls.
        let w = unsafe { &*self.word };
        w.fetch_add(1, std::sync::atomic::Ordering::Release);
        crate::park::sys::wake(w);
    }
}

impl ByteChannel {
    /// Claim a free ring slot for a new connection. The host proxy's `connect`.
    ///
    /// `owner_id` is recorded in the directory and is for diagnostics only —
    /// give it the connection's ordinal, so a stuck ring can be named in a log.
    pub fn claim(control: &Control, owner_id: u64) -> Result<ByteChannel> {
        let slot = control.claim_ring(RingKind::Wayland, owner_id, None)?;
        ByteChannel::open(control, slot, Endpoint::Owner)
    }

    /// Attach to a slot somebody else claimed. The guest compositor's `accept`.
    pub fn open(control: &Control, slot: u32, endpoint: Endpoint) -> Result<ByteChannel> {
        if control.ring_kind(slot)? != RingKind::Wayland {
            return Err(Error::NoSuchRing { ring: slot });
        }
        Ok(ByteChannel {
            tx: RingProducer::open(control, slot, endpoint.tx())?,
            rx: RingConsumer::open(control, slot, endpoint.rx())?,
            slot,
            endpoint,
            held: Vec::new(),
            held_at: 0,
            rx_eof: false,
            tx_eof: false,
        })
    }

    /// Every ring in the directory that is a `Live` Wayland connection.
    ///
    /// **This is the accept loop**, and it is a poll of shared memory rather
    /// than a syscall. `sv-gpu-server`'s `session::serve` does exactly this for
    /// Instance/Device/Queue rings; a compositor calls it once a tick and opens
    /// a [`ByteChannel`] on any slot it has not seen before.
    ///
    /// A `Dead` slot is not returned: `kill_ring` is how a connection that was
    /// torn down without an orderly EOF — a crashed proxy — becomes visible.
    pub fn live_connections(control: &Control) -> Result<Vec<u32>> {
        let mut out = Vec::new();
        for slot in 0..control.layout().geometry.ring_slot_count {
            if control.ring_state(slot)? == RingState::Live
                && control.ring_kind(slot).ok() == Some(RingKind::Wayland)
            {
                out.push(slot);
            }
        }
        Ok(out)
    }

    pub fn slot(&self) -> u32 {
        self.slot
    }

    pub fn endpoint(&self) -> Endpoint {
        self.endpoint
    }

    /// The most bytes a single packet can carry on this ring.
    ///
    /// A `write` longer than this is chunked, not refused: a byte stream has no
    /// message boundaries to preserve, which is the one respect in which it is
    /// easier to carry than a command stream.
    pub fn max_chunk(&self) -> usize {
        // `max_payload` subtracts the barrier reservation, which a byte packet
        // never uses. Keeping the conservative figure costs 16 bytes per packet
        // out of 256 KiB and means this number is true for both kinds.
        self.tx.max_payload() as usize
    }

    /// Push up to `buf.len()` bytes, returning how many were taken.
    ///
    /// **Returns `Ok(0)` when the ring is full**, which is backpressure and not
    /// an error — the same shape as a non-blocking `write` returning
    /// `EWOULDBLOCK`, and the caller should hold the remainder and retry. A
    /// Wayland client that outruns the compositor is supposed to be slowed down,
    /// not disconnected.
    pub fn write(&mut self, buf: &[u8]) -> Result<usize> {
        if self.tx_eof {
            return Err(Error::BadColdMessage("write after end of stream"));
        }
        if buf.is_empty() {
            return Ok(0);
        }
        let n = buf.len().min(self.max_chunk());
        match self.tx.push_bytes(&buf[..n]) {
            Ok(_) => Ok(n),
            Err(Error::RingFull { .. }) => Ok(0),
            Err(e) => Err(e),
        }
    }

    /// Send the zero-length packet that means end of stream, once.
    ///
    /// Ordered behind every byte already written, because it is a packet in the
    /// same ring rather than a flag beside it.
    pub fn write_eof(&mut self) -> Result<bool> {
        if self.tx_eof {
            return Ok(true);
        }
        match self.tx.push_bytes(&[]) {
            Ok(_) => {
                self.tx_eof = true;
                Ok(true)
            }
            // Even the end has to wait for space. Reporting "not yet" rather
            // than dropping it is what stops a close from being lost under load.
            Err(Error::RingFull { .. }) => Ok(false),
            Err(e) => Err(e),
        }
    }

    /// Copy out up to `buf.len()` bytes.
    pub fn read(&mut self, buf: &mut [u8]) -> Result<ReadOutcome> {
        if buf.is_empty() {
            return Ok(ReadOutcome::Empty);
        }
        // Anything held from a previous short read comes first, always.
        if self.held_at < self.held.len() {
            let n = buf.len().min(self.held.len() - self.held_at);
            buf[..n].copy_from_slice(&self.held[self.held_at..self.held_at + n]);
            self.held_at += n;
            if self.held_at == self.held.len() {
                self.held.clear();
                self.held_at = 0;
            }
            return Ok(ReadOutcome::Bytes(n));
        }
        if self.rx_eof {
            return Ok(ReadOutcome::Eof);
        }

        let taken = {
            let Some(pkt) = self.rx.next()? else {
                return Ok(ReadOutcome::Empty);
            };
            if pkt.kind != PacketKind::Bytes {
                // A command packet on a Wayland ring is a wiring mistake — two
                // builds that disagree about what this slot is — and is worth an
                // error rather than being handed to a wayland parser that would
                // report it as a protocol violation by the innocent client.
                return Err(Error::BadPacketLength { ring: self.slot, len: u32::MAX });
            }
            if pkt.payload.is_empty() {
                Taken::Eof
            } else if pkt.payload.len() <= buf.len() {
                buf[..pkt.payload.len()].copy_from_slice(pkt.payload);
                Taken::Direct(pkt.payload.len())
            } else {
                // Short read: keep the whole packet and serve it from `held`.
                // One extra copy, only on the path where the caller's buffer was
                // the smaller of the two.
                let n = buf.len();
                buf.copy_from_slice(&pkt.payload[..n]);
                Taken::Held(n, pkt.payload[n..].to_vec())
            }
        };
        self.rx.advance()?;
        Ok(match taken {
            Taken::Eof => {
                self.rx_eof = true;
                ReadOutcome::Eof
            }
            Taken::Direct(n) => ReadOutcome::Bytes(n),
            Taken::Held(n, rest) => {
                self.held = rest;
                self.held_at = 0;
                ReadOutcome::Bytes(n)
            }
        })
    }

    /// Is there anything to read without touching the ring?
    pub fn has_buffered(&self) -> bool {
        self.held_at < self.held.len()
    }

    /// Would [`Self::read`] return something other than `Empty` — bytes, or the
    /// end of the stream? Reads the lane's head and tail and nothing else.
    ///
    /// This is the consumer's last look before it sleeps (waycompose-design.md
    /// §89): set `parked` on [`Self::rx_bell`], *then* ask this, and sleep only
    /// on `false`. A producer that published before the `parked` store is seen
    /// here; one that published after it sees `parked` and rings. That is
    /// [`crate::park::Bell`]'s handshake, done by a caller whose wait is a
    /// `poll` rather than a futex.
    pub fn readable(&self) -> Result<bool> {
        Ok(self.has_buffered() || self.rx_eof || !self.rx.is_empty()?)
    }

    /// The doorbell of the lane this end reads: the words a sleeping reader
    /// sets so that the other end's write rings it (§89).
    pub fn rx_bell(&self) -> Result<crate::park::Bell<'_>> {
        self.rx.ctrl().bell(self.endpoint.rx())
    }

    /// A handle another thread can use to end this end's
    /// [`Self::wait_readable`] early (§103.16): it rings the lane's own word
    /// locally, as a doorbell byte would. `control` keeps the mapping alive.
    pub fn kicker(&self, control: &std::sync::Arc<Control>) -> Result<Kicker> {
        let bell = self.rx_bell()?;
        Ok(Kicker { word: bell.wake as *const std::sync::atomic::AtomicU32, _control: std::sync::Arc::clone(control) })
    }

    /// Block until this end has something to read, or `timeout` (§103.15).
    ///
    /// [`crate::park::Bell`]'s handshake for a reader whose wait is the
    /// lane's own word: note the wake count, set `parked` so the writer's
    /// next publish rings (and, across a VM, pokes the doorbell, whose reader
    /// thread converts the byte into `ring_local` on this word), look once
    /// more, then sleep. Returns whether there is something to read. Without
    /// a doorbell nothing ends the sleep early, so a caller polling over a
    /// bell-less region passes a short timeout.
    pub fn wait_readable(&self, timeout: std::time::Duration) -> Result<bool> {
        let epoch = self.wake_epoch()?;
        self.wait_readable_since(epoch, timeout)
    }

    /// The lane's wake count now: take it *before* looking at whatever else
    /// a thread waits for (a queue other threads fill and then [`Kicker::kick`]),
    /// and pass it to [`Self::wait_readable_since`], so a kick in between is
    /// not slept through.
    pub fn wake_epoch(&self) -> Result<u32> {
        Ok(self.rx_bell()?.wake.load(std::sync::atomic::Ordering::Acquire))
    }

    /// [`Self::wait_readable`], sleeping only if nothing rang since `epoch`.
    pub fn wait_readable_since(&self, epoch: u32, timeout: std::time::Duration) -> Result<bool> {
        use std::sync::atomic::Ordering;
        if self.readable()? {
            return Ok(true);
        }
        let bell = self.rx_bell()?;
        let seen = epoch;
        bell.parked.fetch_add(1, Ordering::SeqCst);
        if !self.readable()? {
            crate::park::sys::wait(bell.wake, seen, timeout);
        }
        bell.parked.fetch_sub(1, Ordering::SeqCst);
        self.readable()
    }

    /// Bytes of ring space available to [`Self::write`] right now. Diagnostics,
    /// and the number to print when a connection stalls.
    pub fn free_bytes(&self) -> Result<u64> {
        self.tx.free_bytes()
    }

    /// Abandon the slot without an orderly EOF, so the peer can tell the
    /// difference between a closed connection and a dead one.
    pub fn kill(&self, control: &Control) -> Result<()> {
        control.kill_ring(self.slot)
    }
}

enum Taken {
    Eof,
    Direct(usize),
    Held(usize, Vec<u8>),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::control::{Control, Geometry, ProtocolIds};
    use crate::region::SharedRegion;
    use std::sync::Arc;

    /// Both ends of one ring slot over one heap region, which is what a test can
    /// build without a VM and is faithful in every respect that this module
    /// touches: the two `ByteChannel`s share nothing but the bytes.
    fn pair() -> (Control, ByteChannel, ByteChannel) {
        let g = Geometry::small();
        let region = Arc::new(SharedRegion::heap(g.control_size as usize).unwrap());
        let control = Control::create(region, g, ProtocolIds::new(7, 0xfeed_face_dead_beef), 1).unwrap();
        let host = ByteChannel::claim(&control, 7).unwrap();
        let slot = host.slot();
        let guest = ByteChannel::open(&control, slot, Endpoint::Peer).unwrap();
        (control, host, guest)
    }

    fn read_all(ch: &mut ByteChannel) -> (Vec<u8>, bool) {
        let mut out = Vec::new();
        let mut buf = [0u8; 4096];
        loop {
            match ch.read(&mut buf).unwrap() {
                ReadOutcome::Bytes(n) => out.extend_from_slice(&buf[..n]),
                ReadOutcome::Empty => return (out, false),
                ReadOutcome::Eof => return (out, true),
            }
        }
    }

    #[test]
    fn a_byte_stream_survives_the_round_trip_in_both_directions() {
        let (_c, mut host, mut guest) = pair();
        assert_eq!(host.write(b"hello compositor").unwrap(), 16);
        let (got, eof) = read_all(&mut guest);
        assert_eq!(got, b"hello compositor");
        assert!(!eof);

        assert_eq!(guest.write(b"hello client").unwrap(), 12);
        let (got, eof) = read_all(&mut host);
        assert_eq!(got, b"hello client");
        assert!(!eof);
    }

    /// The bug the `FLAG_PAD` field exists to stop. A payload that is not a
    /// multiple of 16 is framed in a packet that is, and without the pad count
    /// the consumer reads the padding as stream content.
    #[test]
    fn an_unaligned_write_does_not_gain_trailing_zeroes() {
        for len in 1..64usize {
            let (_c, mut host, mut guest) = pair();
            let msg: Vec<u8> = (0..len).map(|i| (i as u8) | 0x80).collect();
            assert_eq!(host.write(&msg).unwrap(), len);
            let (got, _) = read_all(&mut guest);
            assert_eq!(got, msg, "length {len} came back a different length");
        }
    }

    /// Wayland messages are 4-byte aligned, never 16, so this is the ordinary
    /// case rather than an edge one: a stream of small messages must reassemble
    /// into exactly the bytes that were written, with no regard for where the
    /// writes were cut.
    #[test]
    fn many_small_writes_reassemble_as_one_stream() {
        let (_c, mut host, mut guest) = pair();
        let mut expect = Vec::new();
        for i in 0..200u32 {
            let msg = [i.to_le_bytes(), (i * 3 + 1).to_le_bytes(), [0xab; 4]].concat();
            assert_eq!(host.write(&msg).unwrap(), msg.len());
            expect.extend_from_slice(&msg);
            // Drain as we go: 200 packets do not fit in a small ring, which is
            // the point — this also exercises the wrap.
            let (got, _) = read_all(&mut guest);
            assert_eq!(got, expect[expect.len() - msg.len()..]);
        }
    }

    #[test]
    fn a_short_read_holds_the_rest_and_returns_it_in_order() {
        let (_c, mut host, mut guest) = pair();
        host.write(b"0123456789").unwrap();
        let mut small = [0u8; 3];
        assert_eq!(guest.read(&mut small).unwrap(), ReadOutcome::Bytes(3));
        assert_eq!(&small, b"012");
        assert!(guest.has_buffered());
        let (rest, _) = read_all(&mut guest);
        assert_eq!(rest, b"3456789");
        assert!(!guest.has_buffered());
    }

    #[test]
    fn eof_is_ordered_behind_the_bytes_before_it() {
        let (_c, mut host, mut guest) = pair();
        host.write(b"last words").unwrap();
        assert!(host.write_eof().unwrap());
        let (got, eof) = read_all(&mut guest);
        assert_eq!(got, b"last words");
        assert!(eof, "the bytes must arrive before the end, not instead of it");
        // And it stays EOF.
        let mut buf = [0u8; 8];
        assert_eq!(guest.read(&mut buf).unwrap(), ReadOutcome::Eof);
    }

    #[test]
    fn eof_is_idempotent_and_writing_after_it_is_an_error() {
        let (_c, mut host, _guest) = pair();
        assert!(host.write_eof().unwrap());
        assert!(host.write_eof().unwrap());
        assert!(host.write(b"x").is_err());
    }

    /// Backpressure, which is the property that lets a slow compositor slow a
    /// fast client down instead of losing its bytes.
    #[test]
    fn a_full_ring_returns_zero_rather_than_an_error_and_recovers() {
        let (_c, mut host, mut guest) = pair();
        let chunk = vec![0x5au8; 4096];
        let mut sent = 0usize;
        loop {
            let n = host.write(&chunk).unwrap();
            if n == 0 {
                break;
            }
            sent += n;
            assert!(sent < 64 * 1024 * 1024, "the ring never filled");
        }
        assert!(sent > 0, "nothing was accepted at all");
        // Draining one packet must make room again.
        let mut buf = vec![0u8; 8192];
        assert!(matches!(guest.read(&mut buf).unwrap(), ReadOutcome::Bytes(_)));
        assert!(host.write(&chunk).unwrap() > 0, "space was not released");
    }

    /// A write larger than a packet is chunked, and the caller is told how much
    /// was taken rather than being given an error it would have to special-case.
    #[test]
    fn a_write_larger_than_a_packet_is_chunked_not_refused() {
        let (_c, mut host, mut guest) = pair();
        let big = vec![0x11u8; host.max_chunk() * 3];
        let mut off = 0;
        let mut guard = 0;
        while off < big.len() {
            let n = host.write(&big[off..]).unwrap();
            if n == 0 {
                let mut buf = vec![0u8; 65536];
                while let ReadOutcome::Bytes(_) = guest.read(&mut buf).unwrap() {}
            }
            off += n;
            guard += 1;
            assert!(guard < 10_000, "made no progress");
        }
        assert_eq!(off, big.len());
    }

    /// The table in this module's header, asserted: a claim is a connect and a
    /// directory poll is an accept.
    #[test]
    fn claiming_a_ring_is_what_makes_a_connection_visible_to_the_peer() {
        let g = Geometry::small();
        let region = Arc::new(SharedRegion::heap(g.control_size as usize).unwrap());
        let control = Control::create(region, g, ProtocolIds::new(7, 0xfeed_face_dead_beef), 1).unwrap();

        assert!(ByteChannel::live_connections(&control).unwrap().is_empty());
        let a = ByteChannel::claim(&control, 0).unwrap();
        assert_eq!(ByteChannel::live_connections(&control).unwrap(), vec![a.slot()]);
        let b = ByteChannel::claim(&control, 1).unwrap();
        let mut live = ByteChannel::live_connections(&control).unwrap();
        live.sort_unstable();
        let mut want = vec![a.slot(), b.slot()];
        want.sort_unstable();
        assert_eq!(live, want, "two connections must be two rings");
        assert_ne!(a.slot(), b.slot(), "two connections sharing a ring is §0.11 again");

        // And a killed one stops being accepted.
        a.kill(&control).unwrap();
        assert_eq!(ByteChannel::live_connections(&control).unwrap(), vec![b.slot()]);
    }

    /// Firefox opens four. The directory must hold them alongside the Vulkan
    /// rings of the same session rather than instead of them.
    #[test]
    fn four_wayland_connections_coexist_with_the_vulkan_rings() {
        // The *live* geometry, not `small()`: the claim this test makes is that
        // sixteen slots hold the reverse ring, Vulkan's three and Firefox's four
        // at the same time, and `small()`'s four slots cannot say that.
        let g = Geometry::for_arena(256 << 20);
        let region = Arc::new(SharedRegion::heap(g.control_size as usize).unwrap());
        let control = Control::create(region, g, ProtocolIds::new(7, 0xfeed_face_dead_beef), 1).unwrap();

        let dev = control.claim_ring(RingKind::Device, 1, None).unwrap();
        let q = control.claim_ring(RingKind::Queue, 2, Some(dev)).unwrap();
        let inst = control.claim_ring(RingKind::Instance, 3, None).unwrap();

        let conns: Vec<ByteChannel> =
            (0..4).map(|i| ByteChannel::claim(&control, i).unwrap()).collect();
        assert_eq!(conns.len(), 4);

        // The Vulkan rings are untouched and still say what they are.
        assert_eq!(control.ring_kind(dev).unwrap(), RingKind::Device);
        assert_eq!(control.ring_kind(q).unwrap(), RingKind::Queue);
        assert_eq!(control.ring_kind(inst).unwrap(), RingKind::Instance);
        assert_eq!(control.ring_depends(q).unwrap(), Some(dev));

        // And the accept loop sees the four Wayland ones and only those.
        assert_eq!(ByteChannel::live_connections(&control).unwrap().len(), 4);

        // Each connection is independent: filling one must not stall another.
        let mut conns = conns;
        let chunk = vec![7u8; 4096];
        while conns[0].write(&chunk).unwrap() != 0 {}
        assert!(conns[1].write(&chunk).unwrap() > 0, "head-of-line blocking between clients");
    }

    /// The `Bytes` kind must not be openable as a command ring by accident.
    #[test]
    fn a_non_wayland_slot_cannot_be_opened_as_a_byte_channel() {
        let g = Geometry::small();
        let region = Arc::new(SharedRegion::heap(g.control_size as usize).unwrap());
        let control = Control::create(region, g, ProtocolIds::new(7, 0xfeed_face_dead_beef), 1).unwrap();
        let dev = control.claim_ring(RingKind::Device, 1, None).unwrap();
        assert!(ByteChannel::open(&control, dev, Endpoint::Peer).is_err());
    }

    /// §14.7, asserted where the number appears. The crossing hop takes bytes
    /// and an offset; there is no parameter through which a descriptor could be
    /// named, so the count is unrepresentable rather than zero.
    #[test]
    fn the_crossing_hop_has_no_parameter_that_could_carry_a_descriptor() {
        let (_c, mut host, mut guest) = pair();
        // The whole API surface that crosses: a slice in, a slice out. If this
        // ever compiles with an fd argument, this test is the thing to change,
        // and changing it is the moment to reopen §14.7.
        let w: fn(&mut ByteChannel, &[u8]) -> Result<usize> = ByteChannel::write;
        let r: fn(&mut ByteChannel, &mut [u8]) -> Result<ReadOutcome> = ByteChannel::read;
        assert_eq!(w(&mut host, b"bytes only").unwrap(), 10);
        let mut buf = [0u8; 16];
        assert_eq!(r(&mut guest, &mut buf).unwrap(), ReadOutcome::Bytes(10));
    }

    /// §89. `readable` is the last look before a reader sleeps: it must see
    /// bytes, a held remainder and the end of the stream, and nothing else.
    #[test]
    fn readable_sees_bytes_held_bytes_and_the_end() {
        let (_c, mut host, mut guest) = pair();
        assert!(!guest.readable().unwrap(), "an empty lane is not readable");
        host.write(b"0123456789").unwrap();
        assert!(guest.readable().unwrap(), "bytes in the lane");
        let mut small = [0u8; 4];
        guest.read(&mut small).unwrap();
        assert!(guest.readable().unwrap(), "bytes held from a short read");
        read_all(&mut guest);
        assert!(!guest.readable().unwrap());
        host.write_eof().unwrap();
        assert!(guest.readable().unwrap(), "the end of the stream is something to read");
        read_all(&mut guest);
        assert!(guest.readable().unwrap(), "and it stays readable: the next read says Eof again");
    }

    /// §89. The doorbell a reader parks on is its *own* lane's, and its token
    /// names the slot and that lane, so the byte the writer sends wakes it.
    #[test]
    fn each_end_parks_on_the_lane_it_reads() {
        let (_c, host, guest) = pair();
        let slot = host.slot();
        assert_eq!(host.rx_bell().unwrap().token, crate::park::lane_token(slot, Lane::Reply));
        assert_eq!(guest.rx_bell().unwrap().token, crate::park::lane_token(slot, Lane::Command));
        host.rx_bell().unwrap().parked.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        assert_eq!(
            guest.tx.ctrl().bell(Lane::Reply).unwrap().parked.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "the writer must see the parked word the reader set"
        );
    }

    /// §89. A connection's slot comes back: claim, talk, end, mark dead,
    /// recycle, and the next claim gets the same slot with nothing of the last
    /// connection in it.
    #[test]
    fn a_dead_slot_is_recycled_clean_and_claimed_again() {
        let g = Geometry::small();
        let region = Arc::new(SharedRegion::heap(g.control_size as usize).unwrap());
        let owner = Control::create(Arc::clone(&region), g, ProtocolIds::new(7, 0xfeed_face_dead_beef), 1).unwrap();
        let peer = Control::attach(region, ProtocolIds::new(7, 0xfeed_face_dead_beef)).unwrap();

        // `peer` plays the proxy (claims), `owner` the compositor (accepts).
        let mut host = ByteChannel::claim(&peer, 1).unwrap();
        let slot = host.slot();
        let mut guest = ByteChannel::open(&owner, slot, Endpoint::Peer).unwrap();
        host.write(b"first connection").unwrap();
        guest.write(b"reply").unwrap();
        host.write_eof().unwrap();
        host.rx_bell().unwrap().parked.fetch_add(1, std::sync::atomic::Ordering::SeqCst);

        // Not dead yet: recycling a live slot is the race the rule forbids.
        assert!(owner.recycle_ring(slot).is_err());
        // And only the owner may do it.
        host.kill(&peer).unwrap();
        assert!(peer.recycle_ring(slot).is_err(), "a peer must not recycle");
        drop(host);
        let (got, eof) = read_all(&mut guest);
        assert_eq!((got.as_slice(), eof), (&b"first connection"[..], true));
        drop(guest);
        owner.recycle_ring(slot).unwrap();
        assert!(owner.recycle_ring(slot).is_err(), "a free slot is not dead");

        let mut host2 = ByteChannel::claim(&peer, 2).unwrap();
        assert_eq!(host2.slot(), slot, "the first free slot is the recycled one");
        let mut guest2 = ByteChannel::open(&owner, slot, Endpoint::Peer).unwrap();
        assert!(!guest2.readable().unwrap(), "the old connection's bytes must be gone");
        assert!(!host2.readable().unwrap(), "and so must the old reply");
        assert_eq!(host2.rx_bell().unwrap().parked.load(std::sync::atomic::Ordering::SeqCst), 0);
        host2.write(b"second").unwrap();
        let (got, eof) = read_all(&mut guest2);
        assert_eq!((got.as_slice(), eof), (&b"second"[..], false));
        guest2.write(b"ok").unwrap();
        let (got, _) = read_all(&mut host2);
        assert_eq!(got, b"ok");
    }

    /// The reverse ring is the session's, never a connection's.
    #[test]
    fn the_reverse_ring_is_never_recycled() {
        let g = Geometry::small();
        let region = Arc::new(SharedRegion::heap(g.control_size as usize).unwrap());
        let owner = Control::create(region, g, ProtocolIds::new(7, 1), 1).unwrap();
        owner.kill_ring(crate::control::REVERSE_RING_SLOT).unwrap();
        assert!(owner.recycle_ring(crate::control::REVERSE_RING_SLOT).is_err());
    }

    /// §89's geometry fits, and every slot can still name itself in a doorbell
    /// byte.
    #[test]
    fn the_wayland_geometry_lays_out_and_every_slot_has_a_token() {
        let g = Geometry::for_wayland();
        let layout = crate::control::Layout::compute(g).unwrap();
        assert!(layout.used <= g.control_size);
        let last = g.ring_slot_count - 1;
        let (s, l) = crate::park::lane_from_token(crate::park::lane_token(last, Lane::Reply));
        assert_eq!((s, l), (last, Lane::Reply), "slot {last} does not survive a doorbell byte");
        let region = Arc::new(SharedRegion::heap(g.control_size as usize).unwrap());
        let control = Control::create(region, g, ProtocolIds::new(7, 1), 1).unwrap();
        let conns: Vec<ByteChannel> =
            (0..g.ring_slot_count - 1).map(|i| ByteChannel::claim(&control, u64::from(i)).unwrap()).collect();
        assert_eq!(conns.len() as u32, g.ring_slot_count - 1, "every slot but the reverse ring");
        assert!(ByteChannel::claim(&control, 99).is_err(), "and then the directory is full");
    }

    /// §103.16: a packet that does not fit before the lane's end makes the
    /// writer publish a pad, and the reader has to consume the pad before
    /// there is room. A reader asleep on the lane must be woken by the pad,
    /// or the writer sees `RingFull` for ever. The first small packet moves
    /// the tail off zero, so a full-size write needs the pad.
    #[test]
    fn a_pad_wakes_a_parked_reader() {
        let g = Geometry::for_wayland();
        let mut mem = vec![0u64; g.control_size as usize / 8];
        // SAFETY: a zeroed, 8-aligned buffer of the geometry's size that
        // outlives both channels.
        let region = Arc::new(unsafe { SharedRegion::from_raw(mem.as_mut_ptr().cast(), g.control_size as usize) });
        let control = Arc::new(Control::create(region, g, ProtocolIds::new(1, 1), 1).unwrap());
        let mut rx = ByteChannel::claim(&control, 9).unwrap();
        let mut tx = ByteChannel::open(&control, rx.slot(), Endpoint::Peer).unwrap();
        assert_eq!(tx.write(&[1; 12]).unwrap(), 12);
        let mut buf = vec![0u8; 1 << 17];
        assert!(matches!(rx.read(&mut buf).unwrap(), ReadOutcome::Bytes(12)));
        let reader = std::thread::spawn(move || {
            let mut got = 0usize;
            let t = std::time::Instant::now();
            while got < 200_000 && t.elapsed() < std::time::Duration::from_secs(20) {
                match rx.read(&mut buf).unwrap() {
                    ReadOutcome::Bytes(n) => got += n,
                    // Long sleeps: only a bell ends them in time.
                    _ => drop(rx.wait_readable(std::time::Duration::from_secs(20))),
                }
            }
            (got, t.elapsed())
        });
        std::thread::sleep(std::time::Duration::from_millis(100));
        let data = vec![7u8; 200_000];
        let mut sent = 0;
        let t = std::time::Instant::now();
        while sent < data.len() {
            match tx.write(&data[sent..]).unwrap() {
                0 => std::thread::sleep(std::time::Duration::from_millis(1)),
                n => sent += n,
            }
            assert!(t.elapsed() < std::time::Duration::from_secs(10), "the writer saw a full ring for ever");
        }
        let (got, took) = reader.join().unwrap();
        assert_eq!(got, 200_000);
        assert!(took < std::time::Duration::from_secs(5), "{took:?}");
        drop(mem);
    }
}

#[cfg(test)]
mod throughput_probe {
    use super::*;
    use crate::control::{Control, Geometry, ProtocolIds};
    use crate::region::SharedRegion;
    use std::sync::Arc;

    #[test]
    #[ignore = "a stopwatch"]
    fn twenty_kib_through_a_wayland_lane() {
        let g = Geometry::for_wayland();
        let region = Arc::new(SharedRegion::heap(g.control_size as usize).unwrap());
        let control = Control::create(region, g, ProtocolIds::new(7, 1), 1).unwrap();
        let mut host = ByteChannel::claim(&control, 7).unwrap();
        let mut guest = ByteChannel::open(&control, host.slot(), Endpoint::Peer).unwrap();
        let payload = vec![7u8; 20_000];
        let mut buf = vec![0u8; 16 * 1024];
        println!("max_chunk {}", host.max_chunk());
        for _ in 0..3 {
            let t0 = std::time::Instant::now();
            for _ in 0..1000 {
                let mut off = 0;
                let mut got = 0;
                while got < payload.len() {
                    if off < payload.len() {
                        off += host.write(&payload[off..]).unwrap();
                    }
                    if let ReadOutcome::Bytes(n) = guest.read(&mut buf).unwrap() {
                        got += n;
                    }
                }
            }
            println!("20 KB: {:?} each", t0.elapsed() / 1000);
        }
    }
}
