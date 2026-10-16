//! The app side of the wire: `sv_icd_entry::Transport` over `sv-transport`.
//!
//! This is the seam design.md called `state::Transport`. Everything above it —
//! all ~239 generated forwarders and every `Icd` method that does not override
//! its default — reaches the GPU side through the two methods below.
//!
//! # Ring choice, and the mutex around it
//!
//! §4.4 gives one instance ring, one device ring per `VkDevice` and one queue
//! ring per `VkQueue`. This build claims one of each, which is what a single
//! device and a single queue need, and routes by the [`RingClass`] the generator
//! derived from each command's first parameter.
//!
//! The device ring takes a mutex, which §4.4 argues for at length: "Vulkan
//! permits two application threads to create objects on one device
//! concurrently … a mutex around a per-device ring producer costs one
//! uncontended atomic in the common case and does not impose ordering on the
//! *queue* rings". Here the mutex does a second job the design does not mention
//! — it makes a reply belong to its command. A ring is SPSC, replies come back
//! in the order the commands went out, and holding the producer across the reply
//! wait means the packet that arrives is *this* call's. The seqno is checked
//! anyway, because "it must be" is not a reason to skip a comparison that costs
//! one branch.
//!
//! # Where the barrier comes from
//!
//! A queue packet that references an object created on the device ring needs a
//! `RingBarrier` or the GPU side may decode it before that object exists (R22).
//! [`sv_transport::ring::BarrierTracker`] answers exactly that question, and the
//! queue ring consults it on every push.

use std::sync::{Arc, Mutex};
use sv_icd_entry::{IcdResult, LowerError, RingClass, Transport};
use sv_transport::control::{Control, Flags};
use sv_transport::park::{Deadline, Futex, wait_until_gone_after};
use sv_transport::region::SharedRegion;
use crate::arena::Backing as _;
use sv_transport::ring::{BarrierTracker, Lane, RingConsumer, RingCtrl, RingProducer};

/// How long a single command may take before the GPU side is presumed gone.
///
/// R4: "timeouts are mandatory on every wait … a wait that expires checks the
/// peer heartbeat and the generation counter before deciding between 'slow' and
/// 'gone'." `wait_until` does that check; this is the budget it does it within.
const CALL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// How often a wait looks at the GPU side's heartbeat.
const LIVENESS_POLL: std::time::Duration = std::time::Duration::from_millis(50);

/// How long the GPU side's heartbeat may be silent before it is presumed gone
/// and the call returns `VK_ERROR_DEVICE_LOST`.
///
/// It was three polls, 150 ms, which is the default `wait_until` builds, and
/// that is a window for two processes on one kernel. The GPU side beats every
/// 20 ms from a thread of its own, but in another VM *all* of its threads stop
/// while the guest is not scheduled, and this side keeps running, so its stall
/// credit sees nothing to forgive. A 400 ms stop of the GPU side was enough to
/// lose Firefox's device (waycompose-design.md §95), and on the guest the iGPU
/// coming out of D3 and a host with 244 MB free are the likely stops. Five
/// seconds is what the GPU side allows the app side (`APP_GONE_AFTER`): long
/// enough that only a stopped or exited peer is silent that long, and still
/// inside [`CALL_TIMEOUT`].
///
/// A test build waits 400 ms instead, so that `abandoned_calls` can outlast it.
const PEER_GONE_AFTER: std::time::Duration = if cfg!(test) {
    std::time::Duration::from_millis(400)
} else {
    std::time::Duration::from_secs(5)
};

/// Every wait this transport makes on the GPU side: [`LIVENESS_POLL`] and
/// [`PEER_GONE_AFTER`], so that no site can pick its own window again.
fn wait_for_peer(
    control: &Control,
    ctrl: &RingCtrl,
    lane: Option<Lane>,
    parker: &dyn sv_transport::park::Parker,
    ready: &dyn Fn() -> bool,
    overall: Deadline,
) -> sv_transport::Result<()> {
    wait_until_gone_after(control, ctrl, lane, parker, ready, overall, LIVENESS_POLL, PEER_GONE_AFTER)
}

/// How much of the arena is set aside for command blobs.
///
/// An eighth, between 4 MiB and 64 MiB. There is no measurement behind the
/// fraction and it should not pretend otherwise. What is behind the bounds
/// changed in docs/design.md §0.28: a block no longer holds a whole recording,
/// only what the GPU side has not read yet (see
/// [`RingTransport::grow_and_write`]), so the reserve is a buffer between the
/// recording threads and the GPU side rather than a store of recordings. Too
/// small now costs waiting — a recording that cannot get a block reclaims and
/// waits for the GPU side — and not the device.
fn blob_reserve(arena_len: u64) -> u64 {
    let want = (arena_len / 8).clamp(4 << 20, 64 << 20);
    // Never take more than half the arena, however small it is: a 4 MiB arena
    // with a 4 MiB reserve is an arena an application cannot allocate from.
    want.min(arena_len / 2) & !(crate::arena::GRANULARITY - 1)
}

/// One command buffer's recording, as docs/design.md §6.5 describes it.
///
/// > Each `VkCommandBuffer` owns an arena block; recording encodes straight into
/// > it; `vkEndCommandBuffer` publishes `(offset, len)`.
#[derive(Debug, Clone, Copy)]
struct Blob {
    /// Arena offset of the block.
    off: u64,
    /// Bytes of it that hold recorded commands.
    len: u64,
    /// Bytes of it that have already been handed to the GPU side.
    ///
    /// # Why a recording is streamed rather than sent whole
    ///
    /// §6.5 sends one blob per command buffer, at submit. Measured, that is a
    /// *loss* on a same-machine boundary and the reason is not obvious: with one
    /// packet per `vkCmd*` the GPU side decodes command `i` while the
    /// application encodes command `i + 1`, so half the work is free. A single
    /// blob at the end throws that away — the app records all 200 commands
    /// alone, then the GPU side decodes all 200 alone — and `frame-200` went
    /// from 105 µs to 130 µs on exactly that.
    ///
    /// So the block is published in pieces as it fills, and the last piece goes
    /// at `vkEndCommandBuffer`. The bytes never move and nothing is copied
    /// twice: a piece is `(off + published, len - published)` of the same
    /// append-only block, and `published` only ever advances. What §6.5 wanted —
    /// a recording that costs one packet rather than N — is kept to within a
    /// small constant, and the pipelining that N packets bought is kept too.
    ///
    /// It is safe against the driver because a `vkCmd*` names its command buffer
    /// as a parameter: the GPU side may interleave pieces of two recordings and
    /// the driver cannot tell, which is the same reason Vulkan lets an
    /// application record two command buffers at once.
    published: u64,
    /// Bytes of it that exist.
    cap: u64,
    /// The seqno of the `svExecuteCommandBlob` that last named this block.
    ///
    /// **This is docs/design.md §5.5's read-pending word, and it did not need to be a
    /// word.** §5.5 asks for "an 8-byte handshake word the GPU side flips from
    /// `READ_PENDING` to `READ_COMPLETE`, so the app side never recycles a blob
    /// the decoder is still walking". The ring already publishes exactly that
    /// fact: `completed_seqno` is stored by `RingConsumer::advance`, which runs
    /// after the packet has been *processed*, so `completed >= this` is
    /// precisely "the sub-decoder has finished with these bytes". A second
    /// mechanism would be a second thing to get wrong, and it would need the
    /// GPU side to write into a blob it otherwise only reads.
    in_flight: u64,
}

/// A hasher for object ids, which are not adversarial and are not random.
///
/// The default `HashMap` hasher is SipHash-1-3, chosen to make hash-flooding
/// impossible from untrusted keys. These keys are ours: `State::mint` hands them
/// out from a counter starting at 1 (docs/design.md §6.4), so they are dense small
/// integers that this process chose. SipHash on the recording path measured at
/// roughly a fifth of what a whole `vkCmd*` costs, for a property nothing here
/// needs — the multiply-shift below spreads a dense counter perfectly and is one
/// instruction.
///
/// This is confined to the blob table. It is deliberately *not* offered to
/// `State`'s maps: they are not on a per-draw-call path, and a fast hasher
/// spread across a crate is how one ends up hashing something that did come from
/// outside.
#[derive(Default, Clone, Copy)]
struct IdHasher(u64);

impl std::hash::Hasher for IdHasher {
    fn finish(&self) -> u64 {
        self.0
    }
    fn write(&mut self, bytes: &[u8]) {
        // Only ever called for `u64` keys; a byte fallback is here so the impl
        // is total rather than panicking if the key type ever changes.
        for b in bytes {
            self.0 = (self.0 ^ u64::from(*b)).wrapping_mul(0x0100_0000_01b3);
        }
    }
    fn write_u64(&mut self, v: u64) {
        // Fibonacci hashing: multiply by 2^64/φ and let `HashMap` take the high
        // bits it wants. Dense inputs come out well spread.
        self.0 = v.wrapping_mul(0x9e37_79b9_7f4a_7c15);
    }
}

#[derive(Default, Clone, Copy)]
struct IdHash;

impl std::hash::BuildHasher for IdHash {
    type Hasher = IdHasher;
    fn build_hasher(&self) -> IdHasher {
        IdHasher(0)
    }
}

/// Every open recording, and the allocator the blocks come from.
struct Blobs {
    /// First-fit over the reserve; blocks are long-lived and few, so the
    /// simplest allocator that coalesces is the right one.
    alloc: crate::arena::Placeholder,
    /// Where the reserve starts in the arena. `Placeholder` counts from zero.
    base: u64,
    open: std::collections::HashMap<u64, Blob, IdHash>,
    /// Blocks a recording has moved out of while the GPU side was still
    /// reading their last piece, each with the seqno that piece went out
    /// under. Given back by [`Blobs::retire_read`] once `completed_seqno`
    /// passes it. See [`RingTransport::grow_and_write`].
    retiring: Vec<(Blob, u64)>,
    /// Bytes of the reserve held by blocks, open or retiring, and the
    /// high-water mark `SV_DEBUG` last reported (docs/design.md §0.28).
    held: u64,
    reported: u64,
}

impl Blobs {
    /// One line under `SV_DEBUG` each time the reserve's high-water mark
    /// passes another MiB: how many blocks, how big the largest, and how much
    /// of their bytes has not even been sent. That is the measurement that
    /// told retention (blocks the size of their whole recording, although
    /// every piece but the last has long been read) from a working set.
    fn note_high_water(&mut self) {
        if self.held < self.reported + (1 << 20) {
            return;
        }
        self.reported = self.held;
        let largest = self.open.values().map(|b| b.cap).max().unwrap_or(0);
        let live: u64 = self.open.values().map(|b| b.len - b.published).sum();
        sv_icd_entry::report_transport_failure(&format_args!(
            "blob reserve: {} KiB held in {} block(s) and {} retiring, largest {} KiB, {} KiB not yet sent",
            self.held >> 10,
            self.open.len(),
            self.retiring.len(),
            largest >> 10,
            live >> 10
        ));
    }

    /// Give back every retiring block whose last piece the GPU side has read.
    fn retire_read(&mut self, completed: u64) {
        let mut i = 0;
        while i < self.retiring.len() {
            if self.retiring[i].1 <= completed {
                let (b, _) = self.retiring.swap_remove(i);
                RingTransport::release(self, b);
            } else {
                i += 1;
            }
        }
    }

    /// Bytes a block can still hand out, or `None` with no block to write to.
    fn alloc_block(&mut self, want: u64) -> Option<(u64, u64)> {
        let a = self.alloc.alloc(want).ok()?;
        let off = self.base + self.alloc.arena_offset(a).ok()?;
        self.held += a.size;
        Some((off, a.size))
    }
}

/// The block a recording starts with. One 4 KiB page would be a growth on the
/// second `vkCmd*` of every frame; 64 KiB holds a thousand of them and is
/// reached in one allocation.
const BLOB_INITIAL: u64 = 64 << 10;

/// How many unsent bytes a recording accumulates before a piece is handed over.
///
/// The trade is one ring packet against how far ahead the GPU side can be. Too
/// large and a recording is again one lump the GPU side only starts on when the
/// application has finished — which is what §6.5 asks for and what measured
/// worse than the per-command packets it replaces. Too small and the packet
/// count creeps back towards one per `vkCmd*`.
///
/// 8 KiB is a hundred-odd commands: enough that the framing is negligible,
/// small enough that the GPU side starts decoding while a real frame is still
/// being recorded.
const BLOB_FLUSH: u64 = 8 << 10;

/// How many times a recording that cannot get a block reclaims the reserve
/// before it reports `VK_ERROR_OUT_OF_DEVICE_MEMORY`. More than one because
/// another thread can take what a reclaim freed before this one asks again.
const RECLAIM_ROUNDS: u32 = 3;

impl Blobs {
    fn new(base: u64, reserve: u64) -> Blobs {
        Blobs {
            // One slab, because the reserve is not the application's arena and
            // nothing imports it separately: it lives inside whatever slabs the
            // GPU side has, and `Placeholder`'s slab rule would only fragment
            // it.
            alloc: crate::arena::Placeholder::new(reserve.max(crate::arena::GRANULARITY), reserve.max(crate::arena::GRANULARITY))
                .expect("the blob reserve is a whole number of pages"),
            base,
            open: std::collections::HashMap::default(),
            retiring: Vec::new(),
            held: 0,
            reported: 0,
        }
    }
}

/// One ring, and everything needed to use it from several threads.
struct Ring {
    producer: RingProducer,
    replies: RingConsumer,
    ctrl: RingCtrl,
}

/// Cross-ring ordering, in **both** directions.
///
/// # The correction to docs/design.md §4.4
///
/// §4.4 describes one direction: "an object created on the device ring may not
/// yet exist on the GPU side when a queue ring references it", and the app side
/// "emits one whenever a packet references an id created on another ring". As
/// built that was a single [`BarrierTracker`] on the queue ring, and it was
/// sufficient for exactly as long as `vkQueueSubmit` was **synchronous**: a
/// blocking submit is itself a barrier, because the app side does not issue
/// anything else until the GPU side has answered.
///
/// Making submit asynchronous — which §4.4's own D15 calls non-negotiable —
/// removes that accidental ordering and the hazard appears in the other
/// direction. `vkDeviceWaitIdle` is a *device*-ring command by the rule that
/// picks a ring from the first parameter, so
///
/// ```text
///     vkQueueSubmit(queue, …)   → queue ring, no reply
///     vkDeviceWaitIdle(device)  → device ring, round trip
/// ```
///
/// lets the device thread call the driver's `vkDeviceWaitIdle` **before** the
/// queue thread has submitted anything, and it returns at once having waited for
/// nothing. The application is then told its work is complete when it has not
/// started. The same shape reaches `vkWaitForFences` with a finite timeout.
///
/// So there are two trackers, and the rule is symmetric: *a packet on either
/// ring carries a barrier past everything produced on the other since the last
/// one.*
///
/// # Why they live in one mutex of their own
///
/// The old comment here read "lock order is device → queue and never the
/// reverse", which held because only the queue ring consulted a tracker. A
/// second tracker in the other direction makes that a cycle immediately. One
/// mutex holding both, always taken *inside* a ring lock and never containing
/// one, has no order to get wrong.
///
/// # No deadlock, and the argument is short
///
/// Suppose the GPU side's device thread is blocked before packet `D`, waiting
/// for queue seqno `Q`. `D` names `Q`, so the app produced `Q` first; `Q`'s own
/// barrier, if it has one, names some device seqno `D'` produced before `Q` and
/// therefore before `D`. The device ring is FIFO, so `D'` was advanced before
/// the thread reached `D`. `Q` is satisfiable, the queue thread runs it, and
/// `D` proceeds.
struct Barriers {
    /// Attached to queue packets; names device seqnos.
    to_queue: BarrierTracker,
    /// Attached to device packets; names queue seqnos.
    to_device: BarrierTracker,
}

/// A connected app side.
pub struct RingTransport {
    control: Arc<Control>,
    instance: Mutex<Ring>,
    device: Mutex<Ring>,
    queue: Mutex<Ring>,
    /// Cross-ring ordering. Always taken *inside* a ring lock, never outside.
    barriers: Mutex<Barriers>,
    /// Serialises `svImportArenaSlab`, so two threads allocating in one slab
    /// send one import rather than two.
    importing: Mutex<()>,
    /// Serialises [`RingTransport::flush_blob`], so a range of a recording is
    /// published exactly once and pieces are pushed in the order they were
    /// recorded.
    ///
    /// # The bug this is the whole of
    ///
    /// `flush_blob` read `(off + published, len - published)` under the blob
    /// lock, **released it**, pushed the packet, then re-took the lock to
    /// advance `published`. Two threads entering that window with the same
    /// `published` compute the *same* range, push it *twice*, and both then
    /// store the same new watermark — so the GPU side replays one piece of a
    /// recording twice and neither side can tell.
    ///
    /// It needs no illegal application: a `vkCmd*` is buffered and a
    /// `vkQueueSubmit` is not, so [`RingTransport::publish_recorded_first`]
    /// flushes **every** open blob — including one that another thread is
    /// legitimately ending at that moment. Zink submits on its own
    /// `util_queue` thread while the driver thread records the next frame, so
    /// `gl-probe` hit it about one run in ten.
    ///
    /// What it cost when the duplicated piece was the tail of a recording: the
    /// piece carries `vkEndCommandBuffer`, RADV finalises an already-finalised
    /// command stream, `radv_amdgpu_cs_finalize` dereferences the `ib_buffer`
    /// it nulled the first time, and `sv-gpu-server` takes a SIGSEGV with a
    /// backtrace pointing at the driver.
    ///
    /// A lock and not a compare-and-swap on `published`, because *ordering* is
    /// half of what is being restored: claiming the range atomically stops the
    /// double send, and two threads could still push their pieces to the ring
    /// out of order, which replays a recording's commands out of order. One
    /// mutex held across the claim and the push settles both. It is taken once
    /// per piece — once per [`BLOB_FLUSH`] of recording, or once at
    /// `vkEndCommandBuffer` — and never on the `vkCmd*` path.
    publishing: Mutex<()>,
    parker: Futex,
    /// docs/design.md §6.5's per-command-buffer arena blocks.
    blobs: Mutex<Blobs>,
    /// The arena, mapped. Held here because the mapping must outlive every
    /// pointer `vkMapMemory` handed out, and this is the only thing that lives
    /// as long as the process.
    pub arena: Arc<SharedRegion>,
    /// Clearing this stops the heartbeat thread.
    beating: Arc<std::sync::atomic::AtomicBool>,
    /// The cross-VM doorbell reader thread (§36.6). Held so it lives as long as
    /// the transport that depends on it; dropping it stops answering the door.
    _doorman: Option<sv_transport::vdoorbell::Doorman>,
}

impl Drop for RingTransport {
    fn drop(&mut self) {
        self.beating.store(false, std::sync::atomic::Ordering::Relaxed);
        report_park_time();
    }
}

/// docs/design.md §4.2: "monotonic tick, updated at least every 100 ms."
///
/// # Why this is a thread and not a call on the wait path
///
/// [`sv_transport::park::wait_until`] beats before every park, and until this
/// existed that was the *only* place either side beat. That is enough exactly
/// while a side is waiting and wrong the moment it is working: an application
/// between two Vulkan calls is not inside `wait_until`, so its heartbeat stops,
/// and the GPU side — whose liveness window is a small multiple of its park
/// slice — concludes it has died. `vulkaninfo` did precisely that, spending long
/// enough formatting its output between calls that the GPU side's ring threads
/// exited underneath it and the next call came back `VK_ERROR_DEVICE_LOST`.
///
/// The heartbeat has to be independent of what the side is doing, because that
/// is what it *means*. R4's distinction between "slow" and "gone" is not
/// answerable by a signal only the not-busy send.
#[cfg(any(target_os = "linux", windows))]
const BEAT_INTERVAL: std::time::Duration = std::time::Duration::from_millis(20);

/// What the app side learned when it attached.
pub struct Attached {
    pub transport: RingTransport,
    pub arena_len: u64,
    pub slab_size: u64,
}

/// A transport failure as a `LowerError`.
///
/// `LowerError` is `Copy` and carries no string, so the transport's own message
/// — which is the one that says *what* went wrong — would be lost here. It is
/// printed on the loader's diagnostic channel instead. Without this the app side
/// reports `NotConnected` for every ring problem, which is true and useless:
/// the first run of the end-to-end test said exactly that and the real answer
/// was a packet larger than the ring.
///
/// **The first of each kind is said unconditionally** (waycompose-design.md
/// §95). A Firefox on the deployed install died twice with `zink: DEVICE
/// LOST!` and nothing from either side to say whether the GPU side's driver
/// had lost its device or this transport had declared the peer gone; the
/// reason existed only here, behind `SV_DEBUG`, which nobody runs a desktop
/// session with. Once per kind, because after the first every outstanding
/// call reports the same thing.
fn lower(e: sv_transport::Error) -> LowerError {
    use std::sync::atomic::AtomicBool;
    static LOST_SAID: AtomicBool = AtomicBool::new(false);
    static OTHER_SAID: AtomicBool = AtomicBool::new(false);
    sv_icd_entry::report_transport_failure(&e);
    match e {
        // §4.3: "the app side turns that into `VK_ERROR_DEVICE_LOST` on every
        // outstanding call, which is exactly what the specification provides
        // for."
        sv_transport::Error::PeerRestarted { .. }
        | sv_transport::Error::PeerGone { .. }
        | sv_transport::Error::TimedOut { .. }
        | sv_transport::Error::RingFatal { .. } => {
            crate::state::say_once(
                &LOST_SAID,
                &format_args!(
                    "transport: {e}; returning VK_ERROR_DEVICE_LOST (the GPU side is \
                     taken to be gone, not its driver's device)"
                ),
            );
            LowerError::DeviceLost
        }
        _ => {
            crate::state::say_once(
                &OTHER_SAID,
                &format_args!("transport: {e}; returning VK_ERROR_INITIALIZATION_FAILED"),
            );
            LowerError::NotConnected
        }
    }
}

impl RingTransport {
    /// Connect to whichever side is serving the cold path, attach, claim rings.
    ///
    /// **The handoff is a name** (docs/design.md §1a): the `Attachment` says what to
    /// open and this process opens it, so nothing crosses the socket but text.
    /// `sv-hostd` names one object holding both regions at different offsets;
    /// its `Mode::Qemu` names QEMU's memory backend and sends the run list after
    /// it, because a scattered extent needs one `mmap` per run.
    ///
    /// The one exception is `sv-gpu-server serve`, which leaves
    /// `arena_object` empty because a dma-buf has no name, and sends a
    /// descriptor. That path is same-machine-only by construction and cannot
    /// exist across a hypervisor; `sv_transport::localfd` documents why it is
    /// still here. Every other topology takes the name.
    #[cfg(target_os = "linux")]
    pub fn connect(socket: &str) -> Result<Attached, String> {
        use std::os::fd::AsFd;
        use std::os::unix::net::UnixStream;
        use sv_transport::cold::{ColdMessage, send};

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let sock = loop {
            match UnixStream::connect(socket) {
                Ok(s) => break s,
                Err(_) if std::time::Instant::now() < deadline => {
                    std::thread::sleep(std::time::Duration::from_millis(20))
                }
                Err(e) => return Err(format!("cannot reach {socket}: {e}")),
            }
        };
        send(&sock, &ColdMessage::AttachRequest).map_err(|e| e.to_string())?;
        // The nameless-arena provider sends its dma-buf with the message, so the
        // read has to be able to collect one. Every other provider sends none,
        // and `fds` is then empty.
        let mut buf = Vec::new();
        let fds =
            sv_transport::localfd::recv_with_fds(&sock, &mut buf).map_err(|e| e.to_string())?;
        let at = match ColdMessage::decode(&buf).map_err(|e| e.to_string())? {
            ColdMessage::Attach(a) => a,
            ColdMessage::Refuse { code, reason } => {
                return Err(format!("refused {code}: {reason}"));
            }
            other => return Err(format!("unexpected cold-path message {other:?}")),
        };

        // Open the control region by name. Always: even the quarantined provider
        // names it, which is what reduced that path to one nameless object.
        let control_file =
            sv_transport::region::open_named(&at.object).map_err(|e| e.to_string())?;

        // A non-zero `run_count` means the extent is scattered through the
        // object and the daemon is sending the runs next. That is the
        // `Mode::Qemu` path: the object is QEMU's memory backend, the pages are
        // guest RAM, and a plain `map_fd` would map the wrong 64 MiB of a
        // thirteen-gigabyte file. §3: `sv-hostd` "hands **name + runs** to the
        // ICDs".
        let (control_region, arena) = if at.run_count != 0 {
            let mut buf = Vec::new();
            sv_transport::cold::recv_bytes(&sock, &mut buf).map_err(|e| e.to_string())?;
            let runs = match ColdMessage::decode(&buf).map_err(|e| e.to_string())? {
                ColdMessage::Runs(r) => r,
                other => return Err(format!("expected the run list, got {other:?}")),
            };
            if runs.len() as u64 != at.run_count {
                return Err(format!(
                    "the daemon said {} runs and sent {}",
                    at.run_count,
                    runs.len()
                ));
            }
            RingTransport::stitch_two(control_file.as_fd(), &at, &runs)?
        } else {
            let control = Arc::new(
                SharedRegion::map_fd(control_file.as_fd(), at.control_offset, at.control_len as usize)
                    .map_err(|e| e.to_string())?,
            );
            let arena = if at.arena_object.is_empty() {
                // The one nameless object in the project: an exported dma-buf.
                let fd = fds
                    .first()
                    .ok_or("the peer named no arena and sent no descriptor for one")?;
                Arc::new(
                    SharedRegion::map_fd(fd.as_fd(), at.arena_offset, at.arena_len as usize)
                        .map_err(|e| e.to_string())?,
                )
            } else if at.arena_object == at.object {
                Arc::new(
                    SharedRegion::map_fd(control_file.as_fd(), at.arena_offset, at.arena_len as usize)
                        .map_err(|e| e.to_string())?,
                )
            } else {
                let f = sv_transport::region::open_named(&at.arena_object)
                    .map_err(|e| e.to_string())?;
                Arc::new(
                    SharedRegion::map_fd(f.as_fd(), at.arena_offset, at.arena_len as usize)
                        .map_err(|e| e.to_string())?,
                )
            };
            (control, arena)
        };
        // Acknowledge, so a daemon that created the name may release it. Sent
        // after both mappings exist, which is what makes it mean anything.
        let _ = send(&sock, &ColdMessage::Accept { generation: at.generation });
        RingTransport::finish_attach(control_region, arena, at.arena_len)
    }

    /// Connect to a Windows GPU side over a named pipe and two named sections.
    ///
    /// The counterpart of the Linux `connect`, and shorter, because **nothing is
    /// handed over**. The section names are derived from the session string on
    /// both sides ([`sv_transport::winshm::section_name`]), so the cold path
    /// carries geometry and no handle — no `DuplicateHandle`, and therefore no
    /// `PROCESS_DUP_HANDLE` right for the GPU side over this process.
    ///
    /// Everything after the two mappings is [`RingTransport::finish_attach`],
    /// shared with the Linux path. That sharing is the claim this whole axis
    /// exists to make: the platform layer differs and the transport does not.
    #[cfg(windows)]
    pub fn connect(session: &str) -> Result<Attached, String> {
        use sv_transport::cold::ColdMessage;
        use sv_transport::stream::{read_message, write_message};
        use sv_transport::{winpipe, winshm};

        let mut pipe = winpipe::Pipe::connect(&winpipe::pipe_name(session), 10_000)
            .map_err(|e| format!("cannot reach the GPU side's pipe: {e}"))?;
        write_message(&mut pipe, &ColdMessage::AttachRequest).map_err(|e| e.to_string())?;
        let mut scratch = Vec::new();
        let at = match read_message(&mut pipe, &mut scratch).map_err(|e| e.to_string())? {
            ColdMessage::Attach(a) => a,
            ColdMessage::Refuse { code, reason } => {
                return Err(format!("refused {code}: {reason}"));
            }
            other => return Err(format!("unexpected cold-path message {other:?}")),
        };

        let control_region = Arc::new(
            SharedRegion::map_section(
                winshm::Section::open(
                    &winshm::section_name(&format!("{session}.ctl")),
                    at.control_len,
                )
                .map_err(|e| e.to_string())?,
            )
            .map_err(|e| e.to_string())?,
        );
        let arena = Arc::new(
            SharedRegion::map_section(
                winshm::Section::open(
                    &winshm::section_name(&format!("{session}.arena")),
                    at.arena_len,
                )
                .map_err(|e| e.to_string())?,
            )
            .map_err(|e| e.to_string())?,
        );
        // The pipe is dropped here, and that is deliberate: D10 says the cold
        // path is "handshake, capability negotiation, wakeups, teardown, never
        // data", and this build has no `SocketDoorbell` yet, so holding it open
        // would only be a handle nobody reads.
        RingTransport::finish_attach(control_region, arena, at.arena_len)
    }

    /// Stitch the two regions out of one descriptor and one run list.
    ///
    /// The extent covers both regions and the runs describe the whole of it, so
    /// the control region and the arena are two *slices* of one stitched view.
    /// They are built as two independent stitches rather than one view sliced in
    /// two, because §4.1 wants two independent mappings with independent bases —
    /// and because slicing a mapping would mean one `munmap` releasing both,
    /// which is not what `Arc<SharedRegion>` promises.
    #[cfg(target_os = "linux")]
    fn stitch_two(
        fd: std::os::fd::BorrowedFd<'_>,
        at: &sv_transport::cold::Attachment,
        runs: &[sv_transport::cold::AttachRun],
    ) -> Result<(Arc<SharedRegion>, Arc<SharedRegion>), String> {
        use sv_transport::region::StitchRun;

        // Cut the run list at `arena_offset`, splitting a run if the boundary
        // falls inside one. The control region is a whole number of pages and
        // the runs are page-granular, so the split is always legal — but it does
        // happen, because nothing makes a guest's fragmentation line up with our
        // geometry.
        let split = at.arena_offset;
        let mut control = Vec::new();
        let mut arena = Vec::new();
        for r in runs {
            let end = r.extent_offset + r.len;
            if end <= split {
                control.push(StitchRun {
                    file_offset: r.file_offset,
                    extent_offset: r.extent_offset,
                    len: r.len,
                });
            } else if r.extent_offset >= split {
                arena.push(StitchRun {
                    file_offset: r.file_offset,
                    extent_offset: r.extent_offset - split,
                    len: r.len,
                });
            } else {
                let head = split - r.extent_offset;
                control.push(StitchRun {
                    file_offset: r.file_offset,
                    extent_offset: r.extent_offset,
                    len: head,
                });
                arena.push(StitchRun {
                    file_offset: r.file_offset + head,
                    extent_offset: 0,
                    len: r.len - head,
                });
            }
        }
        let control = SharedRegion::stitch(fd, &control).map_err(|e| e.to_string())?;
        let arena = SharedRegion::stitch(fd, &arena).map_err(|e| e.to_string())?;
        if control.len() as u64 != at.control_len || arena.len() as u64 != at.arena_len {
            return Err(format!(
                "the runs cover {}+{} bytes and the attachment says {}+{}",
                control.len(),
                arena.len(),
                at.control_len,
                at.arena_len
            ));
        }
        Ok((Arc::new(control), Arc::new(arena)))
    }

    /// The half of attaching that is the same on every platform: wait for the
    /// owner to publish, claim the three rings, start the heartbeat.
    ///
    /// Extracted when the Windows path was written, because it is 60 lines and
    /// duplicating it is how the two platforms come to claim rings in a
    /// different order.
    #[cfg(any(target_os = "linux", windows))]
    fn finish_attach(
        control_region: Arc<SharedRegion>,
        arena: Arc<SharedRegion>,
        attached_arena_len: u64,
    ) -> Result<Attached, String> {
        use sv_transport::control::RingKind;
        use sv_transport::ring::Lane;

        // The owner may not have published yet. `NotReady` is the expected
        // answer and retrying it is the whole reason `magic` is written last.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let control = loop {
            match Control::attach(Arc::clone(&control_region), crate::protocol_ids()) {
                Ok(c) => break c,
                Err(sv_transport::Error::NotReady)
                    if std::time::Instant::now() < deadline =>
                {
                    std::thread::sleep(std::time::Duration::from_millis(5))
                }
                // R13's detector. Say what to do about it, because "protocol
                // hash mismatch" without the next step wastes an afternoon.
                Err(e @ sv_transport::Error::ProtocolHashMismatch { .. }) => {
                    return Err(format!(
                        "{e}\nThe two sides were generated from different registries or \
                         different generators. Dump sv_proto::PROTOCOL_MANIFEST on both \
                         and diff it."
                    ));
                }
                // The coarse check in front of the hash, and the one that fires
                // on a **stale deployed binary** rather than on a code change.
                // This project has lost time to that twice, both times reading
                // the symptom as a transport fault, so the next step is spelled
                // out here rather than left to be inferred from two numbers.
                //
                // `WIRE_VERSION` went 1 → 2 for docs/design.md §0.18's
                // `svDriverVersions`, which is hand-written in `sv_proto::sv`
                // and therefore outside the generated table the protocol hash
                // covers. A GPU side copied to the guest before that commit
                // cannot answer it, and the honest thing is to refuse here
                // rather than to discover it on the first version query.
                Err(e @ sv_transport::Error::WireVersionMismatch { .. }) => {
                    return Err(format!(
                        "{e}\nThis is a version skew between two builds, not a transport \
                         fault. The usual cause is a stale binary: on the cross-VM \
                         topology the GPU side is *copied* to the guest, so rebuilding \
                         here changes only this side. Rebuild and re-deploy **both** \
                         sides from the same tree."
                    ));
                }
                Err(e) => return Err(e.to_string()),
            }
        };
        control.stamp_epoch().map_err(|e| e.to_string())?;
        let slab_size = control.layout().geometry.arena_slab_size;
        let arena_len = control.layout().geometry.arena_size.min(attached_arena_len);

        let instance_slot =
            control.claim_ring(RingKind::Instance, 1, None).map_err(|e| e.to_string())?;
        let device_slot =
            control.claim_ring(RingKind::Device, 2, None).map_err(|e| e.to_string())?;
        let queue_slot = control
            .claim_ring(RingKind::Queue, 3, Some(device_slot))
            .map_err(|e| e.to_string())?;

        let open = |slot: u32| -> Result<Ring, String> {
            Ok(Ring {
                producer: RingProducer::open(&control, slot, Lane::Command)
                    .map_err(|e| e.to_string())?,
                replies: RingConsumer::open(&control, slot, Lane::Reply)
                    .map_err(|e| e.to_string())?,
                ctrl: RingCtrl::open(&control, slot).map_err(|e| e.to_string())?,
            })
        };
        let instance = open(instance_slot)?;
        let device = open(device_slot)?;
        let queue = open(queue_slot)?;
        let barriers = Barriers {
            to_queue: BarrierTracker::new(device_slot),
            to_device: BarrierTracker::new(queue_slot),
        };

        let control = Arc::new(control);

        // THE DOORBELL (waycompose-design.md §36.6), the application side.
        //
        // This is the half that *waits*: `Transport::call` parks on the reply
        // lane for every command, and across a hypervisor the GPU side's
        // `FUTEX_WAKE` reaches nobody, so each of those parks ran out its slice.
        // One `vkQueuePresentKHR` makes about eleven of them, which is the whole
        // of 216.9 ms cross-VM against 1.7-2.1 ms local. A byte on a
        // virtio-serial port crosses in ~9 µs (§36.6.3).
        //
        // Named by environment rather than by argument because this side is a
        // `cdylib` inside somebody else's process — the same reason `SV_SOCKET`
        // is an environment variable and not a flag. Absent, everything behaves
        // exactly as it did.
        //
        // A failure to attach is a warning, never an error: a session without a
        // doorbell is the session that shipped, and refusing to run because an
        // optimisation is unavailable would be worse than not having it.
        // Unix only: the host end of the chardev is a Unix socket, which is what
        // the cross-VM topology has. A Windows *app* side is the same-machine
        // topology, where the peer is on this kernel and the ordinary futex wake
        // already reaches it — there is no hypervisor to carry anything across.
        #[cfg(unix)]
        let doorman = match std::env::var(crate::BELL_ENV) {
            Err(_) => None,
            Ok(path) => match sv_transport::vdoorbell::PortHandle::host_socket(&path)
                .map_err(|e| e.to_string())
                .and_then(|p| {
                    sv_transport::vdoorbell::attach(p, Arc::clone(&control))
                        .map_err(|e| e.to_string())
                }) {
                Ok(d) => {
                    eprintln!("[icd] doorbell attached on {path}");
                    Some(d)
                }
                Err(e) => {
                    eprintln!("[icd] no doorbell on {path} ({e}); waits will run out their slice");
                    None
                }
            },
        };

        #[cfg(not(unix))]
        let doorman: Option<sv_transport::vdoorbell::Doorman> = None;

        arm_park_report();

        let beating = Arc::new(std::sync::atomic::AtomicBool::new(true));
        {
            let control = Arc::clone(&control);
            let beating = Arc::clone(&beating);
            std::thread::Builder::new()
                .name("sv-icd-heartbeat".into())
                .spawn(move || {
                    while beating.load(std::sync::atomic::Ordering::Relaxed) {
                        let _ = control.beat_now();
                        std::thread::sleep(BEAT_INTERVAL);
                    }
                })
                .map_err(|e| format!("cannot start the heartbeat thread: {e}"))?;
        }

        // The blob reserve is the *tail* of the arena, so what the application's
        // allocator sees is a shorter arena starting at zero and nothing above
        // it changes. Taking it from the tail rather than the head also leaves
        // §5.2's rule about the largest possible allocation intact: the reserve
        // removes whole slabs, not room inside one.
        let reserve = blob_reserve(arena_len);
        let arena_len = arena_len - reserve;

        Ok(Attached {
            transport: RingTransport {
                control,
                instance: Mutex::new(instance),
                device: Mutex::new(device),
                queue: Mutex::new(queue),
                barriers: Mutex::new(barriers),
                importing: Mutex::new(()),
                publishing: Mutex::new(()),
                parker: Futex::default(),
                blobs: Mutex::new(Blobs::new(arena_len, reserve)),
                arena,
                beating,
                _doorman: doorman,
            },
            arena_len,
            slab_size,
        })
    }

    #[cfg(not(any(target_os = "linux", windows)))]
    pub fn connect(_socket: &str) -> Result<Attached, String> {
        Err("this build has a cold path for Linux and Windows only".into())
    }

    fn ring(&self, class: RingClass) -> &Mutex<Ring> {
        match class {
            RingClass::Instance => &self.instance,
            RingClass::Device => &self.device,
            RingClass::Queue => &self.queue,
        }
    }

    /// Tell the GPU side we are finished, so it can stop draining and exit.
    pub fn shutdown(&self) {
        let _ = self.control.set_flag(Flags::SHUTDOWN);
    }

    /// Record a device-ring seqno so the next queue packet carries a barrier
    /// past it (R22).
    ///
    /// The barrier to attach to the next packet on `class`, if any.
    fn take_barrier(&self, class: RingClass) -> Option<sv_transport::ring::RingBarrier> {
        let mut b = self.barriers.lock().unwrap_or_else(|e| e.into_inner());
        match class {
            RingClass::Queue => b.to_queue.take(),
            RingClass::Device => b.to_device.take(),
            RingClass::Instance => None,
        }
    }

    /// Record that `class` produced `seqno`, so the *other* ring orders past it.
    fn note_seqno(&self, class: RingClass, seqno: u64) {
        let mut b = self.barriers.lock().unwrap_or_else(|e| e.into_inner());
        match class {
            RingClass::Device => b.to_queue.note_produced(seqno),
            RingClass::Queue => b.to_device.note_produced(seqno),
            // Nothing orders against the instance ring: by the time a device
            // exists every instance command has been answered, because they all
            // generate replies.
            RingClass::Instance => {}
        }
    }

    /// Push one packet, with whatever barrier the other ring has earned, waiting
    /// for room rather than failing if the ring is full.
    ///
    /// The single place either tracker is consulted, so the two directions
    /// cannot drift apart.
    ///
    /// # Why it waits
    ///
    /// `RingProducer::push` returns `RingFull` immediately, and every caller of
    /// this used to turn that into a `LowerError` — which for a command that
    /// returns `void` means `report_failure` and a return. **The command was
    /// dropped and the application was told nothing.** Measured on master, a
    /// burst of 500,000 `vkCmdSetViewport` lost 24,292 of them exactly that way,
    /// visible only under `SV_DEBUG`. §6.5's blob takes `vkCmd*` off this path
    /// entirely, but `vkDestroy*` and every other fire-and-forget command is
    /// still here, and "the ring was momentarily full" must not be able to mean
    /// "your command did not happen".
    ///
    /// So a full ring is back-pressure: wait for the consumer to make room, and
    /// only give up on the R4 deadline — at which point the peer is gone and
    /// `VK_ERROR_DEVICE_LOST` is the truth rather than a shrug.
    fn push_on(&self, class: RingClass, command: &[u8]) -> IcdResult<u64> {
        let mut ring = self.ring(class).lock().unwrap_or_else(|e| e.into_inner());
        self.push_into(class, &mut ring, command, Deadline::after(CALL_TIMEOUT))
    }

    /// The body of [`RingTransport::push_on`], against a ring the caller has
    /// already locked.
    ///
    /// `call` needs this shape: it holds the producer across the *reply* wait,
    /// which is what makes "the packet that arrives is this call's" true, so it
    /// cannot hand the lock to a helper and take it back.
    fn push_into(
        &self,
        class: RingClass,
        ring: &mut Ring,
        command: &[u8],
        deadline: Deadline,
    ) -> IcdResult<u64> {
        loop {
            let barrier = self.take_barrier(class);
            match ring.producer.push(command, barrier) {
                Ok(seqno) => {
                    self.note_seqno(class, seqno);
                    // §0.23.4's instrument. The GPU side's `SV_TRACE` prints
                    // command *names* in the order it executes them, which
                    // cannot distinguish "the app sent the destroy first" from
                    // "the app published the blob late" — both look identical
                    // from there. This prints the order the app *sent* things,
                    // with the seqno the fatal names, which is the one fact
                    // that separates the three candidates.
                    if crate::transport::send_trace() {
                        eprintln!(
                            "[icd] ring {:?} seqno {seqno} opcode {} ({} bytes)",
                            class,
                            command
                                .get(0..4)
                                .map(|b| u32::from_le_bytes(b.try_into().unwrap()))
                                .unwrap_or(u32::MAX),
                            command.len()
                        );
                    }
                    return Ok(seqno);
                }
                Err(sv_transport::Error::RingFull { .. }) => {
                    // The barrier goes back before anything else can take one,
                    // or this packet's ordering guarantee is lost (R22).
                    if let Some(b) = barrier {
                        self.restore_barrier(class, b);
                    }
                    // Header, the widest barrier and the alignment, so this asks
                    // for at least what `frame_len` will.
                    let want = (command.len() as u64
                        + sv_transport::ring::PACKET_HEADER_SIZE
                        + sv_transport::ring::BARRIER_SIZE)
                        .next_multiple_of(sv_transport::ring::PACKET_ALIGN);
                    // There is no doorbell in this direction — space is freed by
                    // the *consumer* advancing `head`, and nobody rings a bell
                    // for that — so this spins and backs off. A ring that is
                    // full is a ring whose consumer is actively working, so the
                    // wait is short by construction; giving the producer a bell
                    // of its own would be two more words per lane for a case
                    // that only arises under a burst.
                    let has_room =
                        || ring.producer.free_bytes().map(|f| f >= want).unwrap_or(true);
                    wait_for_peer(
                        &self.control,
                        &ring.ctrl,
                        None,
                        &self.parker,
                        &has_room,
                        deadline,
                    )
                    .map_err(lower)?;
                }
                Err(e) => {
                    if let Some(b) = barrier {
                        self.restore_barrier(class, b);
                    }
                    return Err(lower(e));
                }
            }
        }
    }

    fn restore_barrier(&self, class: RingClass, b: sv_transport::ring::RingBarrier) {
        let mut t = self.barriers.lock().unwrap_or_else(|e| e.into_inner());
        match class {
            RingClass::Queue => t.to_queue.restore(b),
            RingClass::Device => t.to_device.restore(b),
            RingClass::Instance => {}
        }
    }

    /// Hand the GPU side everything recorded since the last piece.
    ///
    /// Called when a block passes [`BLOB_FLUSH`] of unsent bytes and again at
    /// `vkEndCommandBuffer`. Returns without a packet when there is nothing new,
    /// which is what makes calling it from both places harmless.
    fn flush_blob(&self, command_buffer: u64) -> IcdResult<()> {
        // Held across the read, the push and the watermark update. See
        // `RingTransport::publishing` for what happened without it.
        let _publishing = self.publishing.lock().unwrap_or_else(|e| e.into_inner());
        let (off, len, upto) = {
            let blobs = self.blobs.lock().unwrap_or_else(|e| e.into_inner());
            match blobs.open.get(&command_buffer) {
                Some(b) if b.len > b.published => {
                    (b.off + b.published, b.len - b.published, b.len)
                }
                _ => return Ok(()),
            }
        };
        let mut cmd = Vec::new();
        sv_proto::sv::ExecuteCommandBlob { command_buffer, arena_offset: off, len }
            .encode(&mut cmd)
            .map_err(LowerError::Wire)?;
        let seqno = self.push_on(RingClass::Device, &cmd)?;
        let mut blobs = self.blobs.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(b) = blobs.open.get_mut(&command_buffer) {
            // `upto`, not `b.len`. They differ whenever this flush is another
            // thread's `publish_recorded_first` and the owner recorded more
            // between the read above and this line; writing `b.len` would mark
            // those commands published without having sent them — commands
            // that vanish, which is the exact failure the blob exists to
            // remove.
            b.published = upto;
            b.in_flight = seqno;
        }
        Ok(())
    }

    /// Publish everything recorded so far, before sending anything that is
    /// *not* recorded. docs/design.md §0.23.5.
    ///
    /// # The bug this is the whole of
    ///
    /// `vkCmd*` are buffered into a command buffer's blob and published on a
    /// schedule of their own; `vkCreate*`, `vkDestroy*` and everything else are
    /// forwarded immediately. Both go to the device ring, so each class is
    /// ordered *with itself* — and the program order **between** the two
    /// classes was not preserved. An application that records with a pipeline
    /// layout, ends the command buffer and then destroys the layout emits
    /// `[recorded…] [destroy]`, and the wire carried `[destroy] [recorded…]`.
    ///
    /// `VK_KHR_maintenance4` makes that application correct — a layout's
    /// lifetime ends when the command buffer leaves the recording state — so
    /// this was our defect and not the application's. It reproduced as
    /// `no VkPipelineLayout with id 44` on 144 of 48 CTS cases; §0.23.4 has the
    /// send trace that separates "the app sent the destroy first" from "the app
    /// published the blob late", which is the distinction that decides the fix
    /// and which the GPU side's own trace cannot see.
    ///
    /// # Why here, and why not "before destroys"
    ///
    /// A transport that knew which opcodes are destroys would be a transport
    /// with a reason to be regenerated (see [`Transport`]). It does not need
    /// to: the invariant is simply *program order between recorded and
    /// non-recorded commands*, and restoring it for every immediately-forwarded
    /// command is both simpler and more obviously correct than restoring it for
    /// the one class where it has bitten so far. Destroys are where it was
    /// found; `vkUpdateDescriptorSets` and every other unrecorded command had
    /// the same hole.
    ///
    /// # What it does not cost
    ///
    /// Not the pipelining §6.5 measured at 105 µs against 130 µs. That is
    /// `vkCmd*` against `vkCmd*`, and [`Transport::record`] does **not** call
    /// this — recording still buffers, still crosses nothing, and still
    /// publishes at `vkEndCommandBuffer` or when a block fills. What is forced
    /// here is a publish before an unrecorded command, which is not a per-draw
    /// operation; and [`Self::flush_blob`] returns without a packet when there
    /// is nothing new, so the common case is a lock and a map walk.
    ///
    /// Publishing a blob that is still being recorded is safe and is not new:
    /// a blob is append-only, `published` records how much has gone, and the
    /// sub-decoder replays a partial stream in order. Filling a block already
    /// did exactly this.
    fn publish_recorded_first(&self) -> IcdResult<()> {
        // Collected under the lock and flushed outside it: `flush_blob` takes
        // the same lock, and it takes the ring producer's too.
        let open: Vec<u64> = {
            let blobs = self.blobs.lock().unwrap_or_else(|e| e.into_inner());
            if blobs.open.is_empty() {
                return Ok(());
            }
            blobs
                .open
                .iter()
                .filter(|(_, b)| b.len > b.published)
                .map(|(cb, _)| *cb)
                .collect()
        };
        for cb in open {
            self.flush_blob(cb)?;
        }
        Ok(())
    }

    /// The slow half of [`Transport::record`]: make room, then write.
    ///
    /// # A block holds what the GPU side has not read, not the whole recording
    ///
    /// This used to *grow*: double the block, copy the recording across, and
    /// keep every byte of it until the command buffer was freed. But the GPU
    /// side copies each piece out of the arena the moment it arrives
    /// (`sv_gpu_server::session::protocol_command`), so once a piece is read
    /// its bytes are dead, and a block the size of its whole recording was a
    /// block of dead bytes. Measured (docs/design.md §0.28): Firefox on Zink,
    /// 64 MiB arena, 8 MiB reserve — 29 blocks, the largest doubled to 2 MiB,
    /// and at most 7 KiB of all of them not yet sent; the next doubling needed
    /// 4 MiB contiguous and the device was lost at 60 s.
    ///
    /// So a full block is not grown. Everything unsent is published first,
    /// which leaves the block holding only pieces already handed over, and
    /// then:
    ///
    /// - if the GPU side has read the last of them, the block is **rewound**
    ///   and recording continues at its start — no allocation and no copy;
    /// - if it has not, recording continues in a **fresh** block and the old
    ///   one *retires* with the seqno of its last piece, to be given back once
    ///   `completed_seqno` passes it. Recording does not wait for the GPU side,
    ///   which is the pipelining §6.5 measured (105 µs against 130 µs) kept.
    ///
    /// A block is bigger than [`BLOB_INITIAL`] only when one command is.
    ///
    /// # Running out is back-pressure, not a lost device
    ///
    /// If the reserve cannot supply a block, [`Self::reclaim`] publishes every
    /// open recording, waits for the GPU side to read them, and gives back
    /// every block that holds nothing it still needs — retiring ones and idle
    /// ones alike — before asking again. What is left after that is only what
    /// the GPU side has not read, so the allocation can then fail only for a
    /// single command larger than the reserve.
    fn grow_and_write(&self, command_buffer: u64, command: &[u8]) -> IcdResult<()> {
        let need = command.len() as u64;
        // Only this thread records into `command_buffer` (Vulkan's external
        // synchronisation), so once this returns the block has nothing unsent
        // until the write below.
        self.flush_blob(command_buffer)?;
        for attempt in 0..=RECLAIM_ROUNDS {
            let completed = self.completed_device_seqno()?;
            let mut guard = self.blobs.lock().unwrap_or_else(|e| e.into_inner());
            let blobs = &mut *guard;
            blobs.retire_read(completed);
            let old = blobs.open.get(&command_buffer).copied();
            let placed = match old {
                // Read to the end and big enough: rewind it.
                Some(b) if b.in_flight <= completed && need <= b.cap => Some((b.off, b.cap)),
                _ => {
                    let want = old.map_or(BLOB_INITIAL, |b| b.cap.max(BLOB_INITIAL)).max(need);
                    let fresh = blobs.alloc_block(want);
                    if let (Some(_), Some(b)) = (fresh, old) {
                        blobs.open.remove(&command_buffer);
                        if b.in_flight <= completed {
                            Self::release(blobs, b);
                        } else {
                            blobs.retiring.push((b, b.in_flight));
                        }
                    }
                    fresh
                }
            };
            let Some((off, cap)) = placed else {
                drop(guard);
                if attempt == RECLAIM_ROUNDS {
                    break;
                }
                self.reclaim()?;
                continue;
            };
            // Under the lock, for the reason `record` gives.
            self.arena.write_bytes(off, command).map_err(lower)?;
            blobs
                .open
                .insert(command_buffer, Blob { off, len: need, published: 0, cap, in_flight: 0 });
            blobs.note_high_water();
            return Ok(());
        }
        let blobs = self.blobs.lock().unwrap_or_else(|e| e.into_inner());
        static SAID: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
        crate::state::say_once(&SAID, &format_args!(
            "blob reserve exhausted for a {need}-byte command after {RECLAIM_ROUNDS} reclaims: \
             {} KiB held in {} block(s) and {} retiring",
            blobs.held >> 10,
            blobs.open.len(),
            blobs.retiring.len()
        ));
        Err(LowerError::OutOfDeviceMemory)
    }

    /// Make the reserve hold only what the GPU side has not read yet.
    ///
    /// Publish every open recording, wait until the GPU side has read the
    /// newest piece of any block, then give back every retiring block and every
    /// open block with nothing unsent. An open block given back is not lost to
    /// its command buffer: its next `vkCmd*` takes the slow path and gets a
    /// fresh one, and every piece it already sent was read whole.
    fn reclaim(&self) -> IcdResult<()> {
        self.publish_recorded_first()?;
        let newest = {
            let blobs = self.blobs.lock().unwrap_or_else(|e| e.into_inner());
            let open = blobs.open.values().map(|b| b.in_flight);
            open.chain(blobs.retiring.iter().map(|r| r.1)).max().unwrap_or(0)
        };
        self.await_blob_read(newest)?;
        let completed = self.completed_device_seqno()?;
        let mut blobs = self.blobs.lock().unwrap_or_else(|e| e.into_inner());
        blobs.retire_read(completed);
        let idle: Vec<u64> = blobs
            .open
            .iter()
            .filter(|(_, b)| b.len == b.published && b.in_flight <= completed)
            .map(|(cb, _)| *cb)
            .collect();
        for cb in idle {
            if let Some(b) = blobs.open.remove(&cb) {
                Self::release(&mut blobs, b);
            }
        }
        Ok(())
    }

    /// How far the GPU side has got through the device ring.
    fn completed_device_seqno(&self) -> IcdResult<u64> {
        let ctrl = {
            let d = self.device.lock().unwrap_or_else(|e| e.into_inner());
            d.ctrl.clone()
        };
        ctrl.completed_seqno().map_err(lower)
    }

    /// Return a block to the reserve.
    fn release(blobs: &mut Blobs, b: Blob) {
        let slab_size = blobs.alloc.slab_size();
        let off = b.off - blobs.base;
        let a = crate::arena::Allocation {
            slab: (off / slab_size) as u32,
            offset_in_slab: off % slab_size,
            size: b.cap,
        };
        let _ = blobs.alloc.free_now(a);
        blobs.held -= b.cap;
    }

    /// Wait until the GPU side has finished with a block before overwriting it.
    ///
    /// docs/design.md §5.5 item 3, answered with the ring's own `completed_seqno`
    /// rather than with a second handshake word — see [`Blob::in_flight`].
    fn await_blob_read(&self, in_flight: u64) -> IcdResult<()> {
        if in_flight == 0 {
            return Ok(());
        }
        let ctrl = {
            let d = self.device.lock().unwrap_or_else(|e| e.into_inner());
            d.ctrl.clone()
        };
        if ctrl.completed_seqno().map_err(lower)? >= in_flight {
            return Ok(());
        }
        wait_for_peer(
            &self.control,
            &ctrl,
            None,
            &self.parker,
            &|| ctrl.completed_seqno().map(|c| c >= in_flight).unwrap_or(false),
            Deadline::after(CALL_TIMEOUT),
        )
        .map_err(lower)
    }
}

impl RingTransport {
    fn call_untimed(&self, class: RingClass, command: &[u8], reply: &mut Vec<u8>) -> IcdResult<()> {
        // Before the ring lock, because publishing takes it. See
        // `publish_recorded_first`.
        self.publish_recorded_first()?;
        // The producer lock is held from here to the reply, which is what makes
        // the packet that arrives *this* call's — a ring is SPSC and replies come
        // back in the order the commands went out.
        let mut ring = self.ring(class).lock().unwrap_or_else(|e| e.into_inner());
        let ring = &mut *ring;
        let deadline = Deadline::after(CALL_TIMEOUT);
        // Through the same helper as everything else, so a full ring is
        // back-pressure here too rather than a `NotConnected` on a command the
        // application is waiting for an answer to.
        let seqno = self.push_into(class, ring, command, deadline)?;
        loop {
            wait_for_peer(
                &self.control,
                &ring.ctrl,
                Some(Lane::Reply),
                &self.parker,
                &|| !ring.replies.is_empty().unwrap_or(false),
                deadline,
            )
            .map_err(lower)?;
            let got = ring.replies.next().map_err(lower)?.map(|p| (p.seqno, p.payload.to_vec()));
            let Some((got_seqno, payload)) = got else { continue };
            ring.replies.advance().map_err(lower)?;
            if got_seqno < seqno {
                // **The answer to an earlier call that stopped waiting for it.**
                // A call that times out, or that decides the peer is gone,
                // leaves its command on the ring, and the GPU side, if it was
                // only slow, still answers it. This used to be the error below,
                // and it was not one error: the call that met the late reply
                // left its *own* reply behind for the next call, and so on, so
                // every later call on the ring failed, and Zink would report each
                // `vkCreateBuffer` after that as failed, for the rest of the process
                // (waycompose-design.md §95). Sequence numbers only grow, so an
                // older one is exactly an abandoned call's, and it is dropped.
                continue;
            }
            if got_seqno != seqno {
                // A ring is SPSC and this call holds the producer, so a reply
                // *newer* than the command cannot happen, which is exactly why
                // it is checked. If it ever does, a reply has been matched to
                // the wrong command and the application would get another
                // call's answer.
                return Err(LowerError::Shape("a reply arrived out of order"));
            }
            reply.clear();
            reply.extend_from_slice(&payload);
            return Ok(());
        }
    }
}

/// What the application waited on the GPU side for since the last present
/// (`WC_FRAME_TRACE`, docs/waycompose-design.md §88): every synchronous round
/// trip, by opcode, and how many fire-and-forget packets went with them.
pub(crate) static ROUND_TRIPS: Mutex<sv_transport::frametrace::OpTally> =
    Mutex::new(sv_transport::frametrace::OpTally::new());
pub(crate) static SENDS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

thread_local! {
    static UNTALLIED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// This thread's round trips are not the application's: the presenter's
/// waits are reported as the present's own phases.
pub(crate) fn untallied_on_this_thread() {
    UNTALLIED.with(|u| u.set(true));
}

impl Transport for RingTransport {
    fn call(
        &self,
        class: RingClass,
        opcode: u32,
        command: &[u8],
        reply: &mut Vec<u8>,
    ) -> IcdResult<()> {
        if !sv_transport::frametrace::enabled() || UNTALLIED.with(|u| u.get()) {
            return self.call_untimed(class, command, reply);
        }
        let started = std::time::Instant::now();
        let r = self.call_untimed(class, command, reply);
        ROUND_TRIPS.lock().unwrap_or_else(|e| e.into_inner()).add(opcode, started.elapsed());
        r
    }

    fn send(&self, class: RingClass, _opcode: u32, command: &[u8]) -> IcdResult<()> {
        if sv_transport::frametrace::enabled() {
            SENDS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
        self.publish_recorded_first()?;
        self.push_on(class, command).map(|_| ())
    }

    /// docs/design.md §6.5: append to the command buffer's arena block, and cross
    /// nothing.
    ///
    /// This is the whole of the blob change on the recording side. What it
    /// replaces is one ring packet per `vkCmd*` — a mutex, a sequence number,
    /// a framing header, a doorbell check and a consumer wake-up, for a command
    /// that has no reply and that the application will not look at again until
    /// it submits.
    ///
    /// It also removes a failure this transport had no way to report. A
    /// `vkCmd*` returns `void`, so a full ring produced `RingFull`, which the
    /// generated stub turned into `report_failure` and a return: **the command
    /// was dropped and the application was told nothing.** Measured on master,
    /// a burst of 500,000 `vkCmdSetViewport` lost 24,292 of them that way. A
    /// blob has no such window — it grows, and if the reserve is exhausted the
    /// error is `VK_ERROR_OUT_OF_DEVICE_MEMORY` from a command that is allowed
    /// to fail.
    /// # The hot path is one lock, one map lookup and one `memcpy`
    ///
    /// It is written as an explicit fast path rather than left to fall out of
    /// the general case, and that is not premature: recording is the *only*
    /// operation in this transport that happens hundreds of times per frame, so
    /// a second `HashMap` probe and a second uncontended mutex acquisition —
    /// which is all the obvious version costs — measured at 0.056 µs a command,
    /// against 0.075 µs for the entire ring push this is replacing. A blob that
    /// is slower than the packet it removes is not an optimisation.
    ///
    /// # The write happens under the lock (docs/design.md §0.27)
    ///
    /// It used to happen after: `len` advanced under the lock *reserved* the
    /// range, and the comment here said the GPU side could not see the bytes
    /// until `publish_blob` named them. That was true of *this* thread and
    /// false of every other one. [`RingTransport::publish_recorded_first`] runs
    /// on whichever thread forwards an unrecorded command, and it flushes every
    /// open blob up to `len` — including one whose owner has advanced `len` and
    /// not yet copied the command in. The GPU side then decoded bytes nobody
    /// had written. Measured: Firefox on Zink, a WebGL page and a scroll, and
    /// `sv-gpu-server` ended the session with "in a command blob: unknown
    /// command opcode 4096" after 74 s; Mesa's device-lost path then took
    /// Firefox down with it.
    ///
    /// So `len` only ever covers bytes that are there. The copy is a command —
    /// tens to hundreds of bytes — and the lock is the one the fast path
    /// already takes, so it stays one lock, one probe and one `memcpy`; what it
    /// gives up is overlap between two threads recording two command buffers,
    /// for the length of a `memcpy`.
    ///
    /// The video work met the same race from the other side and fixed it the
    /// same way: ffmpeg's frame threads submitting on one queue while another
    /// records gave a 10-second `CALL_TIMEOUT` stall in roughly one
    /// multi-threaded decode in seven, and none after (docs/video-decode.md
    /// §7.6).
    fn record(&self, command_buffer: u64, _opcode: u32, command: &[u8]) -> IcdResult<()> {
        let need = command.len() as u64;
        let flush = {
            let mut blobs = self.blobs.lock().unwrap_or_else(|e| e.into_inner());
            match blobs.open.get_mut(&command_buffer) {
                Some(b) if b.len + need <= b.cap => {
                    let at = b.off + b.len;
                    self.arena.write_bytes(at, command).map_err(lower)?;
                    b.len += need;
                    b.len - b.published >= BLOB_FLUSH
                }
                // No block yet, or a full one. Both are the slow path, and both
                // are rare: a block is rewound rather than grown, so only a
                // command bigger than the block itself makes a bigger one.
                _ => {
                    drop(blobs);
                    self.grow_and_write(command_buffer, command)?;
                    false
                }
            }
        };
        if flush {
            self.flush_blob(command_buffer)?;
        }
        Ok(())
    }

    /// `vkEndCommandBuffer`: send the last piece, and do not wait for an answer.
    ///
    /// It goes on the **device** ring, in program order with everything else the
    /// application does to that command buffer, which is what keeps a
    /// `vkCmdExecuteCommands` naming a secondary that was ended earlier working
    /// without any extra ordering: the secondary's pieces are device-ring
    /// packets with lower seqnos.
    fn publish_blob(&self, command_buffer: u64) -> IcdResult<()> {
        self.flush_blob(command_buffer)
    }

    /// `vkBeginCommandBuffer` and `vkResetCommandBuffer`: start again at zero.
    ///
    /// **This is where docs/design.md §5.5 item 3's read-pending check lives**, and
    /// it is the right place for it: rewinding is the only operation that
    /// rewrites bytes the GPU side may still be walking, it happens once per
    /// recording rather than once per command, and by the time an application
    /// re-begins a command buffer the pieces of the previous recording have
    /// almost always long since retired — so the wait is one acquire load.
    ///
    /// If the wait fails the block is **not** rewound. That leaves the next
    /// `vkCmd*` to wait again and to report the failure through a call that can
    /// return one, which is better than overwriting a blob the peer is reading
    /// because the peer stopped answering.
    fn discard_blob(&self, command_buffer: u64) {
        let in_flight = {
            let blobs = self.blobs.lock().unwrap_or_else(|e| e.into_inner());
            blobs.open.get(&command_buffer).map(|b| b.in_flight).unwrap_or(0)
        };
        if self.await_blob_read(in_flight).is_err() {
            return;
        }
        let mut blobs = self.blobs.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(b) = blobs.open.get_mut(&command_buffer) {
            // The block itself is kept: a command buffer that is reset is about
            // to be recorded into again, and giving the reserve back only to ask
            // for the same size a moment later is how a first-fit allocator
            // fragments. It is released for real by `free_blob`.
            b.len = 0;
            b.published = 0;
            b.in_flight = 0;
        }
    }
}

impl RingTransport {
    /// Make sure the GPU side has imported the slab an allocation landed in
    /// (docs/design.md §5.2, `svImportArenaSlab`).
    ///
    /// Idempotent and cheap: the slab directory in the control region is the
    /// shared record of which slabs exist, so the common case — a slab already
    /// imported, or a provider that imported the whole arena at startup — is one
    /// acquire load and no packet.
    ///
    /// It is a **round trip**, and that is right rather than regrettable: the
    /// memory type is the GPU side's answer, `vkAllocateMemory` cannot proceed
    /// without knowing the import succeeded, and it happens once per 64 MiB of
    /// arena an application actually touches.
    pub fn ensure_slab(&self, slab: u32, arena_offset: u64, len: u64) -> IcdResult<()> {
        // A second thread allocating in the same slab must not send a second
        // import; the GPU side would allocate a second `VkDeviceMemory` over the
        // same pages and leak one of them.
        let _serialise = self.importing.lock().unwrap_or_else(|e| e.into_inner());
        match self.control.slab_import_state(slab) {
            Ok((1, _)) => return Ok(()),
            Ok(_) => {}
            // A slab index the directory cannot hold is a geometry disagreement
            // and not something to paper over.
            Err(e) => return Err(lower(e)),
        }
        self.control.write_slab(slab, arena_offset, len).map_err(lower)?;

        let mut cmd = Vec::new();
        sv_proto::sv::ImportArenaSlab { slab, arena_offset, len }
            .encode(&mut cmd)
            .map_err(LowerError::Wire)?;
        let mut reply = Vec::new();
        // On the device ring, because that is where everything about objects
        // goes and because the barrier machinery already orders the queue ring
        // against it.
        self.call(RingClass::Device, sv_proto::sv::SV_IMPORT_ARENA_SLAB, &cmd, &mut reply)?;
        let r = sv_proto::sv::ImportArenaSlabReply::decode(&reply).map_err(LowerError::Wire)?;
        if r.result != 0 {
            return Err(LowerError::Result(r.result));
        }
        self.control.complete_slab_import(slab, r.memory_type).map_err(lower)?;
        Ok(())
    }

    /// Ask the GPU side which Vulkan version its driver actually is (§0.18).
    ///
    /// On the **instance** ring, because it is asked before any device exists —
    /// `vkEnumerateInstanceVersion` is a global command and the loader may call
    /// it before `vkCreateInstance`.
    ///
    /// `None` for any failure, and that is deliberate rather than lazy: a GPU
    /// side built before this command existed answers "unknown protocol command"
    /// and its ring is then fatal, so the *only* correct handling of a failure
    /// here is to fall back to the ceiling. `WIRE_VERSION` is what actually
    /// keeps such a peer away; this is the belt to that pair of braces.
    pub fn driver_versions(&self) -> Option<(u32, u32)> {
        let mut cmd = Vec::new();
        sv_proto::sv::DriverVersions.encode(&mut cmd).ok()?;
        let mut reply = Vec::new();
        self.call(RingClass::Instance, sv_proto::sv::SV_DRIVER_VERSIONS, &cmd, &mut reply).ok()?;
        let r = sv_proto::sv::DriverVersionsReply::decode(&reply).ok()?;
        Some((r.instance_api_version, r.device_api_version))
    }

    /// `vkFreeCommandBuffers`: give the block back to the reserve.
    ///
    /// Separate from [`Transport::discard_blob`] because the two mean different
    /// things — a reset command buffer keeps its block and a freed one does not
    /// — and because only `sv-icd` knows which happened.
    pub fn free_blob(&self, command_buffer: u64) {
        let in_flight = {
            let blobs = self.blobs.lock().unwrap_or_else(|e| e.into_inner());
            blobs.open.get(&command_buffer).map(|b| b.in_flight).unwrap_or(0)
        };
        // The GPU side may still be walking these bytes; handing them to the
        // next `vkCmd*` before it has finished is §5.5 item 3's whole subject.
        let _ = self.await_blob_read(in_flight);
        let mut blobs = self.blobs.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(b) = blobs.open.remove(&command_buffer) {
            Self::release(&mut blobs, b);
        }
    }
}

/// Print how long this process spent blocked on a ring, once.
///
/// §36.10's instrument. Wall clock could not resolve the doorbell — most of a
/// run is setup, and one sample includes a VM boot — but **time spent parked**
/// is exactly the quantity a doorbell changes, and the app side is the side that
/// does the waiting.
///
/// # Why an `atexit` and not just `Drop`
///
/// `RingTransport` is reached through a process-lifetime singleton, so its
/// `Drop` never runs: an application that exits without destroying its instance
/// — which `vulkaninfo` does, and which is legal — takes the number with it. The
/// first version of this printed nothing for exactly that reason and the log
/// looked as though the ICD had never waited at all.
///
/// Registered once, on the first attach, so a process that never talks to a ring
/// prints nothing.
fn report_park_time() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        if let Some(s) = sv_transport::park::park_summary() {
            eprintln!("[icd] {s}");
        }
    });
}

#[cfg(unix)]
extern "C" fn park_time_atexit() {
    report_park_time();
}

/// Arrange for [`report_park_time`] to run however the process ends.
///
/// Unix only, and that is the measurement's venue rather than a limitation: in
/// the cross-VM topology the app side — this side — runs on the **host**, which
/// is Linux, and the guest is the GPU half. A Windows app side is the
/// same-machine topology, where the peer is on this kernel, the ordinary futex
/// wake already reaches it, and there is no hypervisor latency to account for.
pub(crate) fn arm_park_report() {
    #[cfg(unix)]
    {
        static ARMED: std::sync::Once = std::sync::Once::new();
        ARMED.call_once(|| {
            // SAFETY: `park_time_atexit` is a plain `extern "C"` function with
            // no arguments that touches only atomics and stderr.
            unsafe { libc::atexit(park_time_atexit) };
        });
    }
}

/// `SV_SEND_TRACE=1`: print every packet this side pushes, with its ring and
/// seqno.
///
/// §0.23.4. The GPU side's `SV_TRACE` prints commands in *execution* order,
/// which is the order it drains rings — so a destroy that appears before a blob
/// there is consistent with the app having sent it first *and* with the app
/// having published the blob late, and those have different fixes. This is the
/// send order, from the only side that knows it.
pub fn send_trace() -> bool {
    use std::sync::OnceLock;
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("SV_SEND_TRACE").is_some())
}

/// docs/design.md §0.27: a recording's bytes, as the GPU side sees them while
/// another thread forwards unrecorded commands.
#[cfg(all(test, any(target_os = "linux", windows)))]
mod blob_race {
    use super::*;
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicBool, Ordering};
    use sv_transport::control::{Geometry, RingKind};

    /// One recorded command: a tag, the command buffer it belongs to and its
    /// index in that recording, then filler. Every byte is non-zero and every
    /// command is distinguishable, so a byte the GPU side reads that nobody
    /// wrote — zero, or another recording's — cannot pass for one somebody did.
    const CMD: usize = 64;
    const TAG: u32 = 0x5eed_c0de;

    fn command(cb: u64, index: u32) -> [u8; CMD] {
        let mut c = [0x5au8; CMD];
        c[0..4].copy_from_slice(&TAG.to_le_bytes());
        c[4..8].copy_from_slice(&(cb as u32).to_le_bytes());
        c[8..12].copy_from_slice(&index.to_le_bytes());
        c
    }

    /// A real `RingTransport` on heap regions, a thread that plays the GPU side
    /// and checks every command of every piece, and optionally one that
    /// forwards unrecorded packets as fast as it can. `record` runs on the
    /// calling thread; the result is how many commands the GPU side saw whole,
    /// and what `record` returned.
    fn rig(
        noisy: bool,
        record: impl FnOnce(&RingTransport) -> IcdResult<()>,
    ) -> (u64, IcdResult<()>, Arc<RingTransport>) {
        let g = Geometry::small();
        let control_region = Arc::new(SharedRegion::heap(g.control_size as usize).unwrap());
        let arena = Arc::new(SharedRegion::heap(g.arena_size as usize).unwrap());
        let owner = Arc::new(
            Control::create(Arc::clone(&control_region), g, crate::protocol_ids(), 1).unwrap(),
        );
        let t = Arc::new(
            RingTransport::finish_attach(control_region, Arc::clone(&arena), g.arena_size)
                .unwrap()
                .transport,
        );
        let device = (0..g.ring_slot_count)
            .find(|s| matches!(owner.ring_kind(*s), Ok(RingKind::Device)))
            .expect("the app side claimed a device ring");

        let stop = Arc::new(AtomicBool::new(false));
        let gpu = {
            let (owner, arena, stop) = (Arc::clone(&owner), Arc::clone(&arena), Arc::clone(&stop));
            std::thread::spawn(move || -> Result<u64, String> {
                let mut cons = RingConsumer::open(&owner, device, Lane::Command).unwrap();
                let mut next: HashMap<u64, u32> = HashMap::new();
                let mut seen = 0u64;
                loop {
                    // A GPU side that does not beat is one the app side's
                    // waits presume gone (R4).
                    let _ = owner.beat_now();
                    let piece = match cons.next().map_err(|e| e.to_string())? {
                        Some(p) => sv_proto::sv::ExecuteCommandBlob::decode(p.payload).ok(),
                        None if stop.load(Ordering::Acquire) => return Ok(seen),
                        None => {
                            std::thread::yield_now();
                            continue;
                        }
                    };
                    if let Some(b) = piece {
                        if b.len % CMD as u64 != 0 {
                            return Err(format!("a piece of {} bytes", b.len));
                        }
                        let mut bytes = vec![0u8; b.len as usize];
                        arena.read_bytes(b.arena_offset, &mut bytes).unwrap();
                        for c in bytes.chunks(CMD) {
                            let want = next.entry(b.command_buffer).or_insert(0);
                            if c != command(b.command_buffer, *want) {
                                return Err(format!(
                                    "command buffer {} command {}: read {:02x?}",
                                    b.command_buffer,
                                    *want,
                                    &c[..12]
                                ));
                            }
                            *want += 1;
                            seen += 1;
                        }
                    }
                    cons.advance().map_err(|e| e.to_string())?;
                }
            })
        };
        let noise = noisy.then(|| {
            let (t, stop) = (Arc::clone(&t), Arc::clone(&stop));
            std::thread::spawn(move || {
                // Not a blob, so the GPU side skips it; what matters is the
                // `publish_recorded_first` every send runs first.
                let mut noise = Vec::new();
                sv_proto::sv::DriverVersions.encode(&mut noise).unwrap();
                while !stop.load(Ordering::Acquire) {
                    let _ = t.send(RingClass::Device, 0, &noise);
                }
            })
        });
        let recorded = record(&t);
        stop.store(true, Ordering::Release);
        if let Some(n) = noise {
            n.join().unwrap();
        }
        // The GPU side's verdict first: when it stops on a bad command it also
        // stops answering, and the recording thread's error is then only the
        // echo of that.
        let seen = gpu.join().unwrap().expect("every command the GPU side was handed was whole");
        (seen, recorded, t)
    }

    /// **The bug**: `record` advanced `len` under the blob lock and copied the
    /// command in after releasing it, and `publish_recorded_first` — run by
    /// whichever thread forwards an unrecorded command — flushes every open
    /// blob up to `len`. So a piece could name bytes not yet written. Firefox
    /// on Zink met it as "unknown command opcode 4096" in `sv-gpu-server` and a
    /// lost device.
    ///
    /// Here: one thread records into a fresh command buffer per round, which
    /// fills a block and moves on at least once, and publishes at the end; a second forwards
    /// unrecorded packets as fast as it can, each one a `publish_recorded_first`;
    /// a third plays the GPU side and checks every command of every piece it is
    /// handed. The GPU side's checks are what fail, not a timeout.
    #[test]
    fn a_flush_from_another_thread_never_publishes_unwritten_bytes() {
        const ROUNDS: u64 = 200;
        const PER_ROUND: u32 = 2_000; // 125 KiB: two blocks' worth
        let (seen, recorded, _) = rig(true, |t| {
            for cb in 1..=ROUNDS {
                for i in 0..PER_ROUND {
                    t.record(cb, 0, &command(cb, i))?;
                }
                t.publish_blob(cb)?;
                t.free_blob(cb);
            }
            Ok(())
        });
        recorded.expect("the recording thread finished");
        assert_eq!(seen, ROUNDS * u64::from(PER_ROUND), "and every command was handed over");
    }

    /// docs/design.md §0.28: a block used to grow to its whole recording, so
    /// one recording longer than the reserve was `VK_ERROR_OUT_OF_DEVICE_MEMORY`
    /// — and in Firefox a lost device. The GPU side copies each piece out on
    /// arrival, so a block only has to hold what it has not read. Here one
    /// recording is three times the 2 MiB reserve.
    #[test]
    fn a_recording_longer_than_the_reserve_streams_through_it() {
        const COMMANDS: u32 = 100_000; // 6.1 MiB
        let (seen, recorded, t) = rig(false, |t| {
            for i in 0..COMMANDS {
                t.record(1, 0, &command(1, i))?;
            }
            t.publish_blob(1)
        });
        recorded.expect("recorded without running out");
        assert_eq!(seen, u64::from(COMMANDS));
        // Blocks the GPU side was still reading retire rather than block the
        // recording, so how many are held depends on how far it lagged; what
        // may not happen is a block growing to the recording.
        let blobs = t.blobs.lock().unwrap();
        assert!(blobs.held <= 2 << 20, "{} bytes held", blobs.held);
        let largest = blobs.open.values().chain(blobs.retiring.iter().map(|r| &r.0)).map(|b| b.cap).max();
        assert_eq!(largest, Some(BLOB_INITIAL), "no block grew past its first size");
    }

    /// The other half of §0.28: command buffers whose blocks nobody gives back
    /// — a `vkDestroyCommandPool` the transport was never told about, or just
    /// more live command buffers than the reserve has blocks for. 200 of them
    /// at a block each is 12.5 MiB against a 2 MiB reserve; running out is
    /// back-pressure that reclaims every block the GPU side has finished with,
    /// not an error.
    #[test]
    fn command_buffers_that_are_never_freed_do_not_exhaust_the_reserve() {
        const BUFFERS: u64 = 200;
        const PER_BUFFER: u32 = 1_500; // 94 KiB each
        let (seen, recorded, t) = rig(true, |t| {
            for cb in 1..=BUFFERS {
                for i in 0..PER_BUFFER {
                    t.record(cb, 0, &command(cb, i))?;
                }
                t.publish_blob(cb)?;
            }
            Ok(())
        });
        recorded.expect("recorded without running out");
        assert_eq!(seen, BUFFERS * u64::from(PER_BUFFER));
        let blobs = t.blobs.lock().unwrap();
        assert!(blobs.held <= 2 << 20, "{} bytes held", blobs.held);
    }
}

/// waycompose-design.md §95: a call that gives up on a GPU side that was only
/// stopped, and the call after it.
#[cfg(all(test, any(target_os = "linux", windows)))]
mod abandoned_calls {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    use sv_transport::control::{Geometry, RingKind};

    /// A `RingTransport` on heap regions and a GPU side that answers every
    /// `DriverVersions` with its own sequence number. Before answering the
    /// first, it stops beating for twice [`PEER_GONE_AFTER`], which is what a
    /// guest VM that is not scheduled looks like from here.
    #[test]
    fn a_late_reply_to_an_abandoned_call_is_not_the_next_calls_answer() {
        let g = Geometry::small();
        let control_region = Arc::new(SharedRegion::heap(g.control_size as usize).unwrap());
        let arena = Arc::new(SharedRegion::heap(g.arena_size as usize).unwrap());
        let owner = Arc::new(
            Control::create(Arc::clone(&control_region), g, crate::protocol_ids(), 1).unwrap(),
        );
        let t = RingTransport::finish_attach(control_region, arena, g.arena_size)
            .unwrap()
            .transport;
        let device = (0..g.ring_slot_count)
            .find(|s| matches!(owner.ring_kind(*s), Ok(RingKind::Device)))
            .expect("the app side claimed a device ring");

        let beating = Arc::new(AtomicBool::new(true));
        let stop = Arc::new(AtomicBool::new(false));
        let heart = {
            let (owner, beating, stop) = (Arc::clone(&owner), Arc::clone(&beating), Arc::clone(&stop));
            std::thread::spawn(move || {
                while !stop.load(Ordering::Acquire) {
                    if beating.load(Ordering::Acquire) {
                        let _ = owner.beat_now();
                    }
                    std::thread::sleep(std::time::Duration::from_millis(5));
                }
            })
        };
        let gpu = {
            let (owner, beating) = (Arc::clone(&owner), Arc::clone(&beating));
            std::thread::spawn(move || {
                let mut cons = RingConsumer::open(&owner, device, Lane::Command).unwrap();
                let mut replies = RingProducer::open(&owner, device, Lane::Reply).unwrap();
                for n in 0..2 {
                    let seqno = loop {
                        if let Some(p) = cons.next().unwrap() {
                            break p.seqno;
                        }
                        std::thread::yield_now();
                    };
                    cons.advance().unwrap();
                    if n == 0 {
                        beating.store(false, Ordering::Release);
                        std::thread::sleep(PEER_GONE_AFTER * 2);
                        beating.store(true, Ordering::Release);
                    }
                    let mut out = Vec::new();
                    sv_proto::sv::DriverVersionsReply {
                        instance_api_version: seqno as u32,
                        device_api_version: 0,
                    }
                    .encode(&mut out)
                    .unwrap();
                    replies.push_seq(seqno, &out, None).unwrap();
                }
            })
        };

        let mut cmd = Vec::new();
        sv_proto::sv::DriverVersions.encode(&mut cmd).unwrap();
        let mut reply = Vec::new();
        let first = t.call(RingClass::Device, 0, &cmd, &mut reply);
        assert_eq!(first, Err(LowerError::DeviceLost), "the silence outlasted PEER_GONE_AFTER");
        // Before the fix this was `Shape("a reply arrived out of order")`, and
        // so was every call after it.
        t.call(RingClass::Device, 0, &cmd, &mut reply).expect("the second call is answered");
        let got = sv_proto::sv::DriverVersionsReply::decode(&reply).unwrap();
        gpu.join().unwrap();
        stop.store(true, Ordering::Release);
        heart.join().unwrap();
        assert_eq!(got.instance_api_version, 2, "with its own reply, not the first call's");
    }
}
