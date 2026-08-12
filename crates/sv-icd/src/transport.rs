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
use sv_transport::park::{Deadline, SpinYield, wait_until};
use sv_transport::region::SharedRegion;
use sv_transport::ring::{BarrierTracker, RingConsumer, RingCtrl, RingProducer};

/// How long a single command may take before the GPU side is presumed gone.
///
/// R4: "timeouts are mandatory on every wait … a wait that expires checks the
/// peer heartbeat and the generation counter before deciding between 'slow' and
/// 'gone'." `wait_until` does that check; this is the budget it does it within.
const CALL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// One ring, and everything needed to use it from several threads.
struct Ring {
    producer: RingProducer,
    replies: RingConsumer,
    ctrl: RingCtrl,
    /// `Some` on the queue ring: the device ring it orders against.
    tracker: Option<BarrierTracker>,
}

/// A connected app side.
pub struct RingTransport {
    control: Arc<Control>,
    instance: Mutex<Ring>,
    device: Mutex<Ring>,
    queue: Mutex<Ring>,
    parker: SpinYield,
    /// The arena, mapped. Held here because the mapping must outlive every
    /// pointer `vkMapMemory` handed out, and this is the only thing that lives
    /// as long as the process.
    pub arena: Arc<SharedRegion>,
    /// Clearing this stops the heartbeat thread.
    beating: Arc<std::sync::atomic::AtomicBool>,
}

impl Drop for RingTransport {
    fn drop(&mut self) {
        self.beating.store(false, std::sync::atomic::Ordering::Relaxed);
    }
}

/// design.md §4.2: "monotonic tick, updated at least every 100 ms."
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
fn lower(e: sv_transport::Error) -> LowerError {
    sv_icd_entry::report_transport_failure(&e);
    match e {
        // §4.3: "the app side turns that into `VK_ERROR_DEVICE_LOST` on every
        // outstanding call, which is exactly what the specification provides
        // for."
        sv_transport::Error::PeerRestarted { .. }
        | sv_transport::Error::PeerGone { .. }
        | sv_transport::Error::TimedOut { .. }
        | sv_transport::Error::RingFatal { .. } => LowerError::DeviceLost,
        _ => LowerError::NotConnected,
    }
}

impl RingTransport {
    /// Connect to whichever side is serving the cold path, attach, claim rings.
    ///
    /// Accepts **one or two descriptors**, and that is how it tells the two
    /// arena providers apart without a flag: `sv-hostd` sends one memfd holding
    /// both regions at different offsets, while `sv-gpu-server serve` sends the
    /// control memfd and the driver's exported arena as two separate objects.
    /// The `Attachment`'s offsets are correct in both cases, so nothing below
    /// this function knows which happened.
    #[cfg(target_os = "linux")]
    pub fn connect(socket: &str) -> Result<Attached, String> {
        use std::os::fd::AsFd;
        use std::os::unix::net::UnixStream;
        use sv_transport::cold::{ColdMessage, recv_with_fds, send};

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
        let mut buf = Vec::new();
        let fds = recv_with_fds(&sock, &mut buf).map_err(|e| e.to_string())?;
        let at = match ColdMessage::decode(&buf).map_err(|e| e.to_string())? {
            ColdMessage::Attach(a) => a,
            ColdMessage::Refuse { code, reason } => {
                return Err(format!("refused {code}: {reason}"));
            }
            other => return Err(format!("unexpected cold-path message {other:?}")),
        };
        let control_fd = fds.first().ok_or("no descriptor for the control region")?;
        let arena_fd = fds.get(1).unwrap_or(control_fd);

        // A non-zero `run_count` means the extent is scattered through the
        // descriptor and the daemon is sending the runs next. That is the
        // `Mode::Qemu` path: the descriptor is QEMU's memory backend, the pages
        // are guest RAM, and a plain `map_fd` would map the wrong 64 MiB of a
        // thirteen-gigabyte file. §3: `sv-hostd` "hands **fd + runs** to the
        // ICDs".
        let (control_region, arena) = if at.run_count == 0 {
            (
                Arc::new(
                    SharedRegion::map_fd(
                        control_fd.as_fd(),
                        at.control_offset,
                        at.control_len as usize,
                    )
                    .map_err(|e| e.to_string())?,
                ),
                Arc::new(
                    SharedRegion::map_fd(arena_fd.as_fd(), at.arena_offset, at.arena_len as usize)
                        .map_err(|e| e.to_string())?,
                ),
            )
        } else {
            let mut buf = Vec::new();
            let _ = recv_with_fds(&sock, &mut buf).map_err(|e| e.to_string())?;
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
            RingTransport::stitch_two(control_fd.as_fd(), &at, &runs)?
        };
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

        let open = |slot: u32, tracker: Option<BarrierTracker>| -> Result<Ring, String> {
            Ok(Ring {
                producer: RingProducer::open(&control, slot, Lane::Command)
                    .map_err(|e| e.to_string())?,
                replies: RingConsumer::open(&control, slot, Lane::Reply)
                    .map_err(|e| e.to_string())?,
                ctrl: RingCtrl::open(&control, slot).map_err(|e| e.to_string())?,
                tracker,
            })
        };
        let instance = open(instance_slot, None)?;
        let device = open(device_slot, None)?;
        let queue = open(queue_slot, Some(BarrierTracker::new(device_slot)))?;

        let control = Arc::new(control);
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

        Ok(Attached {
            transport: RingTransport {
                control,
                instance: Mutex::new(instance),
                device: Mutex::new(device),
                queue: Mutex::new(queue),
                parker: SpinYield::default(),
                arena,
                beating,
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
    /// **Lock order is device → queue and never the reverse.** A queue-ring
    /// operation takes only the queue lock, so no cycle exists; stating it here
    /// is cheaper than rediscovering it from a hang.
    fn note_device_seqno(&self, class: RingClass, seqno: u64) {
        if class != RingClass::Device {
            return;
        }
        let mut q = self.queue.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(t) = q.tracker.as_mut() {
            t.note_produced(seqno);
        }
    }
}

impl Transport for RingTransport {
    fn call(
        &self,
        class: RingClass,
        _opcode: u32,
        command: &[u8],
        reply: &mut Vec<u8>,
    ) -> IcdResult<()> {
        let mut ring = self.ring(class).lock().unwrap_or_else(|e| e.into_inner());
        let ring = &mut *ring;
        let barrier = ring.tracker.as_mut().and_then(|t| t.take());
        let seqno = ring.producer.push(command, barrier).map_err(lower)?;
        self.note_device_seqno(class, seqno);

        let deadline = Deadline::after(CALL_TIMEOUT);
        loop {
            wait_until(
                &self.control,
                &ring.ctrl,
                &self.parker,
                &|| !ring.replies.is_empty().unwrap_or(false),
                deadline,
                std::time::Duration::from_millis(50),
            )
            .map_err(lower)?;
            let got = ring.replies.next().map_err(lower)?.map(|p| (p.seqno, p.payload.to_vec()));
            let Some((got_seqno, payload)) = got else { continue };
            ring.replies.advance().map_err(lower)?;
            if got_seqno != seqno {
                // A ring is SPSC and this call holds the producer, so this
                // cannot happen — which is exactly why it is checked. If it ever
                // does, a reply has been matched to the wrong command and the
                // application would get another call's answer.
                return Err(LowerError::Shape("a reply arrived out of order"));
            }
            reply.clear();
            reply.extend_from_slice(&payload);
            return Ok(());
        }
    }

    fn send(&self, class: RingClass, _opcode: u32, command: &[u8]) -> IcdResult<()> {
        let seqno = {
            let mut ring = self.ring(class).lock().unwrap_or_else(|e| e.into_inner());
            let barrier = ring.tracker.as_mut().and_then(|t| t.take());
            ring.producer.push(command, barrier).map_err(lower)?
        };
        self.note_device_seqno(class, seqno);
        Ok(())
    }
}
