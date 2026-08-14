//! The growth channel: how the host asks the guest for another region.
//!
//! GROWTH, and this module is the answer to the one question the rest of the
//! stage could not answer on its own — **which direction, and over what**.
//!
//! # The problem, stated once
//!
//! The side that runs out of room is the host: `wc-proxy`'s allocator is the
//! one that cannot place a 4.6 MB pool. The side that can produce more memory
//! is the guest, and only the guest — §12.2, "the side inside the VM allocates,
//! because only it can produce pages the hypervisor can locate". So growth
//! needs a signal that travels **host to guest**, and every channel this
//! project had ran the other way: the arena handshake is the guest saying a
//! 64-bit run id, and the launcher's ssh channel has the *guest* as the client
//! with the host key behind a forced command.
//!
//! # Why this is not a new channel
//!
//! Because there already is one, it is already shared memory, and it is
//! already the thing this project is built on. `shared_vulkan`'s control region
//! and rings — `sv_transport::control::Control`, `sv_transport::ring` — carry
//! `deqp-vk` and Firefox's GL across this exact boundary today. Waycompose is
//! an addition to shared_vulkan and not a second system beside it, so a growth
//! request is **a message on that channel** rather than a mechanism of its own.
//!
//! Nothing here extends `sv-transport`. `SharedRegion::from_raw` already exists
//! and its doc comment already names this case — "the one caller is the
//! guest-side arena provider, which commits one pinned window covering both
//! regions … slicing that window into a control region and an arena is what
//! this constructor is for". Both sides build a `SharedRegion` over a
//! sub-range of memory they already hold, one calls `Control::create` and the
//! other `Control::attach`, and that is the whole of the bring-up.
//!
//! # Where the control region lives, and why there
//!
//! **The first [`CONTROL_BYTES`] of the compositor's outbound window.** Four
//! properties made that the only candidate that needed nothing new:
//!
//! * It is **already located**. The window is a sub-range of a region the host
//!   swept at start-up, so the control region costs no second locate and no
//!   second run id — which would have meant a second ~3 s sweep on every
//!   session start.
//! * It is **already the compositor's to write**. §12.11 item 3 settled that
//!   these bytes are the guest's; nothing else may allocate in them.
//! * It is **already excluded from the host's pool allocator**, by the
//!   `outbound` line the plan has carried since stage 10. A control region
//!   anywhere else in the arena would need a hole in a free list that is
//!   deliberately a sorted run list with no holes in it.
//! * It is **never reclaimed**. §20's release returns data regions and exempts
//!   the one holding the window, so the channel cannot be unmapped underneath a
//!   request in flight.
//!
//! It costs the gift allocator [`CONTROL_BYTES`] of its window and nothing
//! else. It does not move the floor: the window is a tail of the startup
//! region either way.
//!
//! # The ordering that has to hold, and it is sharper here than anywhere else
//!
//! `Control::create` **zeroes the control region** (`control.rs`,
//! `region.fill(0, control_size, 0)`), and those bytes are stamp pages. This is
//! not a hypothetical: `sv-gpu-server`'s own bring-up records the run it cost —
//! *"located 8192 of 12288 pages — the 8192 arena pages present and all 4096
//! control pages missing. Nothing said why; the count was simply short."*
//!
//! So the compositor may not create the control region until the host's locate
//! has finished. It does not need a new signal for that, because §12.10 already
//! built one for exactly this hazard: [`crate::arena::release_gifts`] is called
//! when the first client is accepted, and a client can only have arrived
//! through a proxy that was started from a plan, and the plan exists only if
//! the sweep succeeded. The channel is created there, on the same event, for
//! the same reason.
//!
//! # The doorbell does not ring across a hypervisor, and that is fine
//!
//! `sv_transport::park` says so itself: two kernels have two futex hashes and a
//! wake on one reaches nobody on the other. What saves it is that the wait is a
//! loop with a finite slice, so a doorbell that reaches nobody costs latency
//! and not correctness — across a VM the futex parker *is* a spin-yield parker
//! with a bounded sleep. D9 chose polling on purpose ("Ship polling … we are
//! not architecting around an interrupt path nobody has measured"), so this is
//! the designed behaviour rather than a degradation discovered here.
//!
//! The compositor's side costs nothing extra for it: the poll loop already
//! turns every 5 ms for the UI, and servicing this is a load of one atomic.

use std::io;

/// The channel's own slice of the outbound window, at its front.
///
/// Sized by [`GEOMETRY`] and stated as a constant because **both sides derive
/// the control region's position from the plan's `outbound` line and this
/// number, and neither is told it**. That is deliberate and it is the same
/// argument the outbound window itself is under: a number typed into two
/// command lines is a number that can differ between them, and the symptom of
/// its differing here is two `Control`s over two different addresses, each
/// waiting for a peer that is writing somewhere else.
pub const CONTROL_BYTES: u64 = 272 * 1024;

/// The channel's geometry — one ring's worth, and no more.
///
/// # Every number here is the smallest that works, and one is not
///
/// `ring_slot_count: 2` rather than 1 because slot 0 is pre-claimed as the
/// reverse ring by `Control`'s own directory writer, so a geometry with one
/// slot has no slot to claim. Growth uses slot 1.
///
/// `ring_data_bytes: 65536` is the one number chosen by a measurement rather
/// than by a minimum, and the measurement is the **frame list**. A grown region
/// answers with the page frame behind every one of its pages, run-length
/// encoded at 16 bytes a run; `VirtualLock`ed 4 KiB pages measured 1471 runs
/// per 64 MiB, so a 16 MiB region is ~368 runs and ~6 KiB. 64 KiB is that with
/// two orders of magnitude of headroom, and a reply that still does not fit is
/// answered without a frame list — the host then sweeps, which costs three
/// seconds and is correct.
///
/// `arena_size: 0` because this control region describes **no arena**. The
/// arena is `wc-proxy`'s, allocated by `wc_proxy::pool`, and the slab directory
/// this geometry would otherwise size is for `sv_arena`'s allocator, which is
/// not the one in play here. Zero is legal and it is the honest value.
///
/// `timeline_count: 0` for the same reason: timeline semaphores are Vulkan's,
/// and there is no `vkQueueSubmit` on this side of the tree.
pub fn geometry() -> sv_transport::control::Geometry {
    sv_transport::control::Geometry {
        control_size: CONTROL_BYTES,
        arena_size: 0,
        // Non-zero is required even with no arena, and is never used.
        arena_slab_size: 4096,
        ring_slot_count: 2,
        ring_data_bytes: 64 << 10,
        timeline_count: 0,
    }
}

/// The ring both sides agree on, by number rather than by claim.
///
/// `Control::claim_ring` exists and works, but it is the wrong shape for a
/// channel with exactly two participants and exactly one conversation: it would
/// make the host discover a slot the guest had already decided, and the
/// discovery could only ever answer 1. Fixed by agreement is the same
/// arrangement the arena id is under.
pub const GROW_RING: u32 = 1;

/// Version and protocol identity, so two builds that disagree refuse each other
/// instead of misreading each other.
///
/// The hash is this module's own name and shape rather than a Vulkan protocol
/// hash: `Control::attach` compares it and fails the handshake on a mismatch,
/// which is what turns "the compositor was rebuilt and the proxy was not" from
/// a wrong answer into a refusal with a name on it.
pub fn protocol_ids() -> sv_transport::control::ProtocolIds {
    sv_transport::control::ProtocolIds::new(1, 0x7743_4752_4f57_0001)
}

/// Which of the two things the host is asking for.
///
/// # Why release needs a message at all, when the host can just unmap
///
/// Because unmapping frees nothing. The pages are the **guest's** — committed
/// and `VirtualLock`ed by the compositor — and the host's view of them is a
/// second mapping of memory the guest is still charged for. Dropping it returns
/// the host's address space and not one byte of the guest's RAM.
///
/// That asymmetry is the whole reason this exists as a message. `sv-guest.exe`
/// holding hundreds of megabytes with no client connected is exactly what it
/// looks like when the host thinks it has released memory and the guest has
/// never been told: `VirtualLock`ed pages are non-pageable, so they are not
/// merely reserved, they are gone from Windows entirely until somebody calls
/// `VirtualFree`. Only the compositor can, and only if it is asked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum Op {
    /// Commit a region of at least `bytes` and register it as `region`.
    Grow = 1,
    /// `VirtualFree` the region registered as `region`. `bytes` is ignored.
    ///
    /// Sent after the host has already stopped listing the region and unmapped
    /// its own view, so nothing can still be naming those bytes. The reply's
    /// `len` is what actually came back.
    Release = 2,
}

impl Op {
    fn from_u32(v: u32) -> Option<Op> {
        match v {
            1 => Some(Op::Grow),
            2 => Some(Op::Release),
            _ => None,
        }
    }
}

/// What the host asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Request {
    pub op: Op,
    /// The id the region must be registered under. Chosen by the host's
    /// allocator (`Arena::grow` passes its `next_id`), because that number is
    /// what every `Descriptor::ArenaSlice { arena, .. }` into the new region
    /// will name, and a guest that chose its own would register memory under a
    /// number no descriptor ever refers to.
    pub region: u32,
    /// At least this many bytes. A pool may not straddle two regions, so a
    /// short answer cannot serve the request that provoked it and the host
    /// checks for that rather than trusting.
    pub bytes: u64,
}

/// What the guest answers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reply {
    /// Zero for success. Non-zero is a refusal the host survives — the guest
    /// can simply be out of RAM — and `message` says which.
    pub status: u32,
    pub run_id: u64,
    pub len: u64,
    pub pages: u32,
    /// False means the pages may move, which would invalidate whatever the host
    /// maps. Reported rather than swallowed, exactly as at start-up.
    pub locked: bool,
    /// Page frame numbers, one per page, in page order. Empty means the host
    /// must sweep — which works, because nothing has overwritten a fresh
    /// region's stamps yet, and merely costs ~3 s instead of 0.064 s.
    pub frames: Vec<u64>,
    /// What the guest's own half cost, in milliseconds, so that a growth's
    /// price can be split between the two sides rather than guessed at.
    pub query_ms: u32,
    /// Free text: the refusal, or the reason there is no frame list. Carried so
    /// that "the sweep ran" has its cause next to it in one log rather than in
    /// two.
    pub message: String,
}

// ---------------------------------------------------------------------------
// The wire, and the one thing about it that is not ours to choose
// ---------------------------------------------------------------------------

/// The command-lane opcode is [`Op`], which is a `#[repr(u32)]` enum so that
/// the wire's `cmd_type` and the thing the handler matches on are one value.

/// `sv_transport::ring`'s consumer cross-checks the packet's framed length
/// against a length **declared inside the payload**, at a fixed offset that
/// differs per lane: byte 8 on the command lane (`sv_wire::CommandHeader`'s
/// `cmd_size`) and byte 4 on the reply lane (`ReplyHeader`'s `reply_size`). It
/// is not configurable and it is not optional — a payload that disagrees is
/// `Error::FramingDisagreement` and the packet is never delivered.
///
/// So these two messages carry `sv-wire`'s headers rather than a private
/// framing of their own. That is a constraint discovered rather than chosen,
/// and it is a good one: it means a growth request is a well-formed command on
/// the same ring as everything else, and a debugger that can read one can read
/// this.
fn put_u32(v: &mut Vec<u8>, n: u32) {
    v.extend_from_slice(&n.to_le_bytes());
}
fn put_u64(v: &mut Vec<u8>, n: u64) {
    v.extend_from_slice(&n.to_le_bytes());
}
fn get_u32(b: &[u8], at: usize) -> io::Result<u32> {
    b.get(at..at + 4)
        .and_then(|s| s.try_into().ok())
        .map(u32::from_le_bytes)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "short growth message"))
}
fn get_u64(b: &[u8], at: usize) -> io::Result<u64> {
    b.get(at..at + 8)
        .and_then(|s| s.try_into().ok())
        .map(u64::from_le_bytes)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "short growth message"))
}

impl Request {
    pub fn encode(&self) -> Vec<u8> {
        let mut v = Vec::with_capacity(24);
        put_u32(&mut v, self.op as u32); // cmd_type
        put_u32(&mut v, 0); // cmd_flags
        put_u32(&mut v, 24); // cmd_size — checked by the ring, see above
        put_u32(&mut v, self.region);
        put_u64(&mut v, self.bytes);
        debug_assert_eq!(v.len(), 24);
        v
    }

    pub fn decode(b: &[u8]) -> io::Result<Request> {
        let raw = get_u32(b, 0)?;
        let op = Op::from_u32(raw).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("growth ring: {raw} is not a request this compositor understands"),
            )
        })?;
        Ok(Request { op, region: get_u32(b, 12)?, bytes: get_u64(b, 16)? })
    }
}

impl Reply {
    /// ```text
    ///   0 cmd_type   4 reply_size  8 status    12 pages
    ///  16 run_id    24 len        32 locked    36 query_ms
    ///  40 msg_len   44 frame_count
    ///  48 message bytes, then frame_count little-endian u64s
    /// ```
    ///
    /// The frame list last and counted rather than delimited, because it is the
    /// only variable-length thing that can be large and because a length is
    /// cheaper to check than a terminator is to scan for.
    pub const FIXED: usize = 48;

    pub fn encode(&self) -> Vec<u8> {
        let msg = self.message.as_bytes();
        let body = Reply::FIXED + msg.len() + self.frames.len() * 8;
        let mut v = Vec::with_capacity(body);
        put_u32(&mut v, Op::Grow as u32); // cmd_type — a reply is always to a request
        put_u32(&mut v, body as u32); // reply_size — checked by the ring
        put_u32(&mut v, self.status);
        put_u32(&mut v, self.pages);
        put_u64(&mut v, self.run_id);
        put_u64(&mut v, self.len);
        put_u32(&mut v, u32::from(self.locked));
        put_u32(&mut v, self.query_ms);
        put_u32(&mut v, msg.len() as u32);
        put_u32(&mut v, self.frames.len() as u32);
        v.extend_from_slice(msg);
        for f in &self.frames {
            put_u64(&mut v, *f);
        }
        debug_assert_eq!(v.len(), body);
        v
    }

    pub fn decode(b: &[u8]) -> io::Result<Reply> {
        let msg_len = get_u32(b, 40)? as usize;
        let frames_len = get_u32(b, 44)? as usize;
        let at = Reply::FIXED;
        let message = b
            .get(at..at + msg_len)
            .map(|s| String::from_utf8_lossy(s).into_owned())
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "short growth reply"))?;
        let mut frames = Vec::with_capacity(frames_len.min(1 << 20));
        for i in 0..frames_len {
            frames.push(get_u64(b, at + msg_len + i * 8)?);
        }
        Ok(Reply {
            status: get_u32(b, 8)?,
            pages: get_u32(b, 12)?,
            run_id: get_u64(b, 16)?,
            len: get_u64(b, 24)?,
            locked: get_u32(b, 32)? != 0,
            query_ms: get_u32(b, 36)?,
            frames,
            message,
        })
    }
}

// ---------------------------------------------------------------------------
// The two ends
// ---------------------------------------------------------------------------

use sv_transport::control::{Control, RingKind};
use sv_transport::ring::{Lane, RingConsumer, RingProducer};

fn other(e: sv_transport::Error) -> io::Error {
    io::Error::other(format!("growth ring: {e}"))
}

/// The host's end: ask, and wait for the answer.
///
/// # Why this blocks, and what it blocks
///
/// It is called from `Arena::grow`, which is called from a pool allocation that
/// cannot be satisfied — so the client whose pool this is, is already stopped.
/// Blocking here does not add a stall, it *is* the stall, and shortening it is
/// what the frame list is for.
///
/// The deadline is mandatory and there is no spelling without one: a growth
/// that waited for ever on a compositor that had died would hang the client's
/// thread inside an allocation, which is the shape of hang nobody can diagnose
/// from the outside. On expiry the caller gets an error, the allocator counts a
/// refusal, and the client survives it — §20 made a refusal survivable and this
/// is one more of them.
pub struct HostEnd {
    to_guest: RingProducer,
    from_guest: RingConsumer,
}

impl HostEnd {
    /// Attach to the channel the compositor created, and claim the growth ring.
    ///
    /// The *peer* claims, which is `sv-transport`'s own arrangement — its test
    /// has the peer call `claim_ring` and the owner read the directory back.
    /// With [`geometry`]'s two slots and slot 0 reserved as the reverse ring,
    /// the claim is deterministic and returns [`GROW_RING`]; it is called rather
    /// than assumed because claiming is what *initialises the directory entry*
    /// the producer and consumer resolve their offsets from.
    pub fn attach(control: &Control) -> io::Result<HostEnd> {
        let slot = control.claim_ring(RingKind::Instance, 0, None).map_err(other)?;
        if slot != GROW_RING {
            return Err(io::Error::other(format!(
                "growth ring: claimed slot {slot} and both sides agree on {GROW_RING}"
            )));
        }
        Ok(HostEnd {
            to_guest: RingProducer::open(control, slot, Lane::Command).map_err(other)?,
            from_guest: RingConsumer::open(control, slot, Lane::Reply).map_err(other)?,
        })
    }

    /// Ask for a region and wait for the answer.
    pub fn request(&mut self, req: Request, deadline: std::time::Duration) -> io::Result<Reply> {
        self.to_guest.push_bytes(&req.encode()).map_err(other)?;
        let until = std::time::Instant::now() + deadline;
        loop {
            if let Some(packet) = self.from_guest.next().map_err(other)? {
                let reply = Reply::decode(packet.payload)?;
                self.from_guest.advance().map_err(other)?;
                return Ok(reply);
            }
            if std::time::Instant::now() >= until {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!(
                        "the compositor did not answer a request for {} bytes within {:?}: \
                         it is wedged, gone, or its poll loop is not servicing the ring",
                        req.bytes, deadline
                    ),
                ));
            }
            // Polling, and `park.rs` explains why a doorbell would not help:
            // a futex wake does not cross a hypervisor. 200 µs rather than the
            // parker's 20 ms slice because this one caller is *already*
            // stopped and its latency is the whole cost being measured.
            std::thread::sleep(std::time::Duration::from_micros(200));
        }
    }
}

/// The guest's end: answer, if anything has been asked.
///
/// Non-blocking by construction, because it is called from the compositor's
/// poll loop and that loop also drives every client's input. A growth that
/// blocked the loop would freeze the desktop to allocate memory for it.
pub struct GuestEnd {
    from_host: RingConsumer,
    to_host: RingProducer,
}

impl GuestEnd {
    /// Open the ring the host claimed. `Ok(None)` while the host has not
    /// claimed one yet, which is every session until the first growth.
    pub fn open(control: &Control) -> io::Result<Option<GuestEnd>> {
        if !matches!(control.ring_state(GROW_RING), Ok(sv_transport::control::RingState::Live)) {
            return Ok(None);
        }
        Ok(Some(GuestEnd {
            from_host: RingConsumer::open(control, GROW_RING, Lane::Command).map_err(other)?,
            to_host: RingProducer::open(control, GROW_RING, Lane::Reply).map_err(other)?,
        }))
    }

    /// Take one request if there is one and answer it with `handler`.
    ///
    /// One per turn rather than draining: a request commits and pins guest RAM
    /// and queries the page-frame database, which is tens of milliseconds, and
    /// doing several in one turn of a 5 ms poll loop would be a visible stall
    /// to pay for memory nobody had asked for twice.
    pub fn service(
        &mut self,
        handler: &mut dyn FnMut(Request) -> Reply,
    ) -> io::Result<Option<(Request, Reply)>> {
        let Some(packet) = self.from_host.next().map_err(other)? else {
            return Ok(None);
        };
        let req = Request::decode(packet.payload)?;
        self.from_host.advance().map_err(other)?;
        let reply = handler(req);
        self.to_host.push_bytes(&reply.encode()).map_err(other)?;
        Ok(Some((req, reply)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The round trip, and the field the ring will read out of the middle of
    /// it. `cmd_size` at byte 8 and `reply_size` at byte 4 are not ours: the
    /// ring's consumer compares them against the framed length and refuses the
    /// packet if they disagree, so a message whose declared length drifts from
    /// its real one is a message that is never delivered — and the symptom is a
    /// growth that times out rather than one that fails.
    #[test]
    fn a_request_declares_its_own_length_where_the_ring_looks_for_it() {
        let r = Request { op: Op::Grow, region: 3, bytes: 16 << 20 };
        let bytes = r.encode();
        assert_eq!(
            u32::from_le_bytes(bytes[8..12].try_into().unwrap()),
            bytes.len() as u32,
            "cmd_size must be the payload's own length"
        );
        assert_eq!(Request::decode(&bytes).unwrap(), r);
    }

    #[test]
    fn a_reply_declares_its_own_length_and_carries_its_frame_list() {
        let r = Reply {
            status: 0,
            run_id: 0xdead_beef_1234_5678,
            len: 16 << 20,
            pages: 4096,
            locked: true,
            frames: vec![0x1a2b3, 0x1a2b4, 0xffff_ffff],
            query_ms: 331,
            message: "no frame list: the query was refused".into(),
        };
        let bytes = r.encode();
        assert_eq!(
            u32::from_le_bytes(bytes[4..8].try_into().unwrap()),
            bytes.len() as u32,
            "reply_size must be the payload's own length"
        );
        assert_eq!(Reply::decode(&bytes).unwrap(), r);
    }

    /// A refusal is a reply, not a missing one. The host survives it — the
    /// guest can simply be out of RAM — and what it must not do is wait for a
    /// second answer that is never coming.
    #[test]
    fn a_refusal_round_trips_with_its_reason_and_no_frames() {
        let r = Reply {
            status: 1,
            run_id: 0,
            len: 0,
            pages: 0,
            locked: false,
            frames: Vec::new(),
            query_ms: 0,
            message: "VirtualAlloc: not enough memory".into(),
        };
        let back = Reply::decode(&r.encode()).unwrap();
        assert_eq!(back, r);
        assert!(back.frames.is_empty());
    }

    /// A frame list for a 16 MiB region has to fit in one ring packet, or the
    /// host sweeps for three seconds instead of translating for 0.064 s. This
    /// is the arithmetic that says it does, at the *worst* case — one run per
    /// page, which is what `VirtualLock`ed 4 KiB pages approach and what the
    /// measured 1471 runs per 64 MiB is a quarter of.
    #[test]
    fn a_sixteen_mib_region_s_frame_list_fits_in_one_packet() {
        let pages = (16 << 20) / 4096;
        let worst = Reply {
            status: 0,
            run_id: 1,
            len: 16 << 20,
            pages,
            locked: true,
            frames: (0..pages as u64).collect(),
            query_ms: 331,
            message: String::new(),
        };
        // `RingProducer::max_payload` is `ring_data_bytes - 16 - 16`.
        let max_payload = geometry().ring_data_bytes as usize - 32;
        assert!(
            worst.encode().len() <= max_payload,
            "a {pages}-page frame list is {} bytes and the ring holds {max_payload}",
            worst.encode().len()
        );
    }

    /// THE TEST THIS MODULE EXISTS FOR: a request crosses the channel and an
    /// answer comes back, over a real `Control` with a real ring.
    ///
    /// A heap region rather than guest RAM, which is `sv-transport`'s own way of
    /// testing this and is honest about what it does and does not prove. It
    /// exercises every line of the framing, the claim, the two lanes and the
    /// poll — the parts that are ours. It cannot exercise the cross-VM race,
    /// which is outside the language anyway (`region.rs` I4), and it cannot
    /// prove the guest commits memory, which is `attach_guest_ram`'s own tests'
    /// job.
    ///
    /// What it does prove is the thing that was wrong twice while writing it:
    /// that the payloads satisfy the ring's internal length cross-check. A
    /// message whose declared length disagrees with its framed length is never
    /// delivered, and the symptom is this test hanging rather than failing —
    /// which is why the deadline is short and the assertion is on the reply.
    #[test]
    fn a_request_crosses_the_ring_and_an_answer_comes_back() {
        use std::sync::Arc;
        use sv_transport::region::SharedRegion;

        let g = geometry();
        let region = Arc::new(SharedRegion::heap(g.control_size as usize).unwrap());
        // The compositor creates; the proxy attaches. Same order as the
        // deployment, and the order matters: `Control::create` zeroes the
        // region, so an attach that raced it would read a half-written header —
        // which is what the READY flag and the last-written magic are for.
        let guest = Control::create(Arc::clone(&region), g, protocol_ids(), 1).unwrap();
        let host = Control::attach(Arc::clone(&region), protocol_ids()).unwrap();

        // Before the host claims, the guest has no ring to service. That is the
        // normal state of a session that never grows, and it must not be an
        // error.
        assert!(GuestEnd::open(&guest).unwrap().is_none(), "no ring before the host claims one");

        let mut host_end = HostEnd::attach(&host).unwrap();
        let mut guest_end = GuestEnd::open(&guest).unwrap().expect("the claim made it Live");

        // The host asks. Nothing answers yet, so this is done in two steps
        // rather than on two threads: the point is the bytes, not the timing.
        host_end.to_guest.push_bytes(&Request { op: Op::Grow, region: 4, bytes: 16 << 20 }.encode()).unwrap();

        let mut seen = None;
        let served = guest_end
            .service(&mut |req| {
                seen = Some(req);
                Reply {
                    status: 0,
                    run_id: 0xfeed_face,
                    len: req.bytes,
                    pages: (req.bytes / 4096) as u32,
                    locked: true,
                    frames: vec![7, 8, 9],
                    query_ms: 331,
                    message: String::new(),
                }
            })
            .unwrap();
        assert!(served.is_some(), "the guest saw the request");
        assert_eq!(seen, Some(Request { op: Op::Grow, region: 4, bytes: 16 << 20 }));

        // And the host reads the answer off the reply lane.
        let reply = host_end.from_guest.next().unwrap().expect("an answer is waiting");
        let reply = Reply::decode(reply.payload).unwrap();
        assert_eq!(reply.run_id, 0xfeed_face);
        assert_eq!(reply.pages, 4096);
        assert_eq!(reply.frames, vec![7, 8, 9]);
        assert!(reply.locked);

        // A second turn with nothing asked is not an error and not a reply.
        assert!(guest_end.service(&mut |_| unreachable!()).unwrap().is_none());
    }

    /// Release is a request like any other, and the opcode is what tells them
    /// apart. It matters that this round-trips rather than being inferred from
    /// `bytes == 0`: the host unmapping its view frees nothing, so a release
    /// that the guest silently read as a grow-of-zero would leave the pages
    /// `VirtualLock`ed and gone from Windows, which is the complaint this exists
    /// to answer.
    #[test]
    fn release_is_its_own_opcode_and_survives_the_wire() {
        let r = Request { op: Op::Release, region: 9, bytes: 0 };
        let back = Request::decode(&r.encode()).unwrap();
        assert_eq!(back, r);
        assert_eq!(back.op, Op::Release);
        // And the two are distinguishable at the first word, which is where the
        // ring's own `cmd_type` lives.
        let grow = Request { op: Op::Grow, region: 9, bytes: 0 }.encode();
        assert_ne!(&grow[0..4], &r.encode()[0..4]);
        // An opcode neither side knows is refused rather than guessed at.
        let mut bad = r.encode();
        bad[0..4].copy_from_slice(&99u32.to_le_bytes());
        assert!(Request::decode(&bad).is_err());
    }

    /// A compositor that never answers must not hang the client's thread inside
    /// an allocation. The deadline is mandatory and this is what it buys: an
    /// error the allocator counts as a refusal, which §20 made survivable.
    #[test]
    fn a_silent_compositor_times_out_rather_than_hanging() {
        use std::sync::Arc;
        use sv_transport::region::SharedRegion;

        let g = geometry();
        let region = Arc::new(SharedRegion::heap(g.control_size as usize).unwrap());
        let _guest = Control::create(Arc::clone(&region), g, protocol_ids(), 1).unwrap();
        let host = Control::attach(Arc::clone(&region), protocol_ids()).unwrap();
        let mut host_end = HostEnd::attach(&host).unwrap();

        let t = std::time::Instant::now();
        let e = host_end
            .request(Request { op: Op::Grow, region: 1, bytes: 4096 }, std::time::Duration::from_millis(50))
            .unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::TimedOut);
        assert!(e.to_string().contains("did not answer"), "{e}");
        assert!(t.elapsed() < std::time::Duration::from_secs(5), "it gave up promptly");
    }

    /// The control region has to fit in the window it is carved from, with
    /// room left for the objects that window exists for. A keymap is ~50 KiB
    /// and the launcher's window is 1 MiB.
    #[test]
    fn the_control_region_leaves_the_gift_allocator_a_window() {
        let g = geometry();
        assert_eq!(g.control_size, CONTROL_BYTES);
        let window = 1024 * 1024u64;
        assert!(CONTROL_BYTES < window / 2, "the channel may not be most of the window");
        assert!(
            window - CONTROL_BYTES > 512 * 1024,
            "what is left has to hold keymaps and clipboard offers"
        );
        // And it must be a whole number of pages, because a `SharedRegion` over
        // a sub-range has to start on one.
        assert_eq!(CONTROL_BYTES % 4096, 0);
    }
}
