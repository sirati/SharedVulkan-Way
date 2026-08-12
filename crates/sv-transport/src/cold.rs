//! The cold path: the handshake messages, and the fd handoff.
//!
//! design.md D10: "A socket: **virtio-serial** at first because it is already
//! attached, **vsock** once `viosock` is installed. Handshake, capability
//! negotiation, wakeups, teardown. **Never data.**" On the same machine it is a
//! Unix socket, which is what §13 steps 1–6 run over.
//!
//! # Why the locator descriptor rides here and not in the control region
//!
//! > **The locator descriptor must ride the cold path and cannot live in the
//! > control region**, for the obvious reason that the control region is one of
//! > the two things being located. — design.md §4.3
//!
//! Hence [`ExtentDesc`] is a cold-path message type and lives in this crate
//! rather than in `sv-arena`. The split is worth stating once: `ExtentDesc`
//! describes *how to find* an extent and crosses the wire; `sv_arena::Extent`
//! and the run list describe *where it is* on one side and never do (§5.7,
//! "The run list does not cross the wire, and it never did have to").
//!
//! # `unsafe`
//!
//! One function pair, [`send_with_fd`] and [`recv_with_fd`], for `SCM_RIGHTS`.
//! Everything else in this module is safe code over `sv-wire`.

use crate::control::{Geometry, ProtocolIds, ENDIAN_MARKER};
use crate::{Error, Result};
use sv_wire::{Decoder, Encoder, pad4};

/// How the guest obtained the extent's pages. design.md §4.3:
///
/// > **`granularity` is asserted, never assumed.** The GPU side calls
/// > `QueryWorkingSetEx` on the extent and publishes the `LargePage` bit it
/// > actually observed, not the flag it passed to `VirtualAlloc`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum Granularity {
    /// 4 KiB pages, `VirtualAlloc` + `VirtualLock`. Measured at 1471 runs per
    /// 64 MiB, and the path on which the R26 canary is required.
    Small4K = 0,
    /// 2 MiB large pages. Measured at 22 runs per 64 MiB and **1 per GiB**, and
    /// the path on which R26 is removed by construction.
    Large2M = 1,
}

impl Granularity {
    pub fn from_u32(v: u32) -> Option<Granularity> {
        match v {
            0 => Some(Granularity::Small4K),
            1 => Some(Granularity::Large2M),
            _ => None,
        }
    }

    /// Bytes in one allocation unit of this granularity.
    pub fn unit(self) -> u64 {
        match self {
            Granularity::Small4K => 4096,
            Granularity::Large2M => 2 << 20,
        }
    }
}

/// One run of physically contiguous frames, as the AWE path would report them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PfnRun {
    pub first_frame: u64,
    pub frames: u64,
}

/// How the attaching side is to find the extent. design.md §5.1's table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Locator {
    /// Sweep the memfd for the stamp. **v1 on every path**: 1.8 s at 64 MiB,
    /// 2.02 s at 1 GiB, 2.32 s at 2 GiB, because the cost is linear in guest RAM
    /// and not in arena size — and it is the only locator that *verifies* rather
    /// than trusts.
    Scan,
    /// One record every 2 MiB. Sound only on the large-page path. Recorded in
    /// the design, not implemented in v1.
    Probe,
    /// A reported frame list, run-length encoded. AWE only, and AWE is not the
    /// path: 5863–16339 runs per 64 MiB against large pages' 22.
    Pfns(Vec<PfnRun>),
}

/// What crosses about an extent. design.md §4.3:
///
/// > `ExtentDesc { run_id, granularity, control_pages, arena_pages, locator }`.
/// > It is small in every case: on the two locating paths it carries no
/// > addresses at all.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtentDesc {
    pub run_id: u64,
    pub granularity: Granularity,
    pub control_pages: u64,
    pub arena_pages: u64,
    pub locator: Locator,
}

impl ExtentDesc {
    fn size(&self) -> usize {
        8 + 4 + 8 + 8
            + 4
            + match &self.locator {
                Locator::Scan | Locator::Probe => 8,
                Locator::Pfns(runs) => 8 + runs.len() * 16,
            }
    }

    fn encode(&self, e: &mut Encoder<'_>) -> Result<()> {
        e.u64(self.run_id)?;
        e.u32(self.granularity as u32)?;
        e.u64(self.control_pages)?;
        e.u64(self.arena_pages)?;
        match &self.locator {
            Locator::Scan => {
                e.u32(0)?;
                e.array_size(0)?;
            }
            Locator::Probe => {
                e.u32(1)?;
                e.array_size(0)?;
            }
            Locator::Pfns(runs) => {
                e.u32(2)?;
                e.array_size(runs.len() as u64)?;
                for r in runs {
                    e.u64(r.first_frame)?;
                    e.u64(r.frames)?;
                }
            }
        }
        Ok(())
    }

    fn decode(d: &mut Decoder<'_>) -> Result<ExtentDesc> {
        let run_id = d.u64()?;
        let g = d.u32()?;
        let granularity =
            Granularity::from_u32(g).ok_or(Error::BadColdMessage("unknown granularity"))?;
        let control_pages = d.u64()?;
        let arena_pages = d.u64()?;
        let tag = d.u32()?;
        // Branch on the tag *before* reading the count. Reading it with
        // `array_size` first would bounds-check it against the bytes remaining
        // and fail with `ImplausibleCount`, which is true but useless: the thing
        // actually wrong is that a locating path carried a frame list, and that
        // is what the person reading the log needs to be told.
        let locator = match tag {
            0 | 1 => {
                if d.u64()? != 0 {
                    return Err(Error::BadColdMessage("a locating path carried a frame list"));
                }
                if tag == 0 { Locator::Scan } else { Locator::Probe }
            }
            2 => {
                let n = d.array_size(16)?;
                let mut runs = Vec::with_capacity(n);
                for _ in 0..n {
                    runs.push(PfnRun { first_frame: d.u64()?, frames: d.u64()? });
                }
                Locator::Pfns(runs)
            }
            _ => return Err(Error::BadColdMessage("unknown locator")),
        };
        Ok(ExtentDesc { run_id, granularity, control_pages, arena_pages, locator })
    }
}

/// design.md §4.3 step 2, in full.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hello {
    pub ids: ProtocolIds,
    pub generation: u32,
    pub extent: ExtentDesc,
    pub geometry: Geometry,
    /// [`crate::control::ENDIAN_MARKER`] as the sender wrote it.
    pub endianness: u32,
    pub pointer_width: u32,
}

impl Hello {
    /// The check design.md §4.3 step 4 makes explicit, made here because a
    /// diagnostic that says "big-endian peer" is worth more than one that says
    /// "bad magic".
    pub fn check_against(&self, ours: ProtocolIds) -> Result<()> {
        if self.endianness != ENDIAN_MARKER || self.pointer_width != 64 {
            return Err(Error::IncompatiblePeer {
                endianness: self.endianness,
                pointer_width: self.pointer_width,
            });
        }
        if self.ids.layout_version != ours.layout_version {
            return Err(Error::LayoutVersionMismatch {
                ours: ours.layout_version,
                theirs: self.ids.layout_version,
            });
        }
        if self.ids.wire_version != ours.wire_version {
            return Err(Error::WireVersionMismatch {
                ours: ours.wire_version,
                theirs: self.ids.wire_version,
            });
        }
        if self.ids.protocol_hash != ours.protocol_hash {
            return Err(Error::ProtocolHashMismatch {
                ours: ours.protocol_hash,
                theirs: self.ids.protocol_hash,
            });
        }
        Ok(())
    }
}

/// What `sv-hostd` hands a client along with the memfd.
///
/// The run list is *not* in here: §5.7 says it is a side-local derived artefact,
/// and in the development loop it is the identity run anyway. What is in here is
/// the geometry of the two views the client is to build from the fd, which is
/// the only thing the client cannot work out for itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attachment {
    pub control_offset: u64,
    pub control_len: u64,
    pub arena_offset: u64,
    pub arena_len: u64,
    pub generation: u32,
    /// Runs the daemon stitched the view from. Zero means "the extent is one
    /// contiguous range of the fd", which is the development loop and is also
    /// what a 1 GiB large-page extent measured as (§5.7).
    pub run_count: u64,
}

/// Every message on the cold path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ColdMessage {
    Hello(Box<Hello>),
    Accept { generation: u32 },
    Refuse { code: u32, reason: String },
    /// `sv-hostd` → client, carrying the memfd out of band.
    Attach(Attachment),
    /// The client asks for one.
    AttachRequest,
    /// `svPing` / `svShutdown` of design.md §6.3, on the cold path where they
    /// belong: a ring cannot carry liveness for a peer whose rings are the thing
    /// in doubt.
    Ping,
    Shutdown,
    /// `sv-hostd` → **ICD**, following an [`ColdMessage::Attach`] whose
    /// `run_count` is non-zero: how to stitch the extent out of the descriptor
    /// that came with it.
    ///
    /// # This is a different boundary from the one §5.7 is about
    ///
    /// design.md §5.7 says, firmly, "**the run list does not cross the wire, and
    /// it never did have to**", and §4.3's `ExtentDesc` carries a *locator*
    /// rather than addresses. Both are about the **guest↔host** boundary, where
    /// the guest cannot know the runs — that is the entire reason `ScanLocator`
    /// exists.
    ///
    /// This is the **daemon↔ICD** boundary, and there the picture is the
    /// opposite: both processes are the app side, on one kernel, and design.md
    /// §3's own topology box says `sv-hostd` "hands **fd + runs** to the ICDs".
    /// It has to, and the reason is structural rather than a matter of taste:
    /// **a scattered extent has no single descriptor whose `mmap` reproduces
    /// it.** The daemon's contiguous view is many `mmap(MAP_FIXED)` calls into
    /// one reservation, and a view is not a file. So either the runs reach the
    /// client and it stitches its own — one `mmap` per run, the cost §5.7
    /// already budgets — or the daemon copies, which §1's second invariant
    /// forbids.
    ///
    /// `Attachment::run_count` was already in the protocol for this, with 0
    /// meaning "one contiguous range". What was missing was the message that
    /// carries the runs when it is not.
    Runs(Vec<AttachRun>),
}

/// One run of the daemon's stitched view, as the ICD needs it.
///
/// Deliberately *not* `sv_arena::extent::Run`: this is the wire form of a
/// same-machine handoff and that is the side-local derived type. They hold the
/// same three numbers, and keeping them separate is what stops the arena's
/// internal representation becoming protocol by accident.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AttachRun {
    /// Offset in the descriptor that came with the `Attach`.
    pub file_offset: u64,
    /// Offset within the extent — the number both sides agree on (§1).
    pub extent_offset: u64,
    pub len: u64,
}

const TAG_HELLO: u32 = 1;
const TAG_ACCEPT: u32 = 2;
const TAG_REFUSE: u32 = 3;
const TAG_ATTACH: u32 = 4;
const TAG_ATTACH_REQUEST: u32 = 5;
const TAG_PING: u32 = 6;
const TAG_SHUTDOWN: u32 = 7;
const TAG_RUNS: u32 = 8;

/// The most runs one `Runs` message may carry.
///
/// A 2 GiB arena of 4 KiB guest pages measured ~47,000 runs (§5.7), and each is
/// 24 bytes, so the message is ~1.1 MiB — which is why [`MAX_COLD_MESSAGE`] is
/// 1 MiB and this is checked rather than assumed. Beyond it the daemon refuses
/// rather than truncating: half a run list is an arena with a hole in it.
pub const MAX_ATTACH_RUNS: usize = (MAX_COLD_MESSAGE - 16) / 24;

impl ColdMessage {
    fn size(&self) -> usize {
        4 + match self {
            ColdMessage::Hello(h) => 4 + 4 + 8 + 4 + h.extent.size() + 8 * 3 + 4 * 3 + 4 + 4,
            ColdMessage::Accept { .. } => 4,
            ColdMessage::Refuse { reason, .. } => 4 + 8 + pad4(reason.len() + 1),
            ColdMessage::Attach(_) => 8 * 4 + 4 + 8,
            ColdMessage::Runs(r) => 8 + r.len() * 24,
            ColdMessage::AttachRequest | ColdMessage::Ping | ColdMessage::Shutdown => 0,
        }
    }

    /// Encode into `out`, replacing its contents. Uses `sv-wire`'s exact-fill
    /// encoder, so a `size`/`encode` disagreement is caught here rather than
    /// producing a message the far side silently mis-parses — the same
    /// discipline the generated protocol is held to (design.md §7.2).
    pub fn encode(&self, out: &mut Vec<u8>) -> Result<()> {
        out.clear();
        out.resize(self.size(), 0);
        let mut e = Encoder::new(out);
        match self {
            ColdMessage::Hello(h) => {
                e.u32(TAG_HELLO)?;
                e.u32(h.ids.layout_version)?;
                e.u32(h.ids.wire_version)?;
                e.u64(h.ids.protocol_hash)?;
                e.u32(h.generation)?;
                h.extent.encode(&mut e)?;
                e.u64(h.geometry.control_size)?;
                e.u64(h.geometry.arena_size)?;
                e.u64(h.geometry.arena_slab_size)?;
                e.u32(h.geometry.ring_slot_count)?;
                e.u32(h.geometry.ring_data_bytes)?;
                e.u32(h.geometry.timeline_count)?;
                e.u32(h.endianness)?;
                e.u32(h.pointer_width)?;
            }
            ColdMessage::Accept { generation } => {
                e.u32(TAG_ACCEPT)?;
                e.u32(*generation)?;
            }
            ColdMessage::Refuse { code, reason } => {
                e.u32(TAG_REFUSE)?;
                e.u32(*code)?;
                e.c_string(reason.as_bytes())?;
            }
            ColdMessage::Attach(a) => {
                e.u32(TAG_ATTACH)?;
                e.u64(a.control_offset)?;
                e.u64(a.control_len)?;
                e.u64(a.arena_offset)?;
                e.u64(a.arena_len)?;
                e.u32(a.generation)?;
                e.u64(a.run_count)?;
            }
            ColdMessage::Runs(runs) => {
                e.u32(TAG_RUNS)?;
                e.u64(runs.len() as u64)?;
                for r in runs {
                    e.u64(r.file_offset)?;
                    e.u64(r.extent_offset)?;
                    e.u64(r.len)?;
                }
            }
            ColdMessage::AttachRequest => e.u32(TAG_ATTACH_REQUEST)?,
            ColdMessage::Ping => e.u32(TAG_PING)?,
            ColdMessage::Shutdown => e.u32(TAG_SHUTDOWN)?,
        }
        e.finish()?;
        Ok(())
    }

    pub fn decode(bytes: &[u8]) -> Result<ColdMessage> {
        let mut d = Decoder::new(bytes);
        let msg = match d.u32()? {
            TAG_HELLO => {
                let ids = ProtocolIds {
                    layout_version: d.u32()?,
                    wire_version: d.u32()?,
                    protocol_hash: d.u64()?,
                };
                let generation = d.u32()?;
                let extent = ExtentDesc::decode(&mut d)?;
                let geometry = Geometry {
                    control_size: d.u64()?,
                    arena_size: d.u64()?,
                    arena_slab_size: d.u64()?,
                    ring_slot_count: d.u32()?,
                    ring_data_bytes: d.u32()?,
                    timeline_count: d.u32()?,
                };
                ColdMessage::Hello(Box::new(Hello {
                    ids,
                    generation,
                    extent,
                    geometry,
                    endianness: d.u32()?,
                    pointer_width: d.u32()?,
                }))
            }
            TAG_ACCEPT => ColdMessage::Accept { generation: d.u32()? },
            TAG_REFUSE => {
                let code = d.u32()?;
                let reason = String::from_utf8_lossy(d.c_string()?).into_owned();
                ColdMessage::Refuse { code, reason }
            }
            TAG_ATTACH => ColdMessage::Attach(Attachment {
                control_offset: d.u64()?,
                control_len: d.u64()?,
                arena_offset: d.u64()?,
                arena_len: d.u64()?,
                generation: d.u32()?,
                run_count: d.u64()?,
            }),
            TAG_RUNS => {
                let n = d.u64()?;
                // Bound before allocating. A corrupted or hostile length here is
                // R2's shape — `n` comes off the wire and `Vec::with_capacity`
                // would take it at its word.
                let n = usize::try_from(n)
                    .ok()
                    .filter(|n| *n <= MAX_ATTACH_RUNS)
                    .ok_or(Error::BadColdMessage("implausible run count"))?;
                let mut runs = Vec::with_capacity(n);
                for _ in 0..n {
                    runs.push(AttachRun {
                        file_offset: d.u64()?,
                        extent_offset: d.u64()?,
                        len: d.u64()?,
                    });
                }
                ColdMessage::Runs(runs)
            }
            TAG_ATTACH_REQUEST => ColdMessage::AttachRequest,
            TAG_PING => ColdMessage::Ping,
            TAG_SHUTDOWN => ColdMessage::Shutdown,
            _ => return Err(Error::BadColdMessage("unknown message tag")),
        };
        if d.remaining() != 0 {
            return Err(Error::BadColdMessage("trailing bytes"));
        }
        Ok(msg)
    }
}

// ---------------------------------------------------------------------------
// The socket
// ---------------------------------------------------------------------------

/// The largest cold-path message we will read. The cold path carries a
/// handshake, never data (D10), so this is generous by two orders of magnitude
/// and exists so that a corrupt length is an error rather than an allocation.
pub const MAX_COLD_MESSAGE: usize = 1 << 20;

#[cfg(unix)]
mod unix_socket {
    use super::*;
    use std::io::{Read, Write};
    use std::os::fd::{AsRawFd, BorrowedFd, FromRawFd, OwnedFd};
    use std::os::unix::net::UnixStream;

    /// Send a message, optionally passing a file descriptor with it.
    ///
    /// # Safety
    ///
    /// The `unsafe` here is one `sendmsg` with a hand-built `msghdr` and one
    /// `SCM_RIGHTS` control message. The invariants:
    ///
    /// - The `iovec` points at `framed`, which outlives the call.
    /// - The control buffer is `CMSG_SPACE(4)` bytes — 24 on a 64-bit Linux —
    ///   and `cmsg_len` is set to `CMSG_LEN(4)`, so the kernel reads exactly one
    ///   `int` from it.
    /// - The descriptor is *borrowed*: the kernel duplicates it into the
    ///   receiver, and this side's ownership is unaffected.
    ///
    /// The layout of `msghdr`, `iovec` and `cmsghdr` is transcribed for 64-bit
    /// Linux, which is asserted at compile time below rather than assumed.
    pub fn send_with_fd(
        sock: &UnixStream,
        payload: &[u8],
        fd: Option<BorrowedFd<'_>>,
    ) -> Result<()> {
        match fd {
            Some(f) => send_with_fds(sock, payload, &[f]),
            None => send_with_fds(sock, payload, &[]),
        }
    }

    /// The most descriptors one cold-path message may carry.
    ///
    /// Two, and the reason is the GPU-side arena provider: when the driver
    /// allocates the arena and exports it, the control region stays a memfd —
    /// §4.1 splits the two regions *by purpose*, and the control region has no
    /// reason to be GPU memory — so the client needs one descriptor for each.
    /// The memfd provider still sends one, because there both regions are the
    /// same file.
    pub const MAX_FDS: usize = 2;

    /// Send a message, passing up to [`MAX_FDS`] descriptors with it.
    ///
    /// # Safety
    ///
    /// One `sendmsg` with a hand-built `msghdr` and one `SCM_RIGHTS` control
    /// message. The invariants:
    ///
    /// - The `iovec` points at `framed`, which outlives the call.
    /// - The control buffer is `CMSG_SPACE(4 * MAX_FDS)` bytes — 24 on a 64-bit
    ///   Linux, which is the same 24 that one descriptor already needed, because
    ///   `CMSG_SPACE` rounds up to 8 — and `cmsg_len` is set to
    ///   `CMSG_LEN(4 * n)`, so the kernel reads exactly `n` `int`s from it.
    /// - The descriptors are *borrowed*: the kernel duplicates them into the
    ///   receiver, and this side's ownership is unaffected.
    ///
    /// The layout of `msghdr`, `iovec` and `cmsghdr` is transcribed for 64-bit
    /// Linux, which is asserted at compile time below rather than assumed.
    pub fn send_with_fds(
        sock: &UnixStream,
        payload: &[u8],
        fds: &[BorrowedFd<'_>],
    ) -> Result<()> {
        if fds.len() > MAX_FDS {
            return Err(Error::BadColdMessage("too many descriptors for one cold-path message"));
        }
        let mut framed = Vec::with_capacity(payload.len() + 4);
        framed.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        framed.extend_from_slice(payload);

        if fds.is_empty() {
            sock.try_clone()
                .and_then(|mut s| s.write_all(&framed))
                .map_err(|e| Error::Os { call: "write", errno: e.raw_os_error().unwrap_or(0) })?;
            return Ok(());
        }

        let mut iov = sys::Iovec { base: framed.as_ptr() as *mut u8, len: framed.len() };
        let mut cbuf = [0u8; sys::CMSG_SPACE_MAX_FDS];
        // cmsg_len, cmsg_level, cmsg_type, then the descriptors.
        let cmsg_len = sys::CMSG_LEN_HDR + 4 * fds.len();
        cbuf[0..8].copy_from_slice(&(cmsg_len as u64).to_le_bytes());
        cbuf[8..12].copy_from_slice(&sys::SOL_SOCKET.to_le_bytes());
        cbuf[12..16].copy_from_slice(&sys::SCM_RIGHTS.to_le_bytes());
        for (i, f) in fds.iter().enumerate() {
            let at = 16 + 4 * i;
            cbuf[at..at + 4].copy_from_slice(&f.as_raw_fd().to_le_bytes());
        }

        let mut msg = sys::MsgHdr {
            name: core::ptr::null_mut(),
            namelen: 0,
            _pad0: 0,
            iov: &mut iov,
            iovlen: 1,
            control: cbuf.as_mut_ptr() as *mut core::ffi::c_void,
            controllen: sys::cmsg_space(fds.len()),
            flags: 0,
            _pad1: 0,
        };
        // SAFETY: as documented above. `sendmsg` reads through the pointers for
        // the duration of the call only, and all three buffers are live locals.
        let n = unsafe { sys::sendmsg(sock.as_raw_fd(), &mut msg, 0) };
        if n < 0 {
            return Err(Error::Os { call: "sendmsg", errno: crate::region::sys::errno() });
        }
        // A short send of the header would desynchronise the stream, so it is an
        // error rather than something to loop over: on a Unix socket with a
        // message this small it cannot happen, and if it does the assumption is
        // wrong and we want to know.
        if (n as usize) != framed.len() {
            return Err(Error::BadColdMessage("short sendmsg"));
        }
        Ok(())
    }

    /// Receive one message, and the descriptor that came with it if any.
    ///
    /// # Safety
    ///
    /// One `recvmsg`, with `MSG_CMSG_CLOEXEC` so a descriptor cannot leak
    /// through an `exec` between arriving and being wrapped. The returned fd is
    /// wrapped in an [`OwnedFd`] immediately, so it is closed exactly once.
    ///
    /// The first `recvmsg` asks for the four-byte length prefix only. On a
    /// `SOCK_STREAM` Unix socket the ancillary data is delivered with the read
    /// that consumes the first byte it was attached to, and the sender attaches
    /// it to the same `sendmsg` as the prefix — so the fd arrives here or not at
    /// all.
    pub fn recv_with_fd(sock: &UnixStream, out: &mut Vec<u8>) -> Result<Option<OwnedFd>> {
        Ok(recv_with_fds(sock, out)?.into_iter().next())
    }

    /// Receive one message, and every descriptor that came with it.
    ///
    /// # Safety
    ///
    /// One `recvmsg`, with `MSG_CMSG_CLOEXEC` so a descriptor cannot leak
    /// through an `exec` between arriving and being wrapped. Each returned fd is
    /// wrapped in an [`OwnedFd`] immediately, so it is closed exactly once.
    ///
    /// The first `recvmsg` asks for the four-byte length prefix only. On a
    /// `SOCK_STREAM` Unix socket the ancillary data is delivered with the read
    /// that consumes the first byte it was attached to, and the sender attaches
    /// it to the same `sendmsg` as the prefix — so the descriptors arrive here or
    /// not at all.
    pub fn recv_with_fds(sock: &UnixStream, out: &mut Vec<u8>) -> Result<Vec<OwnedFd>> {
        let mut lenbuf = [0u8; 4];
        let mut iov = sys::Iovec { base: lenbuf.as_mut_ptr(), len: 4 };
        let mut cbuf = [0u8; sys::CMSG_SPACE_MAX_FDS];
        let mut msg = sys::MsgHdr {
            name: core::ptr::null_mut(),
            namelen: 0,
            _pad0: 0,
            iov: &mut iov,
            iovlen: 1,
            control: cbuf.as_mut_ptr() as *mut core::ffi::c_void,
            controllen: sys::CMSG_SPACE_MAX_FDS,
            flags: 0,
            _pad1: 0,
        };
        // SAFETY: as documented. All buffers are live locals; the kernel writes
        // at most `iov.len` payload bytes and at most `controllen` control bytes.
        let n = unsafe { sys::recvmsg(sock.as_raw_fd(), &mut msg, sys::MSG_CMSG_CLOEXEC) };
        if n < 0 {
            return Err(Error::Os { call: "recvmsg", errno: crate::region::sys::errno() });
        }
        if n == 0 {
            return Err(Error::BadColdMessage("peer closed the cold path"));
        }

        let mut fds = Vec::new();
        if msg.controllen >= sys::CMSG_LEN_HDR + 4 {
            let cmsg_len = u64::from_le_bytes(cbuf[0..8].try_into().unwrap()) as usize;
            let level = i32::from_le_bytes(cbuf[8..12].try_into().unwrap());
            let ty = i32::from_le_bytes(cbuf[12..16].try_into().unwrap());
            if level == sys::SOL_SOCKET && ty == sys::SCM_RIGHTS {
                // How many descriptors the kernel actually delivered, from the
                // header it wrote — not from how many we hoped for. A truncated
                // `cmsg_len` would otherwise have us wrap uninitialised bytes as
                // descriptor numbers.
                let n = cmsg_len.saturating_sub(sys::CMSG_LEN_HDR) / 4;
                for i in 0..n.min(MAX_FDS) {
                    let at = 16 + 4 * i;
                    let raw = i32::from_le_bytes(cbuf[at..at + 4].try_into().unwrap());
                    // SAFETY: the kernel installed `raw` in this process's table
                    // and nobody else has it; wrapping it here is the unique
                    // owner.
                    fds.push(unsafe { OwnedFd::from_raw_fd(raw) });
                }
            }
        }

        let mut have = n as usize;
        let mut s = sock;
        while have < 4 {
            let got = s
                .read(&mut lenbuf[have..])
                .map_err(|e| Error::Os { call: "read", errno: e.raw_os_error().unwrap_or(0) })?;
            if got == 0 {
                return Err(Error::BadColdMessage("truncated cold-path length"));
            }
            have += got;
        }
        let len = u32::from_le_bytes(lenbuf) as usize;
        if len > MAX_COLD_MESSAGE {
            return Err(Error::BadColdMessage("cold-path message is implausibly long"));
        }
        out.clear();
        out.resize(len, 0);
        s.read_exact(out)
            .map_err(|e| Error::Os { call: "read_exact", errno: e.raw_os_error().unwrap_or(0) })?;
        Ok(fds)
    }

    /// Send a [`ColdMessage`], with no descriptor.
    pub fn send(sock: &UnixStream, msg: &ColdMessage) -> Result<()> {
        let mut buf = Vec::new();
        msg.encode(&mut buf)?;
        send_with_fd(sock, &buf, None)
    }

    /// Receive a [`ColdMessage`], and the descriptor that came with it if any.
    pub fn recv(sock: &UnixStream) -> Result<(ColdMessage, Option<OwnedFd>)> {
        let mut buf = Vec::new();
        let fd = recv_with_fd(sock, &mut buf)?;
        Ok((ColdMessage::decode(&buf)?, fd))
    }

    /// Receive a [`ColdMessage`] and every descriptor it carried.
    pub fn recv_all(sock: &UnixStream) -> Result<(ColdMessage, Vec<OwnedFd>)> {
        let mut buf = Vec::new();
        let fds = recv_with_fds(sock, &mut buf)?;
        Ok((ColdMessage::decode(&buf)?, fds))
    }

    /// The three structures `sendmsg` needs, transcribed for 64-bit Linux.
    pub(super) mod sys {
        use core::ffi::c_void;

        pub const SOL_SOCKET: i32 = 1;
        pub const SCM_RIGHTS: i32 = 1;
        pub const MSG_CMSG_CLOEXEC: i32 = 0x4000_0000;

        /// The `cmsghdr` itself: `cmsg_len` (8), `cmsg_level` (4), `cmsg_type` (4).
        pub const CMSG_LEN_HDR: usize = 16;
        /// `CMSG_SPACE(sizeof(int) * MAX_FDS)`.
        ///
        /// Two descriptors need 16 + 8 = 24 bytes, which is the same 24 one
        /// descriptor needed, because `CMSG_SPACE` rounds the data up to 8. That
        /// coincidence is why widening the cold path to two costs no buffer
        /// growth — it is checked by `two_fds_need_no_more_space_than_one`.
        pub const CMSG_SPACE_MAX_FDS: usize = 16 + 8;

        /// `CMSG_SPACE(sizeof(int) * n)`.
        pub const fn cmsg_space(n: usize) -> usize {
            CMSG_LEN_HDR + (4 * n).div_ceil(8) * 8
        }

        #[repr(C)]
        pub struct Iovec {
            pub base: *mut u8,
            pub len: usize,
        }

        #[repr(C)]
        pub struct MsgHdr {
            pub name: *mut c_void,
            pub namelen: u32,
            pub _pad0: u32,
            pub iov: *mut Iovec,
            pub iovlen: usize,
            pub control: *mut c_void,
            pub controllen: usize,
            pub flags: i32,
            pub _pad1: i32,
        }

        // The layout is the ABI's, not ours. If either of these ever fails, the
        // transcription above is wrong for the target and the fd handoff would
        // corrupt memory rather than fail — so it is a compile error.
        const _: () = assert!(core::mem::size_of::<MsgHdr>() == 56);
        const _: () = assert!(core::mem::size_of::<Iovec>() == 16);
        const _: () = assert!(core::mem::size_of::<usize>() == 8);

        unsafe extern "C" {
            pub fn sendmsg(fd: i32, msg: *mut MsgHdr, flags: i32) -> isize;
            pub fn recvmsg(fd: i32, msg: *mut MsgHdr, flags: i32) -> isize;
        }
    }
}

#[cfg(unix)]
pub use unix_socket::{
    MAX_FDS, recv, recv_all, recv_with_fd, recv_with_fds, send, send_with_fd, send_with_fds,
};

/// On Windows the cold path is a named pipe, and it is not written.
///
/// Stated rather than stubbed: design.md requires either side to be either OS,
/// and `testhost` exercises Linux-app-side only. A named-pipe implementation
/// of exactly these four functions is what a Windows app side needs, and
/// nothing above this module would change.
#[cfg(not(unix))]
pub fn send(_sock: &(), _msg: &ColdMessage) -> Result<()> {
    Err(Error::Unsupported("the cold-path socket is Unix-only in this build"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hello() -> ColdMessage {
        ColdMessage::Hello(Box::new(Hello {
            ids: ProtocolIds::new(3, 0x0123_4567_89ab_cdef),
            generation: 7,
            extent: ExtentDesc {
                run_id: 0xdead_beef_cafe_f00d,
                granularity: Granularity::Large2M,
                control_pages: 4096,
                arena_pages: 524288,
                locator: Locator::Scan,
            },
            geometry: Geometry::default(),
            endianness: ENDIAN_MARKER,
            pointer_width: 64,
        }))
    }

    #[test]
    fn every_message_round_trips() {
        let msgs = [
            hello(),
            ColdMessage::Accept { generation: 7 },
            ColdMessage::Refuse { code: 2, reason: "protocol hash mismatch".into() },
            ColdMessage::Attach(Attachment {
                control_offset: 0,
                control_len: 16 << 20,
                arena_offset: 16 << 20,
                arena_len: 2 << 30,
                generation: 7,
                run_count: 1,
            }),
            ColdMessage::AttachRequest,
            ColdMessage::Ping,
            ColdMessage::Shutdown,
        ];
        let mut buf = Vec::new();
        for m in &msgs {
            m.encode(&mut buf).unwrap();
            assert_eq!(&ColdMessage::decode(&buf).unwrap(), m, "{m:?}");
        }
        assert_eq!(msgs.len(), 7, "every variant must appear above");
    }

    /// design.md §4.3: the descriptor "is small in every case: on the two
    /// locating paths it carries no addresses at all". Worth an assertion,
    /// because the superseded design *did* put a page list on the wire and the
    /// reason it no longer does is a measurement, not a preference.
    #[test]
    fn a_scan_locator_carries_no_addresses_whatever_the_extent_size() {
        let small = ExtentDesc {
            run_id: 1,
            granularity: Granularity::Large2M,
            control_pages: 1,
            arena_pages: 1,
            locator: Locator::Scan,
        };
        let huge = ExtentDesc { arena_pages: 524_288, ..small.clone() };
        assert_eq!(small.size(), huge.size());
        assert!(small.size() < 64, "an ExtentDesc is a handful of words");

        // …and the AWE path is the counter-example that shows the saving is
        // real: 5863 runs per 64 MiB, measured, at 16 bytes each.
        let awe = ExtentDesc {
            locator: Locator::Pfns(vec![PfnRun { first_frame: 1, frames: 1 }; 5863]),
            ..small.clone()
        };
        assert!(awe.size() > 90_000);
    }

    #[test]
    fn a_locating_path_that_carries_a_frame_list_is_refused() {
        // Hand-built, because `encode` will not produce one.
        let mut buf = vec![0u8; 40];
        let mut e = Encoder::new(&mut buf);
        e.u64(1).unwrap(); // run_id
        e.u32(0).unwrap(); // Small4K
        e.u64(1).unwrap();
        e.u64(1).unwrap();
        e.u32(0).unwrap(); // Locator::Scan
        e.array_size(1).unwrap(); // …with a run list
        e.finish().unwrap();
        let mut d = Decoder::new(&buf);
        assert!(matches!(ExtentDesc::decode(&mut d), Err(Error::BadColdMessage(_))));
    }

    #[test]
    fn a_hello_from_a_big_endian_or_32_bit_peer_is_refused_by_name() {
        let ours = ProtocolIds::new(3, 0x0123_4567_89ab_cdef);
        let ColdMessage::Hello(h) = hello() else { unreachable!() };
        h.check_against(ours).unwrap();

        let mut be = h.clone();
        be.endianness = ENDIAN_MARKER.swap_bytes();
        assert!(matches!(be.check_against(ours), Err(Error::IncompatiblePeer { .. })));

        let mut narrow = h.clone();
        narrow.pointer_width = 32;
        assert!(matches!(narrow.check_against(ours), Err(Error::IncompatiblePeer { .. })));

        let mut skew = h.clone();
        skew.ids.protocol_hash = 1;
        assert!(matches!(skew.check_against(ours), Err(Error::ProtocolHashMismatch { .. })));
    }

    #[test]
    fn a_message_with_trailing_bytes_is_refused() {
        let mut buf = Vec::new();
        ColdMessage::Ping.encode(&mut buf).unwrap();
        buf.extend_from_slice(&[0, 0, 0, 0]);
        assert!(matches!(ColdMessage::decode(&buf), Err(Error::BadColdMessage(_))));
    }

    #[test]
    fn an_unknown_tag_is_refused_rather_than_ignored() {
        let mut buf = vec![0u8; 4];
        Encoder::new(&mut buf).u32(999).unwrap();
        assert!(matches!(ColdMessage::decode(&buf), Err(Error::BadColdMessage(_))));
    }

    #[cfg(unix)]
    #[test]
    #[cfg_attr(miri, ignore = "socketpair and sendmsg are syscalls Miri does not implement")]
    fn a_descriptor_crosses_the_socket_and_names_the_same_memory() {
        use crate::region::{SharedRegion, memfd};
        use std::os::fd::AsFd;
        use std::os::unix::net::UnixStream;

        let (a, b) = UnixStream::pair().unwrap();
        let fd = memfd("sv-cold-test", 8192).unwrap();
        let msg = ColdMessage::Attach(Attachment {
            control_offset: 0,
            control_len: 4096,
            arena_offset: 4096,
            arena_len: 4096,
            generation: 1,
            run_count: 0,
        });
        let mut buf = Vec::new();
        msg.encode(&mut buf).unwrap();
        send_with_fd(&a, &buf, Some(fd.as_fd())).unwrap();

        let (got, got_fd) = recv(&b).unwrap();
        assert_eq!(got, msg);
        let got_fd = got_fd.expect("the descriptor did not cross");

        // The proof that it is the *same* memfd and not merely a descriptor: a
        // write through the sender's mapping is visible through the receiver's.
        let here = SharedRegion::map_fd(fd.as_fd(), 0, 4096).unwrap();
        let there = SharedRegion::map_fd(got_fd.as_fd(), 0, 4096).unwrap();
        here.write_bytes(128, b"crossed").unwrap();
        let mut out = [0u8; 7];
        there.read_bytes(128, &mut out).unwrap();
        assert_eq!(&out, b"crossed");
    }

    /// The GPU-allocated arena needs two descriptors, not one.
    ///
    /// On that path the control region stays an ordinary memfd — §4.1 splits the
    /// two regions by purpose and the control region has no reason to be GPU
    /// memory — while the arena is the driver's own allocation, exported. They
    /// are two kernel objects and both have to cross in one message, because the
    /// client cannot attach to one without the other.
    #[cfg(unix)]
    #[test]
    #[cfg_attr(miri, ignore = "socketpair and sendmsg are syscalls Miri does not implement")]
    fn two_descriptors_cross_together_and_stay_distinct() {
        use crate::region::{SharedRegion, memfd};
        use std::os::fd::AsFd;
        use std::os::unix::net::UnixStream;

        let (a, b) = UnixStream::pair().unwrap();
        let control = memfd("sv-cold-control", 4096).unwrap();
        let arena = memfd("sv-cold-arena", 4096).unwrap();
        let msg = ColdMessage::Attach(Attachment {
            control_offset: 0,
            control_len: 4096,
            // Zero, because on this path the arena is its own object and begins
            // at *its* offset zero — which is how a client distinguishes the two
            // providers without a flag.
            arena_offset: 0,
            arena_len: 4096,
            generation: 7,
            run_count: 0,
        });
        let mut buf = Vec::new();
        msg.encode(&mut buf).unwrap();
        send_with_fds(&a, &buf, &[control.as_fd(), arena.as_fd()]).unwrap();

        let (got, fds) = recv_all(&b).unwrap();
        assert_eq!(got, msg);
        assert_eq!(fds.len(), 2, "both descriptors must arrive, in order");

        // They must be *different* objects: a receiver that got the same fd
        // twice would map the control region as the arena and the ring
        // directory would be the first thing an allocation overwrote.
        let c_here = SharedRegion::map_fd(control.as_fd(), 0, 4096).unwrap();
        let a_there = SharedRegion::map_fd(fds[1].as_fd(), 0, 4096).unwrap();
        let c_there = SharedRegion::map_fd(fds[0].as_fd(), 0, 4096).unwrap();
        c_here.write_bytes(0, b"CONTROL").unwrap();
        let mut out = [0u8; 7];
        c_there.read_bytes(0, &mut out).unwrap();
        assert_eq!(&out, b"CONTROL", "the first descriptor is the control region");
        a_there.read_bytes(0, &mut out).unwrap();
        assert_eq!(&out, &[0u8; 7], "the arena must not be the control region");
    }

    /// One descriptor still works, because `sv-hostd` still sends one.
    #[cfg(unix)]
    #[test]
    #[cfg_attr(miri, ignore = "socketpair and sendmsg are syscalls Miri does not implement")]
    fn one_descriptor_is_still_one_descriptor() {
        use crate::region::memfd;
        use std::os::fd::AsFd;
        use std::os::unix::net::UnixStream;

        let (a, b) = UnixStream::pair().unwrap();
        let fd = memfd("sv-cold-single", 4096).unwrap();
        let mut buf = Vec::new();
        ColdMessage::Ping.encode(&mut buf).unwrap();
        send_with_fds(&a, &buf, &[fd.as_fd()]).unwrap();
        let (_, fds) = recv_all(&b).unwrap();
        assert_eq!(fds.len(), 1);
    }

    #[test]
    fn two_descriptors_need_no_more_control_space_than_one() {
        // `CMSG_SPACE` rounds the data up to 8, so one `int` and two `int`s
        // occupy the same 24 bytes. The buffer size is therefore unchanged by
        // widening the cold path, which is worth pinning rather than
        // rediscovering the next time it widens to three.
        assert_eq!(unix_socket::sys::cmsg_space(1), unix_socket::sys::CMSG_SPACE_MAX_FDS);
        assert_eq!(unix_socket::sys::cmsg_space(2), unix_socket::sys::CMSG_SPACE_MAX_FDS);
        // Three would not fit, which is what `MAX_FDS` is for.
        assert!(unix_socket::sys::cmsg_space(3) > unix_socket::sys::CMSG_SPACE_MAX_FDS);
    }

    #[cfg(unix)]
    #[test]
    #[cfg_attr(miri, ignore = "socketpair is a syscall Miri does not implement")]
    fn a_message_without_a_descriptor_carries_none() {
        use std::os::unix::net::UnixStream;
        let (a, b) = UnixStream::pair().unwrap();
        send(&a, &ColdMessage::Ping).unwrap();
        let (msg, fd) = recv(&b).unwrap();
        assert_eq!(msg, ColdMessage::Ping);
        assert!(fd.is_none(), "a descriptor arrived that nobody sent");
    }
}
