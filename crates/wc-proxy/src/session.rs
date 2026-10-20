//! One client connection's worth of translation.
//!
//! Deliberately free of sockets. Everything here is "bytes and descriptors in,
//! bytes and descriptors out", which is what lets the whole translation —
//! including the pool path that is the point of the stage — be driven by tests
//! rather than only by a running compositor. `wc-shmclient` was split the same
//! way in stage 3 and for the same reason (docs/waycompose-design.md §8.9): the encoding
//! is the part most likely to be quietly wrong and it is testable without a
//! socket, so it lives where the tests can reach it.
//!
//! # What this does to each direction
//!
//! **Client → compositor.** Walk the message. If it carries an `h`, take the
//! next descriptor off the fd queue and replace it with an in-band
//! [`wc_handle::Descriptor`] naming memory the compositor can reach on its own.
//! Otherwise copy it through untouched. Track `new_id`s so the next message can
//! be walked. On `wl_surface.commit`, copy the pixels the client has just
//! finished drawing into the shared region **before** the commit is forwarded.
//!
//! **Compositor → client.** The mirror image, and it is not hypothetical: an
//! `h` travels this way too (`wl_keyboard.keymap`, `wl_data_source.send`). The
//! in-band descriptor is removed from the body and a real file descriptor is
//! materialised locally from the *name* it carried, then sent by `SCM_RIGHTS`.
//! That fd is created on this side out of memory this side can already reach,
//! which is the distinction the rule turns on: a descriptor *passed* is
//! forbidden, a descriptor *created from a name that crossed* is the sanctioned
//! pattern.
//!
//! # The one thing that is not forwarded
//!
//! Nothing. Every message is forwarded, including the ones this file has no
//! idea about. That is the property that makes the proxy worth building rather
//! than a set of protocol implementations: `docs/goal-firefox.md` needs
//! `xdg-decoration`, `wp_viewporter`, `wl_data_device` and whatever else Firefox
//! turns out to bind, and none of them need code here unless they carry an `h`.

use std::collections::HashMap;
use std::io;
use std::os::fd::{AsRawFd as _, OwnedFd};
use std::time::{Duration, Instant};

use wc_handle::Descriptor;

use crate::clip::{Answer, Source};
use crate::objects::Objects;
use crate::pool::{CopyScope, PoolTransport, Region};
use crate::spec::{ArgKind, Spec};
use crate::wire::{self, FdEncoding, Header, Span};

/// A rectangle in buffer pixels.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

impl Rect {
    fn clip(self, w: i32, h: i32) -> Option<Rect> {
        let x0 = self.x.max(0);
        let y0 = self.y.max(0);
        let x1 = self.x.saturating_add(self.w).min(w);
        let y1 = self.y.saturating_add(self.h).min(h);
        (x1 > x0 && y1 > y0).then(|| Rect { x: x0, y: y0, w: x1 - x0, h: y1 - y0 })
    }
}

/// A read-only mapping of the client's own pool object.
///
/// This is the fd the client sent us, mapped here and **nowhere else**. It is
/// the whole of what the local `SCM_RIGHTS` buys, and it is legal precisely
/// because it is created and consumed on this side of the boundary.
struct ClientMap {
    fd: std::os::fd::OwnedFd,
    ptr: *mut libc::c_void,
    len: usize,
}

impl ClientMap {
    fn new(fd: std::os::fd::OwnedFd, len: usize) -> io::Result<ClientMap> {
        use std::os::fd::AsRawFd as _;
        if len == 0 {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "a pool cannot be empty"));
        }
        // SAFETY: a kernel-chosen address, a non-zero length, and a live fd
        // that outlives the call. `MAP_SHARED` because the client keeps drawing
        // into it and a private copy would freeze the first frame forever —
        // which is a bug that looks exactly like a working compositor showing a
        // static window.
        let ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ,
                libc::MAP_SHARED,
                fd.as_raw_fd(),
                0,
            )
        };
        if ptr == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }
        Ok(ClientMap { fd, ptr, len })
    }

    fn as_slice(&self) -> &[u8] {
        // SAFETY: `ptr`/`len` are what `mmap` returned and the mapping lives as
        // long as `self`.
        //
        // The honest caveat, which is the same one libwayland has and solves
        // with a `SIGBUS` handler: the client can `ftruncate` its pool
        // *smaller* at any moment and fault this read. `remap` below re-checks
        // the object's real size on every resize, which closes the case a
        // cooperative client can produce; a hostile one can still do it and
        // this proxy would die where libwayland-server would recover. Recorded
        // in docs/waycompose-design.md §9 as an unclosed gap rather than hidden here.
        unsafe { std::slice::from_raw_parts(self.ptr as *const u8, self.len) }
    }

    /// Re-map after `wl_shm_pool.resize`.
    fn remap(&mut self, len: usize) -> io::Result<()> {
        use std::os::fd::AsRawFd as _;
        // SAFETY: `ptr`/`len` are exactly what the previous `mmap` returned.
        let ptr = unsafe {
            libc::mremap(self.ptr, self.len, len, libc::MREMAP_MAYMOVE)
        };
        if ptr == libc::MAP_FAILED {
            // `mremap` on a file mapping can legitimately fail; fall back to a
            // fresh mapping rather than leaving the pool unusable.
            // SAFETY: a fresh anonymous-address request against a live fd.
            let fresh = unsafe {
                libc::mmap(
                    std::ptr::null_mut(),
                    len,
                    libc::PROT_READ,
                    libc::MAP_SHARED,
                    self.fd.as_raw_fd(),
                    0,
                )
            };
            if fresh == libc::MAP_FAILED {
                return Err(io::Error::last_os_error());
            }
            // SAFETY: unmapping the old mapping exactly once.
            unsafe { libc::munmap(self.ptr, self.len) };
            self.ptr = fresh;
        } else {
            self.ptr = ptr;
        }
        self.len = len;
        Ok(())
    }
}

impl Drop for ClientMap {
    fn drop(&mut self) {
        // SAFETY: exactly what `mmap` returned, unmapped once.
        unsafe {
            libc::munmap(self.ptr, self.len);
        }
    }
}

/// A client's `wl_shm_pool`.
///
/// §109: **lazy pools.** A pool is the client's fd and its size, mapped on
/// this side, and nothing else: no arena bytes are reserved for it, whatever
/// size it says. Foot asks for 512 MiB and uses a few MiB of it; until §109
/// that pool was "held back" for want of a 512 MiB run and every request
/// naming it was absorbed, the compositor's ids fell behind the client's, and
/// the client died with `Invalid new_id`. Now the arena holds only what the
/// pool's **buffers** cover, each its own extent, and only once a buffer is
/// attached ([`BufferInfo::backing`]).
///
/// Upstream, the compositor has a `wl_shm_pool` with the client's id from the
/// first message, always. It is backed by whatever this proxy last pointed it
/// at: the session's 4 KiB stub, or the extent of the buffer most recently
/// created through it. Re-pointing is `destroy` and `create_pool` again with
/// the same id (the compositor frees an id at `destroy`, and this proxy keeps
/// the `delete_id` that answers its own destroy from the client); see
/// [`Session::repoint`].
struct Pool {
    client: ClientMap,
    /// The `wl_shm` this pool was created from: where a re-pointed pool is
    /// created again.
    shm: u32,
    /// What the client says the pool is.
    size: usize,
    /// The client has destroyed the `wl_shm_pool` object.
    zombie: bool,
    /// How many `wl_buffer`s cut from this pool the client still has.
    buffers: u32,
    /// §109. The size of the compositor's pool of this id now: 4096 on the
    /// stub, a buffer's extent after a re-point. A client's `resize` is
    /// forwarded with this number, which the compositor accepts as no change.
    upstream_len: u32,
    /// §109. The compositor's pool of this id is backed by the stub.
    on_stub: bool,
}

impl Pool {
    /// Whether the client's mapping may go: it has destroyed the pool and no
    /// buffer cut from it is left to copy from.
    fn releasable(&self) -> bool {
        self.zombie && self.buffers == 0
    }
}

#[derive(Clone, Debug)]
struct BufferInfo {
    pool: u32,
    offset: i32,
    width: i32,
    height: i32,
    stride: i32,
    /// `wl_shm.format`, the last word of `create_buffer`.
    format: u32,
    /// §109. This buffer's own extent in the arena, or `None`.
    ///
    /// `None` is a buffer the compositor has as a **1x1 stub** on the
    /// session's stub pool: created at the client's `create_buffer` (its id
    /// must reach the compositor then, in the client's sequence), never
    /// attached yet, or reclaimed since. The first `attach` backs it: the
    /// stub is destroyed and the buffer created again under the same id with
    /// the client's geometry, on an extent of exactly its size. The proxy
    /// keeps the `delete_id` that answers its own destroy.
    backing: Option<Region>,
    /// Committed and not yet released, so the compositor may still be
    /// reading it. `wl_buffer.release` is the signal.
    held: bool,
    /// When it was last attached or copied into: the reclaimer takes the
    /// coldest first, and nothing younger than [`RECLAIM_FLOOR`].
    last_used: Instant,
}

impl BufferInfo {
    /// The last byte this buffer occupies, exclusive. The final row needs only
    /// `width * 4` bytes and a client is within its rights to allocate exactly
    /// that, so this is computed rather than `stride * height` — the same
    /// reasoning as `wc-compositor`'s `BufferSpec::required_len`, and getting
    /// it wrong rejects legal buffers.
    fn end(&self) -> Option<usize> {
        let stride = usize::try_from(self.stride).ok()?;
        let rows = usize::try_from(self.height.checked_sub(1)?).ok()?;
        let last = usize::try_from(self.width).ok()?.checked_mul(4)?;
        usize::try_from(self.offset)
            .ok()?
            .checked_add(stride.checked_mul(rows)?)?
            .checked_add(last)
    }

    /// §109. The bytes its own extent needs: the client's view of it from its
    /// offset on, and at least `stride * height`, which is what a compositor
    /// checks a `create_buffer` against.
    fn extent_len(&self) -> Option<u64> {
        let tail = self.end()?.checked_sub(usize::try_from(self.offset).ok()?)?;
        let rows = usize::try_from(self.stride).ok()?.checked_mul(usize::try_from(self.height).ok()?)?;
        Some(tail.max(rows) as u64)
    }

    /// The same buffer, placed at offset 0 of its own extent.
    fn at_zero(&self) -> BufferInfo {
        BufferInfo { offset: 0, backing: None, ..self.clone() }
    }
}

/// How far one buffer's copy in the shared region has fallen behind the
/// client's own copy of it.
///
/// See `Surface::stale` for why a per-buffer answer is needed at all.
#[derive(Clone, Debug)]
enum Stale {
    /// Nothing is known about what the client has written into it — it has
    /// never been attached to this surface, or the rectangle list grew past
    /// the point where tracking it is cheaper than copying the extent. Copy
    /// the whole extent.
    Everything,
    /// Every rectangle the *surface* has changed since this buffer's copy was
    /// last brought up to date. Empty means the copy is current.
    Rects(Vec<Rect>),
}

/// Past this many rectangles, the union is close enough to the extent that
/// tracking it costs more than copying it. Firefox's scroll damage is one or
/// two rectangles a frame and a buffer comes back round every two or three
/// frames, so the real number is single digits; this only bounds the pathology
/// of a buffer that is allocated, damaged around for a thousand frames and
/// then attached once.
const STALE_RECT_CAP: usize = 64;

/// §102.9. Where [`Session::retire_pool_id`] files pools whose id the client
/// has reused: the server's half of the id space, which a client's
/// `wl_shm.create_pool` never names.
const RETIRED_POOL_KEYS: u32 = 0xff00_0000;

#[derive(Default)]
struct Surface {
    pending_buffer: Option<u32>,
    pending_attached: bool,
    current: Option<u32>,
    damage: Vec<Rect>,
    /// Per-buffer arrears, and the fix for docs/waycompose-design.md §27.
    ///
    /// `wl_surface.damage` is defined against the **surface**: it is what
    /// changed on screen since the last commit, not what the client wrote into
    /// the buffer it is attaching. For a compositor that reads the buffer those
    /// are interchangeable, because the attached buffer is required to hold the
    /// whole current surface content. For a proxy that keeps a *copy* of each
    /// buffer they are not, and the gap is exactly one buffer cycle: a client
    /// that double-buffers puts the surface's full content into the back buffer
    /// every frame, so its copy here is stale everywhere the surface changed
    /// while the *other* buffer was on screen — and the client will never
    /// mention those rectangles again.
    ///
    /// So each commit's damage is filed against every buffer of this surface,
    /// and attaching one pays off its whole arrears rather than only this
    /// frame's. Keyed on buffer id, which is also why a buffer that migrates to
    /// another surface starts at `Everything` there and is copied whole: the
    /// arrears are a property of the pair.
    stale: HashMap<u32, Stale>,
}

impl Surface {
    /// File `damage` against every buffer except `attached`, and hand back what
    /// `attached` now owes — this frame's damage included.
    fn arrears(&mut self, attached: u32, damage: &[Rect]) -> Stale {
        for (id, stale) in self.stale.iter_mut() {
            if *id == attached {
                continue;
            }
            if let Stale::Rects(rects) = stale {
                if rects.len() + damage.len() > STALE_RECT_CAP {
                    *stale = Stale::Everything;
                } else {
                    rects.extend_from_slice(damage);
                }
            }
        }
        match self.stale.insert(attached, Stale::Rects(Vec::new())) {
            // First sight of this buffer on this surface: nothing is known
            // about what is in it, and the first commit of a buffer has nothing
            // in the region yet either.
            None | Some(Stale::Everything) => Stale::Everything,
            Some(Stale::Rects(mut rects)) => {
                rects.extend_from_slice(damage);
                Stale::Rects(rects)
            }
        }
    }
}

/// What the run did, for the report and for the harness.
#[derive(Clone, Copy, Debug, Default)]
pub struct Stats {
    pub commits: u64,
    /// §102. Commits of a surface showing a buffer with no pool (a
    /// single-pixel buffer): forwarded, nothing copied.
    pub foreign_commits: u64,
    /// Commits that actually moved pixels. A commit with no buffer attached is
    /// legal and common.
    pub copies: u64,
    pub bytes_copied: u64,
    pub copy_nanos: u64,
    pub pools_created: u64,
    /// Pools whose region was released after the compositor acknowledged the
    /// destructor. Should equal `pools_created` at the end of a clean run.
    pub pools_released: u64,
    /// DYNAMIC ARENA. Regions handed back to the guest, and their bytes. The
    /// second number is the one a run has to print for release to be a claim
    /// rather than a hope — "the guest's committed memory went down by this
    /// much", said by the side that gave it back.
    pub regions_released: u64,
    pub region_bytes_released: u64,
    /// STAGE 16. Pools whose region this proxy took back **without the client
    /// asking**, because the arena had no room and the pool was cold. See
    /// [`Session::reclaim_for`].
    ///
    /// A number worth having on its own rather than folded into
    /// `pools_released`, because the two answer different questions: released
    /// is what the client returned, reclaimed is what it did not, and
    /// docs/waycompose-design.md §17.8 measured that the second is where nearly all the
    /// guest's memory is.
    pub pools_reclaimed: u64,
    /// STAGE 16. Pools rebuilt upstream after being reclaimed, because the
    /// client touched one again. The cost of the policy, in the units the
    /// policy is paid in.
    pub pools_republished: u64,
    /// STAGE 16. Allocation refusals. **Not fatal any more** — this is the
    /// count of times the arena said no, which used to be the count of dead
    /// clients.
    pub alloc_refusals: u64,
    /// STAGE 16. Attaches held back because their buffer had no upstream
    /// object. Each one is a frame the client drew and the compositor did not
    /// show, which is the visible cost of degrading instead of dying.
    pub attaches_deferred: u64,
    /// §109. Buffers given an extent of their own (at their first attach, or
    /// again after a reclaim), and the arena bytes all of them hold now and
    /// at most.
    pub buffers_backed: u64,
    pub backed_bytes_now: u64,
    pub backed_bytes_peak: u64,
    /// §109. Buffers the compositor was given as a 1x1 stub (at
    /// `create_buffer`, or when reclaimed).
    pub buffers_stubbed: u64,
    /// §109. Buffers that could not have an extent even after reclaiming,
    /// answered with `wl_display.error(no_memory)`, which ends the client.
    pub no_memory_errors: u64,
    pub pool_resizes: u64,
    /// Resizes that could not be satisfied because the region had no headroom
    /// left. Should be zero; if it is not, the number is the argument for a
    /// larger `--pool-headroom`.
    pub resize_overflows: u64,
    pub fds_received: u64,
    pub fds_materialised: u64,
    /// Descriptors taken off the queue and closed because the message carrying
    /// them had no transport. Zero for every protocol in the tree; see the note
    /// at the bottom of `client_message`.
    pub fds_dropped: u64,
    /// Of those, the ones the compositor described as a **slice of the arena**
    /// rather than as a file both sides can name.
    ///
    /// STAGE 10, and it is the §8.4 rule applied to the direction stage 6
    /// opened: "a stage that added a second provider with no way to tell which
    /// one ran would not really have added it". A keymap that crossed by arena
    /// and a keymap that crossed by shared file produce byte-identical
    /// characters in the client, so the only way to state which mechanism
    /// carried it is to count them here, in the one function that opens them.
    pub fds_from_arena: u64,
    pub messages_client: u64,
    pub messages_server: u64,

    // --- the clipboard, §29 -------------------------------------------
    //
    // Counted separately from `fds_received` / `fds_materialised` even though
    // both of those move too, because §14.7's line is an assertion about the
    // *boundary* and these are an assertion about a feature. See §29.4 for
    // what the fd line reads now and why it is still the same claim.
    /// Selections pulled out of a client and shipped to the system clipboard.
    pub selections_copied: u64,
    /// `wl_data_offer.receive` requests terminated here rather than forwarded.
    pub pastes_served: u64,
    pub clip_bytes_out: u64,
    pub clip_bytes_in: u64,
    /// Copies that hit [`COPY_DEADLINE`] with a client still holding its pipe
    /// open. Should be zero; if it is not, something offered a mime type it
    /// could not produce.
    pub copy_timeouts: u64,
    /// §27, and only with `--verify-damage`. Commits whose copy was
    /// checked against the client's own bytes.
    pub verified: u64,
    /// Of those, the ones where the region did **not** match — the number that
    /// has to be zero, and the one that was not.
    pub stale_commits: u64,
    /// The worst single commit's count of disagreeing rows, which is what a
    /// user sees as the height of the wrong band.
    pub stale_rows_worst: u64,
    /// Commits the check could not be applied to, because they attached no
    /// buffer. Reported rather than dropped: it is the number that says how
    /// much of the traffic the check has nothing to say about, and a check
    /// whose coverage is unstated is a check nobody can size.
    pub verify_skipped: u64,
    /// Commits that never reached the copy at all, and so were neither verified
    /// nor classified as state-only: no such surface, a surface that has never
    /// had a buffer attached, a buffer or pool already gone, or a pool the
    /// compositor does not have.
    ///
    /// This exists because `wc-damage.sh` asserts an **identity** — every
    /// commit is verified, skipped, or unreached — and an identity with an
    /// unnamed remainder is not one. The remainder was 7 of 3711 on the first
    /// arena run, and the harness was right to fail: not because seven commits
    /// matter, but because a coverage claim with a hole in it cannot be sized.
    /// Every one of these is a legitimate early return; what was missing was
    /// the arithmetic saying so.
    pub verify_unreached: u64,
    /// §104. `wl_buffer`s made from a dma-buf token (`--dmabuf-node`): a
    /// shared allocation the compositor maps by name, nothing copied here.
    pub dmabuf_buffers: u64,
    /// §104. Commits of a surface showing one of those. Every one copied zero
    /// bytes, which is the claim this number exists to make checkable.
    pub dmabuf_commits: u64,
    /// §104. `create`/`create_immed` answered with `failed`.
    pub dmabuf_failed: u64,
    /// §104.11. `wc_gpu_fence_v1.wait`s sent ahead of those commits.
    pub dmabuf_fence_waits: u64,
}

impl Stats {
    pub fn summary(&self) -> String {
        let mb = self.bytes_copied as f64 / (1024.0 * 1024.0);
        let ms = self.copy_nanos as f64 / 1e6;
        let per = if self.copies > 0 {
            format!(
                "{:.1} KiB and {:.0} us per copy",
                self.bytes_copied as f64 / self.copies as f64 / 1024.0,
                self.copy_nanos as f64 / self.copies as f64 / 1000.0
            )
        } else {
            "no copies".to_owned()
        };
        let clip = if self.selections_copied > 0 || self.pastes_served > 0 {
            format!(
                "; clipboard: {} copied ({} bytes), {} pasted ({} bytes){}",
                self.selections_copied,
                self.clip_bytes_out,
                self.pastes_served,
                self.clip_bytes_in,
                if self.copy_timeouts > 0 {
                    format!(", {} timed out", self.copy_timeouts)
                } else {
                    String::new()
                }
            )
        } else {
            String::new()
        };
        // One grep-able line, with the number a harness can assert on spelled
        // out even when it is zero — a check that only prints when it fails is
        // indistinguishable from a check that never ran.
        let verify = if self.verified > 0 || self.verify_skipped > 0 {
            format!(
                "; verified {} commits ({} state-only, unverifiable, {} never reached the copy), \
                 {} stale (worst {} rows)",
                self.verified,
                self.verify_skipped,
                self.verify_unreached,
                self.stale_commits,
                self.stale_rows_worst
            )
        } else {
            String::new()
        };
        // §104. Only when there was any, so a run without `--dmabuf-node`
        // prints the line it always printed.
        let dmabuf = if self.dmabuf_buffers > 0 || self.dmabuf_commits > 0 || self.dmabuf_failed > 0 {
            format!(
                "; dmabuf_buffers={} dmabuf_commits={} dmabuf_failed={} (zero copy) fence_waits={}",
                self.dmabuf_buffers, self.dmabuf_commits, self.dmabuf_failed, self.dmabuf_fence_waits
            )
        } else {
            String::new()
        };
        format!(
            "{} commits, {} copies, {mb:.2} MiB in {ms:.1} ms ({per}); \
             {} pools ({} released, {} reclaimed, {} republished, {} resizes, {} overflows, \
             {} refusals, {} attaches deferred); {} fds in, {} out{}; \
             {} client msgs, {} server msgs; buffers: {} backed ({:.1} MiB now, {:.1} MiB peak), {} stubs, {} no-memory{verify}{clip}{dmabuf}",
            self.commits,
            self.copies,
            self.pools_created,
            self.pools_released,
            self.pools_reclaimed,
            self.pools_republished,
            self.pool_resizes,
            self.resize_overflows,
            self.alloc_refusals,
            self.attaches_deferred,
            self.fds_received,
            self.fds_materialised,
            if self.fds_from_arena > 0 {
                format!(" ({} from the arena)", self.fds_from_arena)
            } else {
                String::new()
            },
            self.messages_client,
            self.messages_server,
            self.buffers_backed,
            self.backed_bytes_now as f64 / (1024.0 * 1024.0),
            self.backed_bytes_peak as f64 / (1024.0 * 1024.0),
            self.buffers_stubbed,
            self.no_memory_errors,
        )
    }
}

/// What a message could not be translated for.
#[derive(Debug)]
pub enum Error {
    /// The object's interface is unknown, so the message cannot be walked.
    /// **Fatal**, and deliberately so: forwarding a message whose shape is
    /// unknown is how a stream desynchronises silently, which is the one
    /// failure docs/waycompose-design.md §2.4 says must never be allowed to happen
    /// quietly.
    UnknownObject { id: u32, opcode: u16 },
    /// The opcode is outside the interface's table.
    UnknownOpcode { interface: String, opcode: u16 },
    Walk(String),
    /// A message needed a descriptor and the queue was empty.
    MissingFd { interface: String, message: String },
    /// The compositor named an object this side cannot open. ADDED FOR
    /// STAGE 6; see the note in `server_message` for why it is fatal.
    Materialise {
        interface: String,
        message: String,
        descriptor: String,
        source: io::Error,
    },
    Io(io::Error),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::UnknownObject { id, opcode } => {
                write!(f, "object {id} has no known interface (opcode {opcode})")
            }
            Error::UnknownOpcode { interface, opcode } => {
                write!(f, "{interface} has no message {opcode}")
            }
            Error::Walk(m) => write!(f, "{m}"),
            Error::MissingFd { interface, message } => {
                write!(f, "{interface}.{message} needs a descriptor and none arrived")
            }
            Error::Materialise { interface, message, descriptor, source } => write!(
                f,
                "{interface}.{message} names {descriptor}, which this side cannot open: {source}"
            ),
            Error::Io(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<io::Error> for Error {
    fn from(e: io::Error) -> Self {
        Error::Io(e)
    }
}

/// Opcodes this file acts on, by name rather than by number at the use site.
/// Checked against the vendored XML in the tests, so a protocol bump that moved
/// one would fail the build's tests rather than the demo.
mod op {
    pub const SHM_CREATE_POOL: u16 = 0;
    /// `wl_shm.release`, version 2.
    pub const SHM_RELEASE: u16 = 1;
    /// `wl_display.error`, an event.
    pub const DISPLAY_ERROR: u16 = 0;
    pub const POOL_CREATE_BUFFER: u16 = 0;
    pub const POOL_DESTROY: u16 = 1;
    pub const POOL_RESIZE: u16 = 2;
    pub const BUFFER_DESTROY: u16 = 0;
    /// `wl_buffer.release`, an **event**: the compositor is finished reading
    /// these bytes. STAGE 16 uses it as the reclaimer's safety interlock.
    pub const BUFFER_RELEASE: u16 = 0;
    pub const SURFACE_DESTROY: u16 = 0;
    pub const SURFACE_ATTACH: u16 = 1;
    pub const SURFACE_DAMAGE: u16 = 2;
    pub const SURFACE_COMMIT: u16 = 6;
    pub const SURFACE_DAMAGE_BUFFER: u16 = 9;
    pub const DISPLAY_DELETE_ID: u16 = 1;
    pub const CALLBACK_DONE: u16 = 0;
}

/// How long a copy waits for the client to finish writing into the pipe.
///
/// A client that answers `wl_data_source.send` is expected to write and close
/// immediately, and every one measured does. The deadline is here because the
/// alternative to a deadline is a selection that never publishes: a client that
/// offers a mime type it cannot actually produce may write nothing and never
/// close, and there is no message that says so. Two seconds is far longer than
/// any real transfer and far shorter than a user will wait before pressing
/// Ctrl-C again.
const COPY_DEADLINE: Duration = Duration::from_secs(2);

/// The mime types worth pulling out of a client's selection, richest first.
///
/// **At most one text flavour plus HTML**, which is a policy and not a
/// limitation of the wire. GTK offers the same string under five names
/// (docs/waycompose-design.md §22.3); pulling all five would open five pipes, wake the
/// client five times and hand the compositor five copies of one paragraph to
/// pick one from. The compositor picks by exactly this order anyway
/// (`wc-compositor`'s `clipboard::TEXT_MIMES`), so the choice is made once,
/// here, where the pipes are.
const WANTED_TEXT: &[&str] = &[
    "text/plain;charset=utf-8",
    "text/plain",
    "UTF8_STRING",
    "STRING",
    "TEXT",
];
const WANTED_HTML: &str = "text/html";
/// §93. A picture: what Firefox offers for "Copy Image", among other names.
const WANTED_IMAGE: &str = "image/png";

/// One selection's worth of bytes: a payload per mime type.
pub type Payloads = Vec<(String, Vec<u8>)>;

/// A selection read out of the client and ready to go across.
pub struct Pulled {
    pub payloads: Payloads,
    /// A drag rather than a copy. The two carry the same bytes through the
    /// same code and differ only in what the far side does with them, which is
    /// the property the side interface was shaped to preserve.
    pub for_drag: bool,
}

/// A pull from the client: the proxy asked for one mime type and is reading it.
struct Outgoing {
    mime: String,
    /// The **read** end of a pipe this process made. The write end went to the
    /// client in a `wl_data_source.send` and this process no longer holds one,
    /// which is what makes the EOF below meaningful.
    fd: OwnedFd,
    buf: Vec<u8>,
    done: bool,
}

/// A push into the client: the client asked to paste and is reading.
struct Incoming {
    token: u32,
    /// The **write** end of a pipe the *client* made, which arrived by
    /// `SCM_RIGHTS` on `wl_data_offer.receive` and stops here. This is the
    /// descriptor that used to end the connection (docs/waycompose-design.md §22.1).
    fd: OwnedFd,
    /// `None` until the compositor answers. `Some` — including `Some(empty)` —
    /// means write it and close.
    data: Option<Vec<u8>>,
    at: usize,
}

/// Which event opcodes the proxy has to *synthesise* toward its client.
///
/// Read from the same XML everything else is, because these are events this
/// program sends as though it were the compositor, and a wrong opcode here is
/// a message the client mis-parses — §2.4's failure with the proxy as its
/// author rather than its victim.
struct DataOps {
    source_send: u16,
    source_cancelled: u16,
    source_dnd_drop_performed: u16,
    source_dnd_finished: u16,
    source_action: u16,
}

impl DataOps {
    fn from(spec: &Spec) -> DataOps {
        let ev = |name: &str| -> u16 {
            spec.interface("wl_data_source")
                .and_then(|i| i.events.iter().position(|m| m.name == name))
                .map(|p| p as u16)
                // Only reachable if `wayland.xml` lost a message, which the
                // spec tests would catch first. Zero is `target`, which is
                // harmless to send and impossible to mistake in a trace.
                .unwrap_or(0)
        };
        DataOps {
            source_send: ev("send"),
            source_cancelled: ev("cancelled"),
            source_dnd_drop_performed: ev("dnd_drop_performed"),
            source_dnd_finished: ev("dnd_finished"),
            source_action: ev("action"),
        }
    }
}

/// Everything about this connection's clipboard.
///
/// # Why the proxy owns the whole `wl_data_*` family rather than forwarding it
///
/// Because two of its messages carry an `h` and the boundary carries none.
/// `wl_data_offer.receive` is the client handing the compositor a pipe to write
/// into; `wl_data_source.send` is the compositor handing the client a pipe to
/// write into. Neither can be turned into a name, because the bytes do not
/// exist yet at the moment the descriptor changes hands — which is the property
/// that separates them from `wl_keyboard.keymap` (§14.7).
///
/// So the proxy answers those two itself, and **exactly those two plus the two
/// requests that would cause them**:
///
/// * `wl_data_offer.receive` is answered here, so the compositor is never asked
///   to write into a descriptor.
/// * `wl_data_device.set_selection` and `.start_drag` are not forwarded, so the
///   compositor never holds a client source and can never be asked to send one
///   — which is what makes `wl_data_source.send` unreachable rather than merely
///   unimplemented.
///
/// Everything else in the family is forwarded unchanged, including the requests
/// that *create* objects. That is not a softening of the rule, it is what makes
/// it survivable: a client's ids are a sequence and a swallowed `new_id` breaks
/// the next one (see `create_data_source` in [`Session::clip_request`], where
/// the measurement is). And every announcement — `wl_data_device.data_offer`,
/// `wl_data_offer.offer`, `wl_data_device.selection` — is plain bytes, with the
/// compositor much better placed than a proxy to decide who may see the
/// selection.
#[derive(Default)]
struct Clip {
    /// Client-created `wl_data_source`s and the mime types each has offered.
    sources: HashMap<u32, Vec<String>>,
    /// The source that owns the selection on this connection, if any.
    selection: Option<u32>,
    /// A source in the middle of a drag, if any.
    dragging: Option<u32>,
    /// `wl_data_offer`s the compositor announced as belonging to a **drag over
    /// one of our surfaces** rather than to the clipboard, learned by watching
    /// `wl_data_device.enter` go past. See [`Session::begin_paste`].
    ///
    /// A set rather than one id because the protocol permits a new offer per
    /// `enter` and nothing says the old one is destroyed first; the ids are
    /// retired by `wl_display.delete_id` like every other object.
    drag_offers: std::collections::HashSet<u32>,
    /// Of those, the ones a `wl_data_device.drop` has already been sent for.
    ///
    /// ADDED FOR docs/waycompose-design.md §50, and it exists because of an ordering nobody had seen.
    /// `wl_data_device.leave` used to forget every drag offer, which reads
    /// correctly — the drag is over, nothing more will be received. It is
    /// wrong, and the first drag that ever entered a client showed why: the
    /// client's `wl_data_offer.receive` comes **after** the drop, and this
    /// compositor sends `leave` in between. The proxy's own log, from that run:
    ///
    /// ```text
    /// wl_data_device.enter: offer 4278190080 is a drag's
    /// wl_data_device.drop
    /// wl_data_device.leave: forgetting drag offers {4278190080}
    /// receive on offer 4278190080 for "text/plain;charset=utf-8" -> Clipboard
    /// paste 1 refused: the clipboard holds nothing of that type
    /// ```
    ///
    /// A dropped offer stays valid until the client destroys it — that is what
    /// `wl_data_offer.finish` is for — so `leave` may only forget the offers
    /// that were *not* dropped. The direction this failed in loses the paste;
    /// §29.8 names the direction it could fail in instead, where a `receive`
    /// routed to the wrong selection hands somebody the last thing dragged.
    dropped_offers: std::collections::HashSet<u32>,
    outgoing: Vec<Outgoing>,
    /// When the current pull started, for [`COPY_DEADLINE`].
    started: Option<Instant>,
    /// What the pull is for once it completes.
    for_drag: bool,
    /// What the dragging source permits, in `wl_data_device_manager.dnd_action`
    /// bits.
    drag_actions: u32,
    incoming: Vec<Incoming>,
    /// Fetches to issue on the side connection.
    fetches: Vec<(u32, Source, String)>,
    /// A completed pull, waiting for the pump to hand it to the compositor.
    ready: Option<Pulled>,
    next_token: u32,
    /// The descriptors that ride beside the events this proxy invents.
    ///
    /// **The bytes go in `Session::to_client`, which stage 16 already built for
    /// its fabricated `wl_display.delete_id`.** Two queues of "events this
    /// proxy invents for its own client" would be two orders in which they
    /// could reach the socket, and the one that matters here is that a
    /// `wl_data_source.send` must not overtake a `delete_id` for an id it
    /// reuses. One queue, one order.
    to_client_fds: Vec<OwnedFd>,
}

pub struct Session {
    spec: Spec,
    objects: Objects,
    transport: Box<dyn PoolTransport>,
    scope: CopyScope,
    /// Every pool, including the ones the client has destroyed. A destroyed
    /// pool stays here, with `zombie` set, until the compositor acknowledges
    /// the destructor — see the note on `POOL_DESTROY` in `client_message`, and
    /// note that its buffers keep working meanwhile, which the `wl_shm` spec
    /// requires and which `weston-presentation-shm` depends on.
    ///
    /// Keyed on the client's id **while the client owns that id**, and on a
    /// key of this proxy's own after: see [`Session::retire_pool_id`].
    pools: HashMap<u32, Pool>,
    /// §102.9. The next key [`Session::retire_pool_id`] tries. Starts in the
    /// server's id range, where no `wl_shm_pool` the client creates can land.
    next_retired_pool: u32,
    buffers: HashMap<u32, BufferInfo>,
    /// §102. `wl_buffer`s that were not cut from a pool — today only
    /// `wp_single_pixel_buffer_manager_v1.create_u32_rgba_buffer`'s, which is
    /// how a nested KWin paints its window's background. They hold no bytes on
    /// this side, so there is nothing to copy, publish or reclaim: they are
    /// upstream from the moment they are created and stay so until destroyed,
    /// and every message naming them is forwarded as it came.
    foreign_buffers: std::collections::HashSet<u32>,
    surfaces: HashMap<u32, Surface>,
    pub stats: Stats,
    /// Log every message as it is walked. Off by default because it is one line
    /// per message and a running client produces thousands, but it is the only
    /// way to answer "what does this client actually *do*" — a question this
    /// stage had to answer about `wl_shm_pool.resize` before it could design
    /// for it, and got a different answer to the one it assumed.
    trace: bool,
    /// `--pace-frames` (docs/waycompose-design.md §88): this side answers the
    /// client's frame callbacks. `None` forwards the compositor's, as before.
    pacer: Option<crate::pace::Pacer>,
    /// Input events (`wl_pointer`, `wl_keyboard`) translated since the caller
    /// last asked, for `WC_FRAME_TRACE`'s `px.out` (waycompose-design.md §89).
    trace_inputs: u32,
    /// Lines worth telling the operator about, drained by the caller so this
    /// stays free of I/O.
    notes: Vec<String>,
    /// Every `xdg_toplevel.set_app_id` seen, in order, drained by the caller.
    ///
    /// The same shape as `notes` and for the same reason: this module is
    /// bytes-in / bytes-out and has no business opening an icon theme or
    /// writing to a second socket. What it can do is notice.
    app_ids: Vec<String>,
    /// §96: the app id for a toplevel whose client sets an empty one, from the
    /// client's own `WC_APP_ID` ([`crate::icons::client_env`]). See
    /// [`Session::set_default_app_id`].
    default_app_id: Option<String>,
    /// §102: the title every toplevel of this client carries, whatever it
    /// asks for. [`Session::set_forced_title`].
    forced_title: Option<String>,
    clip: Clip,
    data_ops: DataOps,
    /// STAGE 16. Events this proxy invents for its own client, drained by the
    /// pump. Today there is exactly one kind: `wl_display.delete_id`.
    /// # Why a proxy invents an event at all
    ///
    /// Because it now destroys objects the client still believes it owns, and
    /// absorbs the client's own destructor for objects the compositor no longer
    /// has. libwayland-client does not recycle an id until the server's
    /// `delete_id` arrives for it — that is what its zombie list is for — so an
    /// absorbed destructor with no answer leaks an id per pool, for the life of
    /// the connection. Inventing the acknowledgement is one message and it is
    /// the truthful one: the object really is gone.
    to_client: Vec<u8>,
    /// Ids **this proxy** destroyed upstream without the client asking, and
    /// how many of their `delete_id`s are still to come. Each is the
    /// compositor answering this proxy, not the client: forwarding it would
    /// tell libwayland-client to recycle an id its application still holds.
    /// A count, because §109 destroys and re-creates the same id (a pool
    /// re-pointed, a buffer backed or reclaimed) many times over, and the
    /// answers come back in the order the destroys went.
    swallow: HashMap<u32, u32>,
    /// §109. The session's 4 KiB stub extent: what a pool is backed by when it
    /// is not carrying a buffer, and a 1x1 stub buffer is cut from.
    stub: Option<Region>,
    /// §109. Extents of buffers the client destroyed, freed when the
    /// compositor's `delete_id` says it has finished with them.
    pending_free: HashMap<u32, Region>,
    /// §109. `wl_shm.release`s held until no pool of the client needs the
    /// `wl_shm` to be re-created on.
    shm_release_held: Vec<u32>,
    /// §109. A fatal `wl_display.error` has been sent to the client; it is
    /// dying, and nothing more of it is forwarded.
    dead: bool,
    /// STAGE 16. Whether the "nothing could be reclaimed" line has been said
    /// since the last time something was. See [`Session::reclaim_for`].
    pressure_noted: bool,
    /// §27. After every copy, compare the attached buffer's visible rows in the
    /// region against the client's own, and count the ones that differ.
    /// The oracle is exact and it is the protocol's: between `commit` and
    /// `wl_buffer.release` the client may not touch the buffer it attached, and
    /// the attached buffer must hold the whole current surface content. So
    /// immediately after the copy, "the region and the client disagree" has
    /// exactly one cause — this proxy did not copy enough — and no race.
    ///
    /// Off by default and never on in a real run: it reads the whole extent
    /// every commit, which is the cost `CopyScope::Damage` exists to avoid.
    /// That makes it an instrument for correctness and not for cadence, and
    /// docs/waycompose-design.md §13.8's retraction is the reason to write that down: an
    /// instrument inside the loop being timed produced a 2x that was really
    /// 1.03x. Nothing here may be used to time anything.
    verify: bool,
    /// §104, `--dmabuf-node`: the `zwp_linux_dmabuf_v1` this proxy answers
    /// itself. `None` (the default) leaves every message as it was. See
    /// `session/dmabuf.rs`.
    dmabuf: Option<dmabuf::Dmabuf>,
    native_shm: native_shm::NativeShm,
}

///
/// STAGE 16. Reclamation is demand-driven — nothing is taken back while there
/// is room — so this is not a timer, it is a guard against thrash: a pool in
/// active rotation would be destroyed and rebuilt on alternate frames, which
/// costs two messages and a full-extent copy each time and gains nothing.
///
/// A quarter of a second is fifteen frames at 60 Hz. It is long enough that
/// nothing in a video's rotation is ever a candidate, and short enough that the
/// stale pools a resize leaves behind — which is what actually accumulates —
/// become candidates almost at once. The number that would falsify it is a
/// client whose working set cycles more slowly than this and is larger than the
/// arena, and no measured client is either.
const RECLAIM_FLOOR: std::time::Duration = std::time::Duration::from_millis(250);

/// Give the arena back every pool this client still holds.
///
/// # The leak this closes, and why it was invisible for four stages
///
/// [`Pool::releasable`] is `zombie && acked && buffers == 0`, and `zombie` is
/// set in exactly one place: the client sending `wl_shm_pool.destroy`. A client
/// that **exits** — which is what a browser being closed looks like — never
/// sends it. So `release_pool_if_done` never fired for the pools a client still
/// held, the `Session` was dropped, and the bytes stayed `published` on an
/// arena that outlives it.
///
/// It was invisible because nothing above the allocator noticed. The socket
/// closes, the client is gone, and the *proxy* is fine — the arena is shared
/// (`Arc<ArenaInner>`) and simply carries the loss into the next connection.
/// The guest measurement is what made it visible: `21880832 of 29360128 bytes
/// out` with no Firefox alive.
///
/// # Why it is the whole of the release story and not a tidy-up
///
/// A region is returned to the guest only when it is **empty**
/// ([`Arena::reclaim_regions`] marks `idle_since` from `slot.is_empty()`), and
/// the guest's pages are `VirtualLock`ed, so they stay gone from Windows until
/// `VirtualFree` runs guest-side. A region that never empties is therefore a
/// region Windows never gets back — which is the user's original complaint,
/// `sv-guest.exe` holding memory with nothing connected. Leaked pool bytes are
/// upstream of every other part of release: no amount of fixing the reclaim
/// clock helps a region that can never reach empty.
///
/// # `Drop` rather than a call at the teardown site
///
/// Because there is more than one way for a session to end — a clean
/// disconnect, a protocol error, a read failure, a panic unwinding the pump —
/// and a sweep that has to be remembered at each of them is a sweep that will
/// be forgotten at the next one added. The arena's bytes outlive this struct,
/// so returning them is exactly what `Drop` is for.
impl Drop for Session {
    fn drop(&mut self) {
        let native = self.native_shm.quarantined();
        if native.0 != 0 {
            eprintln!("[proxy] native GTK disconnect: {} allocation(s), {} bytes remain reserved until the transport ends; compositor/Cairo drain was not proved", native.0, native.1);
        }
        let mut n = 0u64;
        let mut bytes = 0u64;
        // §109: the arena holds buffers' extents, the stub, and the extents of
        // buffers whose `delete_id` never came.
        let mut regions: Vec<Region> = self.buffers.values_mut().filter_map(|b| b.backing.take()).collect();
        regions.extend(self.pending_free.drain().map(|(_, r)| r));
        regions.extend(self.stub.take());
        for region in regions {
            bytes += region.size;
            self.transport.destroy(&region);
            n += 1;
        }
        if n > 0 {
            // Said out loud, because "the client left holding pools" is normal
            // and "the client left holding *all* of them" is a client that
            // crashed — and the two are worth telling apart in a log after the
            // fact.
            println!(
                "[proxy] session ended holding {n} extent(s), {bytes} bytes returned to the arena"
            );
        }
    }
}

impl Session {
    pub fn new(
        spec: Spec,
        transport: Box<dyn PoolTransport>,
        scope: CopyScope,
        headroom: u64,
    ) -> Session {
        let data_ops = DataOps::from(&spec);
        let _ = headroom;
        Session {
            spec,
            objects: Objects::new(),
            transport,
            scope,
            // §109: a pool holds no arena bytes, so there is nothing for
            // `--pool-headroom` to over-allocate; kept for the command line.
            pools: HashMap::new(),
            next_retired_pool: RETIRED_POOL_KEYS,
            buffers: HashMap::new(),
            foreign_buffers: std::collections::HashSet::new(),
            native_shm: native_shm::NativeShm::new(),
            surfaces: HashMap::new(),
            stats: Stats::default(),
            trace: false,
            pacer: None,
            trace_inputs: 0,
            notes: Vec::new(),
            app_ids: Vec::new(),
            default_app_id: None,
            forced_title: None,
            clip: Clip { next_token: 1, ..Clip::default() },
            data_ops,
            to_client: Vec::new(),
            swallow: HashMap::new(),
            stub: None,
            pending_free: HashMap::new(),
            shm_release_held: Vec::new(),
            dead: false,
            pressure_noted: false,
            verify: false,
            dmabuf: None,
        }
    }

    /// Events this session has invented for its own client. See the field.
    pub fn take_client_bytes(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.to_client)
    }

    /// Tell the client an object is gone, when the compositor will not.
    ///
    /// `wl_display` is object 1 in every connection, by the protocol's own
    /// definition, so this needs nothing from the object table.
    fn fabricate_delete_id(&mut self, id: u32) {
        if self.trace {
            self.note(format!("invented delete_id {id}"));
        }
        self.to_client
            .extend_from_slice(&wire::message(1, op::DISPLAY_DELETE_ID, &id.to_ne_bytes()));
        self.objects.forget(id);
    }

    /// Turn the post-copy check on. See `Session::verify`.
    pub fn set_verify(&mut self, on: bool) {
        self.verify = on;
    }

    /// Log every message as it is walked. See the `trace` field.
    pub fn set_trace(&mut self, on: bool) {
        self.trace = on;
    }

    /// Answer the client's frame callbacks here at `hz`, at most `depth` ahead
    /// of the compositor. See [`crate::pace`].
    pub fn set_pacing(&mut self, hz: u32, depth: usize) {
        self.pacer = (hz > 0).then(|| crate::pace::Pacer::new(hz, depth));
    }

    /// Input events translated since the last call (§89's `px.out`).
    pub fn take_trace_inputs(&mut self) -> u32 {
        std::mem::take(&mut self.trace_inputs)
    }

    /// How long the loop may sleep before a frame tick with work on it.
    pub fn until_frame_tick(&self, now: Instant) -> Option<Duration> {
        self.pacer.as_ref().and_then(|p| p.until_tick(now))
    }

    /// Answer whatever the tick allows, into the client's byte stream.
    pub fn tick_frames(&mut self, now: Instant) {
        let Some(p) = self.pacer.as_mut() else { return };
        let answered = p.tick(now);
        if answered.is_empty() {
            return;
        }
        let ms = crate::pace::now_ms();
        for (cb, then_delete) in answered {
            self.to_client.extend_from_slice(&wire::message(cb, op::CALLBACK_DONE, &ms.to_ne_bytes()));
            // The compositor's `delete_id`, held until the client had this.
            if then_delete {
                self.to_client
                    .extend_from_slice(&wire::message(1, op::DISPLAY_DELETE_ID, &cb.to_ne_bytes()));
                self.objects.forget(cb);
            }
            if sv_transport::frametrace::enabled() {
                sv_transport::frametrace::emit("px.done", format_args!("cb={cb} src=local"));
            }
        }
    }

    pub fn transport_name(&self) -> &'static str {
        self.transport.name()
    }

    /// What the arena is holding. See `PoolTransport::memory` for why this is
    /// reported per connection.
    pub fn transport_memory(&self) -> Option<crate::pool::MemoryReport> {
        self.transport.memory()
    }

    pub fn take_notes(&mut self) -> Vec<String> {
        std::mem::take(&mut self.notes)
    }

    fn note(&mut self, s: String) {
        self.notes.push(s);
    }

    /// §96 (docs/waycompose-design.md 96.11). The app id to give a toplevel
    /// whose client calls `xdg_toplevel.set_app_id("")`.
    ///
    /// GLFW 3.4 does exactly that unless the application sets the
    /// `GLFW_WAYLAND_APP_ID` hint (`if (window->wl.appId)` is true for the
    /// empty string it duplicated), and Minecraft does not set it. A window
    /// with no app id has no icon and no taskbar identity: on the guest it was
    /// `Waycompose.unnamed.c4`, the generic icon, a button of its own and no
    /// pin. The id comes from whoever started the client, as `WC_APP_ID` in
    /// its environment, which the caller reads from `/proc`. Only an empty id
    /// is replaced: a client that names itself is never overruled.
    pub fn set_default_app_id(&mut self, id: Option<String>) {
        self.default_app_id = id;
    }

    /// §102 (docs/waycompose-design.md 102.4). The title for every toplevel
    /// of this client, in place of what it sets and from its first moment.
    ///
    /// For a client its launcher started as another user, whose own title is
    /// a statement about Linux and not about the window on Windows: the nested
    /// desktop's KWin calls its window "KDE Wayland Compositor WL-0 — Press
    /// right control key to grab pointer", and its login screen's cosmic-comp
    /// says "Smithay". The launcher puts the application's name in
    /// `wc-foreign-app-ids`, beside the app id.
    pub fn set_forced_title(&mut self, title: Option<String>) {
        self.forced_title = title;
    }

    /// The app ids seen since the last call. See the field.
    pub fn take_app_ids(&mut self) -> Vec<String> {
        std::mem::take(&mut self.app_ids)
    }

    /// Translate one request from the local client.
    ///
    /// `fds` is the connection's fd queue, in arrival order, exactly as
    /// libwayland maintains it: descriptors are matched to `h` arguments
    /// positionally across the whole stream and not per message.
    pub fn client_message(
        &mut self,
        header: Header,
        body: &[u8],
        fds: &mut std::collections::VecDeque<std::os::fd::OwnedFd>,
    ) -> Result<Vec<u8>, Error> {
        self.stats.messages_client += 1;
        // §109: a client told `wl_display.error` is dying, and libwayland-client
        // sends nothing after it; anything already queued is not forwarded.
        if self.dead {
            return Ok(Vec::new());
        }
        let interface = self
            .objects
            .interface(header.object)
            .ok_or(Error::UnknownObject { id: header.object, opcode: header.opcode })?
            .to_owned();
        let msg = self
            .spec
            .request(&interface, header.opcode)
            .ok_or_else(|| Error::UnknownOpcode {
                interface: interface.clone(),
                opcode: header.opcode,
            })?
            .clone();
        let spans = wire::walk(body, &msg.args, FdEncoding::OutOfBand)
            .map_err(|e| Error::Walk(format!("{interface}.{}: {e}", msg.name)))?;
        if self.trace {
            self.note(format!("-> {interface}@{}.{}", header.object, msg.name));
        }

        self.record_new_ids(body, &spans, &msg.args);
        // §102. A `wl_buffer` made by anything but a pool has no bytes here
        // (see `foreign_buffers`). Decided by the argument's interface rather
        // than by the one factory that exists today, so that the next one —
        // `linux-dmabuf`'s `create_immed`, say — is not silently held back by
        // the attach rule below.
        if interface != "wl_shm_pool" {
            for (span, arg) in spans.iter().zip(&msg.args) {
                if arg.kind == ArgKind::NewId
                    && arg.interface.as_deref() == Some("wl_buffer")
                    && let Some(id) = wire::word_at(body, span)
                {
                    self.foreign_buffers.insert(id);
                }
            }
        }
        if sv_transport::frametrace::enabled() {
            trace_request(&interface, &msg.name, header.object, body, &spans);
        }
        if interface == "wl_surface"
            && msg.name == "frame"
            && let Some(p) = self.pacer.as_mut()
            && let Some(cb) = spans.first().and_then(|s| wire::word_at(body, s))
        {
            p.requested(cb, header.object);
        }

        let mut out = Vec::new();

        if let Some(bytes) = self.native_request(&interface, header.object, header.opcode, body, &spans, fds)? {
            return Ok(bytes);
        }

        // §104. `zwp_linux_dmabuf_v1` and its objects are answered here, with
        // `--dmabuf-node`; a no-op without it.
        if let Some(bytes) =
            self.dmabuf_request(&interface, header.opcode, header.object, body, &spans, fds)?
        {
            out.extend_from_slice(&bytes);
            return Ok(out);
        }

        // STAGE 11. The one message this program *watches* rather than
        // understands: it is forwarded unchanged, and a copy of the string goes
        // to the icon resolver, which is on this side because the icon theme is
        // (`crate::icons`). Matched on the message's **name** rather than on an
        // opcode constant, because the opcode is a position in somebody else's
        // XML and this is the only place that would notice if it moved.
        //
        // §96: and the one it rewrites, when the string is empty and the
        // client's launcher said what it should be (`set_default_app_id`).
        let mut rewritten: Option<Vec<u8>> = None;
        // §102: a forced title replaces whatever the client calls its window.
        if interface == "xdg_toplevel" && msg.name == "set_title"
            && let Some(title) = self.forced_title.clone()
        {
            let mut b = Vec::new();
            wire::push_string(&mut b, &title);
            rewritten = Some(b);
        }
        if interface == "xdg_toplevel" && msg.name == "set_app_id"
            && let Some(app_id) = spans.first().and_then(|s| wire::string_at(body, s)) {
                match (self.default_app_id.clone(), app_id.is_empty()) {
                    (Some(id), true) => {
                        let mut b = Vec::new();
                        wire::push_string(&mut b, &id);
                        self.note(format!(
                            "xdg_toplevel@{} set an empty app id; it is {id:?}, from the \
                             client's WC_APP_ID",
                            header.object
                        ));
                        self.app_ids.push(id);
                        rewritten = Some(b);
                    }
                    _ => self.app_ids.push(app_id),
                }
            }
        // §96: and a toplevel whose client may never call `set_app_id` at all.
        // GLFW on libdecor (glfw3-minecraft takes that path when libdecor is
        // there) calls `libdecor_frame_set_app_id` only for a non-empty id,
        // so Minecraft's window never named itself and the rewrite above had
        // nothing to rewrite. The default follows `get_toplevel` at once; an
        // id the client names later still replaces it, as any second
        // `set_app_id` does.
        let mut injected: Vec<u8> = Vec::new();
        if interface == "xdg_surface" && msg.name == "get_toplevel"
            && let Some(title) = self.forced_title.clone()
            && let Some(top) = spans.first().and_then(|s| wire::word_at(body, s))
            && let Some(op) = self.spec.interface("xdg_toplevel").and_then(|i| {
                i.requests.iter().position(|m| m.name == "set_title")
            })
        {
            let mut b = Vec::new();
            wire::push_string(&mut b, &title);
            self.note(format!("xdg_toplevel@{top} is titled {title:?}, from its launcher"));
            injected.extend_from_slice(&wire::message(top, op as u16, &b));
        }
        if interface == "xdg_surface" && msg.name == "get_toplevel"
            && let Some(id) = self.default_app_id.clone()
            && let Some(top) = spans.first().and_then(|s| wire::word_at(body, s))
            && let Some(op) = self.spec.interface("xdg_toplevel").and_then(|i| {
                i.requests.iter().position(|m| m.name == "set_app_id")
            })
        {
            let mut b = Vec::new();
            wire::push_string(&mut b, &id);
            self.note(format!(
                "xdg_toplevel@{top} starts with app id {id:?}, from the client's WC_APP_ID"
            ));
            self.app_ids.push(id);
            injected.extend_from_slice(&wire::message(top, op as u16, &b));
        }

        // STAGE 13. The one *family* this program answers itself rather than
        // forwarding. See [`Clip`] for why, and note that this is matched on
        // the interface and the message's **name**: an opcode is a position in
        // somebody else's XML, and this is the only place that would notice if
        // one moved.
        if interface.starts_with("wl_data_")
            && self.clip_request(&interface, &msg.name, header.object, body, &spans, fds)
        {
            return Ok(out);
        }

        // The semantic half: everything the proxy has to understand rather than
        // merely carry.
        let descriptors: Vec<Vec<u8>> = Vec::new();
        match (interface.as_str(), header.opcode) {
            // §109. Every request about a pool or a buffer reaches the
            // compositor, in the client's order and with the client's ids. What
            // changes is what backs them: see `Pool` and `BufferInfo::backing`.
            ("wl_shm", op::SHM_CREATE_POOL) => {
                let pool_id = wire::word_at(body, &spans[0]).unwrap_or(0);
                let size = wire::word_at(body, &spans[2]).unwrap_or(0) as i32;
                let fd = fds.pop_front().ok_or_else(|| Error::MissingFd {
                    interface: interface.clone(),
                    message: msg.name.clone(),
                })?;
                self.stats.fds_received += 1;
                self.create_pool(pool_id, header.object, fd, size)?;
                // Upstream, on the stub, whatever size the client asked for.
                let Some(stub) = self.ensure_stub() else {
                    self.no_memory(pool_id, "its pool's 4 KiB stub".into());
                    return Ok(out);
                };
                out.extend_from_slice(&create_pool_message(header.object, pool_id, &stub.descriptor, STUB_LEN));
                self.stats.pools_created += 1;
                self.note(format!(
                    "pool {pool_id}: {size} bytes from the client; nothing of the arena is held for it \
                     until a buffer cut from it is attached (§109)"
                ));
                return Ok(out);
            }
            ("wl_shm", op::SHM_RELEASE) => {
                // A pool may still have to be re-created on this `wl_shm`; the
                // destructor goes when none can be (`release_held_shm`).
                if self.pools.values().any(|p| p.shm == header.object && !p.zombie) {
                    self.shm_release_held.push(header.object);
                    return Ok(out);
                }
            }
            ("wl_shm_pool", op::POOL_CREATE_BUFFER) => {
                let id = wire::word_at(body, &spans[0]).unwrap_or(0);
                let b = BufferInfo {
                    pool: header.object,
                    offset: wire::word_at(body, &spans[1]).unwrap_or(0) as i32,
                    width: wire::word_at(body, &spans[2]).unwrap_or(0) as i32,
                    height: wire::word_at(body, &spans[3]).unwrap_or(0) as i32,
                    stride: wire::word_at(body, &spans[4]).unwrap_or(0) as i32,
                    format: wire::word_at(body, &spans[5]).unwrap_or(0),
                    backing: None,
                    held: false,
                    last_used: Instant::now(),
                };
                // The compositor would check the buffer against the pool; its
                // pool is the stub, so the check is made here, with its words.
                let size = self.pools.get(&header.object).map_or(0, |p| p.size);
                if b.width <= 0 || b.height <= 0 || b.stride <= 0 || b.end().is_none_or(|e| e > size) {
                    self.client_error(
                        header.object,
                        SHM_ERROR_INVALID_STRIDE,
                        format!(
                            "buffer {id}: {}x{} stride {} at offset {} is outside the {size}-byte pool",
                            b.width, b.height, b.stride, b.offset
                        ),
                    );
                    return Ok(out);
                }
                self.foreign_buffers.remove(&id);
                self.buffers.insert(id, b);
                self.forget_buffer_arrears(id);
                if let Some(pool) = self.pools.get_mut(&header.object) {
                    pool.buffers += 1;
                }
                // A 1x1 stub until it is attached: the id reaches the
                // compositor now, the bytes when they are needed.
                out.extend_from_slice(&self.stub_buffer(header.object, id));
                return Ok(out);
            }
            ("wl_shm_pool", op::POOL_RESIZE) => {
                let size = wire::word_at(body, &spans[0]).unwrap_or(0) as i32;
                // The client's mapping follows; the compositor's pool is as
                // big as what it carries, so its resize is to that same size,
                // which is no change. Forwarded, so the stream stays the
                // client's.
                let len = self.resize_pool(header.object, size);
                out.extend_from_slice(&wire::message(header.object, op::POOL_RESIZE, &len.to_ne_bytes()));
                return Ok(out);
            }
            // **The client's mapping is not dropped here.** `weston-presentation-
            // shm` creates a pool, its buffers, and destroys the pool at once,
            // which the `wl_shm` spec allows: the memory lives until the
            // buffers are gone. The mapping goes when the last buffer cut from
            // it does (`release_pool_if_done`).
            ("wl_shm_pool", op::POOL_DESTROY) => {
                // §109. A stub buffer is backed through a live pool. If this was
                // the client's last one, back its stubs now, while there is a
                // pool to do it through.
                let last = !self.pools.iter().any(|(id, p)| *id != header.object && !p.zombie);
                if last {
                    let stubs: Vec<u32> = self
                        .buffers
                        .iter()
                        .filter(|(_, b)| b.backing.is_none())
                        .map(|(id, _)| *id)
                        .collect();
                    let mut stubs = stubs;
                    stubs.sort_unstable();
                    for id in stubs {
                        match self.back_buffer(header.object, id) {
                            Ok(bytes) => out.extend_from_slice(&bytes),
                            Err(why) => {
                                self.no_memory(id, why);
                                return Ok(out);
                            }
                        }
                    }
                }
                if let Some(pool) = self.pools.get_mut(&header.object) {
                    pool.zombie = true;
                }
                out.extend_from_slice(&wire::message(header.object, op::POOL_DESTROY, &[]));
                out.extend_from_slice(&self.release_held_shm());
                return Ok(out);
            }
            ("wl_buffer", op::BUFFER_DESTROY) => {
                self.foreign_buffers.remove(&header.object);
                if let Some(b) = self.buffers.remove(&header.object) {
                    if let Some(pool) = self.pools.get_mut(&b.pool) {
                        pool.buffers = pool.buffers.saturating_sub(1);
                    }
                    // Its extent goes when the compositor's `delete_id` says
                    // it has finished with it.
                    if let Some(r) = b.backing {
                        self.pending_free.insert(header.object, r);
                    }
                    self.release_pool_if_done(b.pool);
                }
                self.forget_buffer_arrears(header.object);
            }
            ("wl_surface", op::SURFACE_DESTROY) => {
                self.surfaces.remove(&header.object);
                if let Some(p) = self.pacer.as_mut() {
                    p.surface_destroyed(header.object);
                }
            }
            ("wl_surface", op::SURFACE_ATTACH) => {
                let buffer = wire::word_at(body, &spans[0]).unwrap_or(0);
                // §104: a failed dma-buf's placeholder is no `wl_buffer` upstream;
                // its attach is held back, as that module's header says.
                if buffer != 0 && self.is_placeholder_buffer(buffer) {
                    self.stats.attaches_deferred += 1;
                    return Ok(out);
                }
                // §109. The buffer's first attach (or its first since it was
                // reclaimed) gives it its own extent, before the attach.
                if buffer != 0 && self.buffers.get(&buffer).is_some_and(|b| b.backing.is_none()) {
                    let pool = self.buffers[&buffer].pool;
                    match self.back_buffer(pool, buffer) {
                        Ok(bytes) => out.extend_from_slice(&bytes),
                        Err(why) => {
                            self.no_memory(buffer, why);
                            return Ok(out);
                        }
                    }
                }
                if let Some(b) = self.buffers.get_mut(&buffer) {
                    b.last_used = Instant::now();
                }
                let s = self.surfaces.entry(header.object).or_default();
                s.pending_attached = true;
                s.pending_buffer = (buffer != 0).then_some(buffer);
            }
            ("wl_surface", op::SURFACE_DAMAGE | op::SURFACE_DAMAGE_BUFFER) => {
                let r = Rect {
                    x: wire::word_at(body, &spans[0]).unwrap_or(0) as i32,
                    y: wire::word_at(body, &spans[1]).unwrap_or(0) as i32,
                    w: wire::word_at(body, &spans[2]).unwrap_or(0) as i32,
                    h: wire::word_at(body, &spans[3]).unwrap_or(0) as i32,
                };
                self.surfaces.entry(header.object).or_default().damage.push(r);
            }
            ("wl_surface", op::SURFACE_COMMIT) => {
                self.commit(header.object);
                // §104.11: the compositor waits for the GPU's frame, not KWin.
                if let Some(b) = self.surfaces.get(&header.object).and_then(|s| s.current)
                    && self.is_dmabuf_buffer(b)
                {
                    let fence = self.dmabuf_fence(b);
                    out.extend_from_slice(&fence);
                }
            }
            _ => {}
        }

        let out_body = if let Some(b) = rewritten {
            b
        } else if msg.has_fd() {
            // Every `h` must have produced a descriptor above. Splicing a
            // zero-length one instead would be the §2.4 failure in its purest
            // form: a well-formed message the compositor parses happily and
            // then cannot map, or worse, a length the parser walks past.
            //
            // **CHANGED IN STAGE 13, and this is the defect the stage exists to
            // remove.** This used to return `Error::UnsupportedFd`, which
            // `proxy.rs` turns into a failed drain, which **ends the client's
            // connection** — so a client that pasted did not fail to paste, it
            // died. The only message that could reach here was
            // `wl_data_offer.receive`, and it no longer can, because the
            // clipboard family is answered above.
            //
            // What is left is the case somebody else's protocol brings: an `h`
            // in an XML added later, in a message this file has no transport
            // for. Dropping the message is the honest answer. The descriptors
            // are consumed, so the fd queue stays in step and nothing after it
            // desynchronises; the client is left waiting for whatever it asked
            // for, which is recoverable, where a closed connection is not. It
            // is loud in the log because a silent drop would be the mystery
            // the old error was trying to avoid.
            if descriptors.len() != msg.fd_count() {
                for _ in 0..msg.fd_count() {
                    if fds.pop_front().is_some() {
                        self.stats.fds_received += 1;
                    }
                }
                self.stats.fds_dropped += 1;
                self.note(format!(
                    "{interface}.{} carries {} descriptor(s) this proxy has no transport for; \
                     the message was dropped and its descriptors closed. The client will wait \
                     for an answer that is not coming, which is better than the connection \
                     being closed under it — but nothing here can carry that fd across the \
                     boundary, so the protocol needs a by-bytes route the way the clipboard \
                     got one (docs/waycompose-design.md §22.2).",
                    msg.name,
                    msg.fd_count()
                ));
                return Ok(out);
            }
            wire::splice_fds(body, &spans, FdEncoding::OutOfBand, &descriptors)
        } else {
            body.to_vec()
        };
        out.extend_from_slice(&wire::message(header.object, header.opcode, &out_body));
        out.extend_from_slice(&injected);
        Ok(out)
    }

    // -----------------------------------------------------------------------
    // The clipboard
    // -----------------------------------------------------------------------

    /// Answer one `wl_data_*` request here, or say it should be forwarded.
    ///
    /// Returns `true` if the message was **consumed** — not forwarded, and any
    /// descriptors it carried taken off the queue.
    fn clip_request(
        &mut self,
        interface: &str,
        name: &str,
        object: u32,
        body: &[u8],
        spans: &[Span],
        fds: &mut std::collections::VecDeque<OwnedFd>,
    ) -> bool {
        match (interface, name) {
            // **Watched and forwarded, not swallowed**, and the difference was
            // measured rather than reasoned about.
            //
            // The first design swallowed `create_data_source`, on the argument
            // that a compositor which never learns the source's id can never
            // send `wl_data_source.send` on it. That is true and it is fatal:
            // a client's object ids are a *sequence*, and `wayland-backend`'s
            // server-side map accepts a new id only when it is the next free
            // one (`rs/map.rs`, the same check `iconlink`'s header quotes).
            // Skipping id 10 means the client's next request, for 11, is
            // refused — and the failure is not subtle. `wl-copy` died with
            //
            //     wl_display#1: error 0: Invalid new_id: 10.
            //
            // on the request *after* the one that was swallowed.
            //
            // So the object is created upstream and simply never used: the
            // compositor holds a `wl_data_source` with mime types on it that is
            // never made a selection and never starts a drag, because
            // `set_selection` and `start_drag` are the requests that do not get
            // forwarded. smithay sends `send` only for a source it has been
            // given as the selection or as a drag's, so the event that cannot
            // cross is unreachable by construction — which is the same property
            // the first design wanted, obtained one message later.
            ("wl_data_device_manager", "create_data_source") => {
                if let Some(id) = wire::word_at(body, &spans[0]) {
                    self.clip.sources.insert(id, Vec::new());
                }
                false
            }
            ("wl_data_source", "offer") => {
                if let Some(mime) = wire::string_at(body, &spans[0])
                    && let Some(list) = self.clip.sources.get_mut(&object)
                    && !list.contains(&mime)
                {
                    list.push(mime);
                }
                false
            }
            ("wl_data_source", "set_actions") => {
                if let Some(actions) = wire::word_at(body, &spans[0]) {
                    self.clip.drag_actions = actions;
                }
                false
            }
            ("wl_data_source", "destroy") => {
                self.clip.sources.remove(&object);
                if self.clip.selection == Some(object) {
                    self.clip.selection = None;
                }
                if self.clip.dragging == Some(object) {
                    self.clip.dragging = None;
                }
                // Forwarded like the rest, so the `wl_display.delete_id` that
                // retires the number comes from the compositor as it does for
                // every other object. Synthesising one here would race the real
                // one and retire the id twice.
                false
            }
            ("wl_data_device", "set_selection") => {
                let source = wire::word_at(body, &spans[0]).unwrap_or(0);
                self.set_selection(source);
                true
            }
            ("wl_data_device", "start_drag") => {
                let source = wire::word_at(body, &spans[0]).unwrap_or(0);
                self.start_drag(source);
                true
            }
            // The message this whole stage is about. The descriptor is the
            // client's own pipe, on the client's own machine, and it stops
            // here.
            ("wl_data_offer", "receive") => {
                let mime = wire::string_at(body, &spans[0]).unwrap_or_default();
                match fds.pop_front() {
                    Some(fd) => {
                        self.stats.fds_received += 1;
                        self.begin_paste(object, mime, fd);
                    }
                    None => self.note(format!(
                        "wl_data_offer.receive for {mime:?} arrived with no descriptor; \
                         nothing to write into"
                    )),
                }
                true
            }
            _ => false,
        }
    }

    /// One event on a client-owned `wl_data_source`, sent as though this were
    /// the compositor.
    fn source_event(&mut self, source: u32, opcode: u16, body: &[u8]) {
        self.to_client
            .extend_from_slice(&wire::message(source, opcode, body));
    }

    /// `wl_data_device.set_selection`: the client has copied something.
    fn set_selection(&mut self, source: u32) {
        if source == 0 {
            // A null source clears the selection. Publishing an empty set
            // empties the system clipboard, which is what the client asked for.
            self.clip.selection = None;
            self.clip.ready = Some(Pulled { payloads: Vec::new(), for_drag: false });
            self.note("the client cleared its selection".into());
            return;
        }
        self.clip.selection = Some(source);
        self.begin_pull(source, false);
    }

    /// `wl_data_device.start_drag`.
    fn start_drag(&mut self, source: u32) {
        if source == 0 {
            return;
        }
        self.clip.dragging = Some(source);
        self.begin_pull(source, true);
    }

    /// Ask the client for the bytes behind a source, over pipes this process
    /// makes.
    ///
    /// **Eager, and that is forced rather than chosen.** Windows' clipboard
    /// takes the data at `SetClipboardData` time and OLE asks for a drag's data
    /// with a synchronous call from inside a modal loop; neither can round-trip
    /// to a Wayland client on another machine at the moment it is asked. Delayed
    /// rendering (`SetClipboardData(fmt, NULL)` and answer `WM_RENDERFORMAT`)
    /// would be the Win32 way to avoid the copy, and it would put a hypervisor
    /// crossing inside a synchronous message handler with a client that may be
    /// busy. So a copy costs one read of the selection at the moment of
    /// copying, which is what every clipboard bridge does.
    fn begin_pull(&mut self, source: u32, for_drag: bool) {
        let mimes = self.clip.sources.get(&source).cloned().unwrap_or_default();
        let wanted = wanted_mimes(&mimes);
        if wanted.is_empty() {
            self.note(format!(
                "the client offered [{}], none of which this clipboard carries; \
                 the system clipboard is left alone",
                mimes.join(", ")
            ));
            // Deliberately **not** an empty publish: clearing the system
            // clipboard because a client copied an image would destroy
            // something the user put there on purpose.
            if !for_drag {
                self.clip.selection = None;
            }
            return;
        }
        // A pull already running is abandoned rather than interleaved: two
        // selections in flight would publish in whichever order the pipes
        // drained, and the client's later copy is the one the user meant.
        self.clip.outgoing.clear();
        self.clip.for_drag = for_drag;
        self.clip.started = Some(Instant::now());
        for mime in wanted {
            let (read, write) = match pipe() {
                Ok(p) => p,
                Err(e) => {
                    self.note(format!("could not make a pipe for {mime:?}: {e}"));
                    continue;
                }
            };
            let mut body = Vec::new();
            wire::push_bytes(&mut body, &nul(&mime));
            self.source_event(source, self.data_ops.source_send, &body);
            // The write end goes to the client and this process keeps none of
            // it — which is what makes the EOF in `pump_transfers` mean "the
            // client has finished writing" rather than "nobody has written
            // yet". `proxy.rs` drops these once they are sent.
            self.clip.to_client_fds.push(write);
            self.stats.fds_materialised += 1;
            let _ = set_nonblocking(&read);
            self.clip.outgoing.push(Outgoing { mime, fd: read, buf: Vec::new(), done: false });
        }
    }

    /// `wl_data_offer.receive`: the client wants to paste.
    ///
    /// `offer` decides **which system selection is asked for**, and it has to be
    /// the offer's own identity rather than anything about this connection's
    /// state. An offer the compositor announced with `wl_data_device.enter` is a
    /// drag's; one announced with `.selection` is the clipboard's. Deciding from
    /// "is this client dragging" instead would be asking about the wrong drag
    /// entirely — the client is the *source* of its own drag and never receives
    /// from it — and Windows keeps the two selections apart, so a paste that
    /// asked the wrong one would quietly return the last thing dragged.
    fn begin_paste(&mut self, offer: u32, mime: String, fd: OwnedFd) {
        let token = self.clip.next_token;
        self.clip.next_token = self.clip.next_token.wrapping_add(1).max(1);
        // Non-blocking on **the write end this process now holds**. A pipe's
        // two ends are separate open file descriptions, so this cannot make the
        // client's own reads non-blocking; without it a payload larger than the
        // pipe buffer would block the pump on a client that is not reading yet.
        let _ = set_nonblocking(&fd);
        self.stats.pastes_served += 1;
        let source = if self.clip.drag_offers.contains(&offer) {
            Source::Drag
        } else {
            Source::Clipboard
        };
        // ADDED FOR docs/waycompose-design.md §50. This decision had never been *observed* — the first
        // real drag into a client made it wrongly and the only symptom was a
        // paste that came back empty, four processes away. It is one line and
        // it names both the answer and the evidence for it.
        self.note(format!(
            "receive on offer {offer} for {mime:?} -> {source:?} \
             (offers announced by wl_data_device.enter: {:?})",
            self.clip.drag_offers
        ));
        self.clip.fetches.push((token, source, mime));
        self.clip.incoming.push(Incoming { token, fd, data: None, at: 0 });
    }

    /// Fetches the pump should issue on the side connection.
    pub fn take_fetches(&mut self) -> Vec<(u32, Source, String)> {
        std::mem::take(&mut self.clip.fetches)
    }

    /// A completed pull, and whether it is for a drag rather than a copy.
    pub fn take_selection(&mut self) -> Option<Pulled> {
        self.clip.ready.take()
    }

    /// What the source permits, for a drag.
    pub fn drag_actions(&self) -> u32 {
        self.clip.drag_actions
    }

    /// The compositor answered a fetch. Write it into the client's pipe.
    pub fn deliver(&mut self, token: u32, answer: Answer) {
        let bytes = match answer {
            Answer::Bytes(b) => {
                self.stats.clip_bytes_in += b.len() as u64;
                b
            }
            Answer::Failed(why) => {
                self.note(format!("paste {token} refused: {why}"));
                // An empty payload rather than a dangling pipe. The client
                // reads EOF at once and pastes nothing, which is the outcome a
                // user understands; leaving the descriptor open would hang
                // whatever is reading it.
                Vec::new()
            }
        };
        if let Some(inc) = self.clip.incoming.iter_mut().find(|i| i.token == token) {
            inc.data = Some(bytes);
        }
    }

    /// A drag has ended: tell the source what happened.
    pub fn drag_finished(&mut self, action: u32) {
        let Some(source) = self.clip.dragging.take() else {
            return;
        };
        if action != 0 {
            self.source_event(source, self.data_ops.source_action, &action.to_ne_bytes());
            self.source_event(source, self.data_ops.source_dnd_drop_performed, &[]);
            self.source_event(source, self.data_ops.source_dnd_finished, &[]);
        } else {
            self.source_event(source, self.data_ops.source_cancelled, &[]);
        }
    }

    /// Whether a transfer is in flight, so the pump can poll more eagerly.
    pub fn clip_busy(&self) -> bool {
        !self.clip.outgoing.is_empty() || !self.clip.incoming.is_empty()
    }

    /// Move whatever bytes will move, in both directions. Called once per turn
    /// of the pump.
    ///
    /// **Not `poll`ed on**, and that is a decision rather than an oversight.
    /// Adding the pipes to the pump's `pollfd` array would mean matching a
    /// variable-length array back to a variable-length list of transfers on
    /// every turn, for a gain of a few milliseconds on an operation that
    /// happens when a human presses a key. Instead the pump polls at 1 ms
    /// while `clip_busy`, and this drains everything available each turn — so
    /// a transfer costs a millisecond of latency and a spin that lasts as long
    /// as the transfer does.
    pub fn pump_transfers(&mut self) {
        let mut progressed = false;
        for out in &mut self.clip.outgoing {
            if out.done {
                continue;
            }
            loop {
                let mut buf = [0u8; 16 * 1024];
                // SAFETY: a live fd owned by `out` and a buffer whose length is
                // passed truthfully.
                let n = unsafe {
                    libc::read(out.fd.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len())
                };
                if n > 0 {
                    out.buf.extend_from_slice(&buf[..n as usize]);
                    progressed = true;
                    // Past the cap there is nothing this can still publish
                    // (`fit_selection` leaves it out), so it stops reading:
                    // the deadline now waits for a client that has gone
                    // quiet, and one that never does must not grow this.
                    if out.buf.len() > MAX_SELECTION_BYTES {
                        out.done = true;
                        break;
                    }
                    continue;
                }
                if n == 0 {
                    // The client closed its end: the payload is complete.
                    out.done = true;
                } else {
                    let e = io::Error::last_os_error();
                    if e.kind() != io::ErrorKind::WouldBlock
                        && e.kind() != io::ErrorKind::Interrupted
                    {
                        out.done = true;
                    }
                }
                break;
            }
        }
        // §93: the deadline is for a client that has gone *quiet*, not for a
        // big copy. Measured from the start, a page of HTML that took longer
        // than two seconds to write was published cut off in the middle.
        if progressed && self.clip.started.is_some() {
            self.clip.started = Some(Instant::now());
        }

        let expired = self
            .clip
            .started
            .is_some_and(|t| t.elapsed() > COPY_DEADLINE);
        if !self.clip.outgoing.is_empty()
            && (self.clip.outgoing.iter().all(|o| o.done) || expired)
        {
            if expired {
                self.stats.copy_timeouts += 1;
                self.note(format!(
                    "a client held its clipboard pipe open for more than {:?}; \
                     publishing the {} bytes it did write",
                    COPY_DEADLINE,
                    self.clip.outgoing.iter().map(|o| o.buf.len()).sum::<usize>()
                ));
            }
            let for_drag = self.clip.for_drag;
            let payloads: Payloads = self
                .clip
                .outgoing
                .drain(..)
                .map(|o| {
                    self.stats.clip_bytes_out += o.buf.len() as u64;
                    (o.mime, o.buf)
                })
                .collect();
            self.clip.started = None;
            self.stats.selections_copied += 1;
            self.note(format!(
                "the client's selection is {} byte(s) across {} mime type(s): {}",
                payloads.iter().map(|(_, b)| b.len()).sum::<usize>(),
                payloads.len(),
                payloads.iter().map(|(m, _)| m.as_str()).collect::<Vec<_>>().join(", ")
            ));
            // §93: never more than the compositor holds. Past its cap the
            // compositor refused the whole publish with a protocol error,
            // which ended this connection's clipboard feed: one large copy (a
            // long page's HTML) and copy and paste did nothing for the rest
            // of the session.
            let (payloads, dropped) = fit_selection(payloads, MAX_SELECTION_BYTES);
            if !dropped.is_empty() {
                self.note(format!(
                    "the selection is larger than the compositor holds ({} MiB); left out: {}",
                    MAX_SELECTION_BYTES >> 20,
                    dropped.join(", ")
                ));
            }
            // Nothing that fits: the system clipboard is left as it was
            // rather than emptied, the same choice `begin_pull` makes for a
            // selection with no type this clipboard carries.
            if !(payloads.is_empty() && !dropped.is_empty()) {
                self.clip.ready = Some(Pulled { payloads, for_drag });
            }
            // The client no longer owns the selection — the system does, and it
            // holds a copy. Telling it so is what lets a clipboard tool exit
            // rather than sit forever waiting to serve a `send` that will never
            // come, and it is what the protocol means by `cancelled`.
            //
            // A drag is **not** cancelled here: its source is still live and is
            // owed a `dnd_finished` when the drop happens.
            if !for_drag && let Some(source) = self.clip.selection.take() {
                self.source_event(source, self.data_ops.source_cancelled, &[]);
            }
        }

        let mut finished = Vec::new();
        for inc in &mut self.clip.incoming {
            let Some(data) = &inc.data else { continue };
            while inc.at < data.len() {
                // SAFETY: a live fd owned by `inc`, writing inside `data`.
                let n = unsafe {
                    libc::write(
                        inc.fd.as_raw_fd(),
                        data[inc.at..].as_ptr().cast(),
                        data.len() - inc.at,
                    )
                };
                if n > 0 {
                    inc.at += n as usize;
                    continue;
                }
                let e = io::Error::last_os_error();
                if e.kind() == io::ErrorKind::WouldBlock || e.kind() == io::ErrorKind::Interrupted {
                    break;
                }
                // The client closed its read end mid-paste, which is legal —
                // a text field that only wanted the first line. Nothing to
                // report.
                inc.at = data.len();
            }
            if inc.at >= data.len() {
                finished.push(inc.token);
            }
        }
        // Dropping the `Incoming` closes the write end, which is the EOF the
        // client is waiting for. A paste that never closed would hang the
        // client just as surely as the connection error this stage removed.
        self.clip.incoming.retain(|i| !finished.contains(&i.token));
    }

    /// Bytes and descriptors this program is sending its client on its own
    /// account, drained by the pump.
    pub fn take_client_messages(&mut self) -> (Vec<u8>, Vec<OwnedFd>) {
        (
            std::mem::take(&mut self.to_client),
            std::mem::take(&mut self.clip.to_client_fds),
        )
    }

    // -----------------------------------------------------------------------
    // §109: lazy pools, a backing per buffer
    // -----------------------------------------------------------------------

    /// The session's stub extent, made the first time a pool needs it.
    fn ensure_stub(&mut self) -> Option<Region> {
        if self.stub.is_none() {
            match self.transport.create(u64::from(STUB_LEN)) {
                Ok(r) => self.stub = Some(r),
                Err(e) => {
                    self.note(format!("could not publish the session's {STUB_LEN}-byte stub: {e}"));
                    return None;
                }
            }
        }
        self.stub.clone()
    }

    /// A live pool to create through: `prefer` if the client still owns it,
    /// else any of the client's live pools. Its `wl_shm` must be live too,
    /// which `shm_release_held` sees to.
    fn carrier(&self, prefer: u32) -> Option<u32> {
        if self.pools.get(&prefer).is_some_and(|p| !p.zombie) {
            return Some(prefer);
        }
        let mut live: Vec<u32> = self.pools.iter().filter(|(_, p)| !p.zombie).map(|(id, _)| *id).collect();
        live.sort_unstable();
        live.first().copied()
    }

    /// Point the compositor's pool `carrier` at `descriptor`: its destroy
    /// (whose `delete_id` this proxy keeps) and `create_pool` again under the
    /// same id. The compositor frees an id at `destroy`, so the same id is
    /// legal at once, and it is still the client's: the client never hears of
    /// either.
    fn repoint(&mut self, carrier: u32, descriptor: &Descriptor, len: u32, stub: bool) -> Vec<u8> {
        let Some(shm) = self.pools.get(&carrier).map(|p| p.shm) else { return Vec::new() };
        *self.swallow.entry(carrier).or_insert(0) += 1;
        let mut out = wire::message(carrier, op::POOL_DESTROY, &[]);
        out.extend_from_slice(&create_pool_message(shm, carrier, descriptor, len));
        if let Some(p) = self.pools.get_mut(&carrier) {
            p.upstream_len = len;
            p.on_stub = stub;
        }
        out
    }

    /// `buffer` as a 1x1 stub on the stub pool, through `carrier`: what the
    /// compositor has for a buffer that is not backed.
    fn stub_buffer(&mut self, carrier: u32, buffer: u32) -> Vec<u8> {
        let mut out = Vec::new();
        let Some(format) = self.buffers.get(&buffer).map(|b| b.format) else { return out };
        let Some(carrier) = self.carrier(carrier) else { return out };
        if !self.pools.get(&carrier).is_some_and(|p| p.on_stub) {
            let Some(stub) = self.ensure_stub() else { return out };
            out.extend_from_slice(&self.repoint(carrier, &stub.descriptor, STUB_LEN, true));
        }
        out.extend_from_slice(&create_buffer_message(carrier, buffer, 0, 1, 1, 4, format));
        self.stats.buffers_stubbed += 1;
        out
    }

    /// Give `buffer` an extent of its own and the compositor the buffer at
    /// its real geometry on it, in place of its stub: destroy the stub (its
    /// `delete_id` kept), re-point a pool at the extent, create the buffer
    /// again under its id. `Err` says why there was no room even after
    /// reclaiming.
    fn back_buffer(&mut self, prefer: u32, buffer: u32) -> Result<Vec<u8>, String> {
        let Some(info) = self.buffers.get(&buffer).cloned() else { return Ok(Vec::new()) };
        if info.backing.is_some() {
            return Ok(Vec::new());
        }
        let Some(carrier) = self.carrier(prefer) else {
            return Err(format!(
                "buffer {buffer}: the client has destroyed every pool, so there is none to create its \
                 extent through"
            ));
        };
        let len = info.extent_len().ok_or_else(|| format!("buffer {buffer}: a geometry with no size"))?;
        let mut out = Vec::new();
        let region = self.allocate(len, &mut out).map_err(|e| format!("buffer {buffer}: {len} bytes: {e}"))?;
        let len32 = u32::try_from(len).map_err(|_| format!("buffer {buffer}: {len} bytes is over 4 GiB"))?;
        *self.swallow.entry(buffer).or_insert(0) += 1;
        out.extend_from_slice(&wire::message(buffer, op::BUFFER_DESTROY, &[]));
        out.extend_from_slice(&self.repoint(carrier, &region.descriptor, len32, false));
        out.extend_from_slice(&create_buffer_message(
            carrier,
            buffer,
            0,
            info.width,
            info.height,
            info.stride,
            info.format,
        ));
        self.stats.buffers_backed += 1;
        self.stats.backed_bytes_now += region.size;
        self.stats.backed_bytes_peak = self.stats.backed_bytes_peak.max(self.stats.backed_bytes_now);
        if self.trace {
            self.note(format!("buffer {buffer}: backed by {}", region.describe()));
        }
        if let Some(b) = self.buffers.get_mut(&buffer) {
            b.backing = Some(region);
            b.last_used = Instant::now();
        }
        // Whatever these arena bytes last held is not this buffer: its first
        // copy is its whole extent.
        self.forget_buffer_arrears(buffer);
        Ok(out)
    }

    /// `len` bytes of the arena, reclaiming cold buffers' extents if there is
    /// no room. The reclaim emits its own messages into `out`, ahead of the
    /// ones that use the bytes.
    fn allocate(&mut self, len: u64, out: &mut Vec<u8>) -> io::Result<Region> {
        match self.transport.create(len) {
            Ok(r) => Ok(r),
            Err(_) => {
                self.stats.alloc_refusals += 1;
                out.extend_from_slice(&self.reclaim_for(len));
                self.transport.create(len)
            }
        }
    }

    fn free_backing(&mut self, region: &Region) {
        self.stats.backed_bytes_now = self.stats.backed_bytes_now.saturating_sub(region.size);
        self.transport.destroy(region);
    }

    /// Give the arena back the extents of buffers nothing is using, until
    /// `needed` bytes are free.
    ///
    /// A buffer's extent is a **copy**: the client's own pool, mapped here,
    /// has the bytes. So a cold buffer can lose its extent and get it back
    /// later, which is what the reclaimer does: the compositor's buffer is
    /// destroyed (the `delete_id` kept) and created again under the same id as
    /// a 1x1 stub, the extent is freed, and the next attach backs it again
    /// ([`Session::back_buffer`]), with a full copy.
    ///
    /// Only a buffer that is **not attached** to any surface (current or
    /// pending), **released** by the compositor (`wl_buffer.release`: it reads
    /// those bytes no longer) and older than [`RECLAIM_FLOOR`]; and only while
    /// the client has a live pool to re-create its stub through. The extent is
    /// freed at once: the compositor holds the buffer released and its destroy
    /// is ahead, in the same stream, of anything that uses the bytes again.
    fn reclaim_for(&mut self, needed: u64) -> Vec<u8> {
        let mut pinned: std::collections::HashSet<u32> = std::collections::HashSet::new();
        for s in self.surfaces.values() {
            pinned.extend(s.current);
            pinned.extend(s.pending_buffer);
        }
        let now = Instant::now();
        let mut cold: Vec<(Duration, u32, u64, u32)> = self
            .buffers
            .iter()
            .filter(|(id, b)| {
                b.backing.is_some() && !b.held && !pinned.contains(*id) && now.duration_since(b.last_used) >= RECLAIM_FLOOR
            })
            .map(|(id, b)| (now.duration_since(b.last_used), *id, b.backing.as_ref().map_or(0, |r| r.size), b.pool))
            .collect();
        cold.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
        let mut out = Vec::new();
        let mut freed = 0u64;
        for (idle, id, size, pool) in cold {
            if freed >= needed {
                break;
            }
            let Some(carrier) = self.carrier(pool) else { break };
            *self.swallow.entry(id).or_insert(0) += 1;
            out.extend_from_slice(&wire::message(id, op::BUFFER_DESTROY, &[]));
            out.extend_from_slice(&self.stub_buffer(carrier, id));
            if let Some(region) = self.buffers.get_mut(&id).and_then(|b| b.backing.take()) {
                self.free_backing(&region);
            }
            self.forget_buffer_arrears(id);
            freed += size;
            self.stats.pools_reclaimed += 1;
            self.note(format!(
                "buffer {id}: reclaiming its {size}-byte extent, idle {} ms -- the arena needs {needed}",
                idle.as_millis()
            ));
        }
        if freed < needed && !self.pressure_noted {
            self.pressure_noted = true;
            self.note(format!(
                "arena pressure: {freed} of {needed} bytes could be reclaimed; the rest is in buffers that \
                 are attached, unreleased or newer than {} ms",
                RECLAIM_FLOOR.as_millis()
            ));
        }
        if freed > 0 {
            self.pressure_noted = false;
        }
        out
    }

    /// The client's `wl_shm.release`s held back for pools that are gone now.
    fn release_held_shm(&mut self) -> Vec<u8> {
        let mut out = Vec::new();
        let held = std::mem::take(&mut self.shm_release_held);
        for shm in held {
            if self.pools.values().any(|p| p.shm == shm && !p.zombie) {
                self.shm_release_held.push(shm);
            } else {
                out.extend_from_slice(&wire::message(shm, op::SHM_RELEASE, &[]));
            }
        }
        out
    }

    /// A fatal protocol error for the client, said in the compositor's place:
    /// `wl_display.error`, after which libwayland-client sends nothing and
    /// the connection ends. The stream to the compositor stays in step,
    /// because nothing more is forwarded.
    fn client_error(&mut self, object: u32, code: u32, message: String) {
        self.note(format!("wl_display.error to the client: object {object}, code {code}: {message}"));
        let mut b = Vec::new();
        b.extend_from_slice(&object.to_ne_bytes());
        b.extend_from_slice(&code.to_ne_bytes());
        wire::push_string(&mut b, &message);
        self.to_client.extend_from_slice(&wire::message(1, op::DISPLAY_ERROR, &b));
        self.dead = true;
    }

    /// A buffer that cannot have an extent even after reclaiming: a clean
    /// `no_memory`, naming it, rather than a stream out of step.
    fn no_memory(&mut self, object: u32, why: String) {
        self.stats.no_memory_errors += 1;
        let (free, largest) = self.transport.free_bytes().unwrap_or((0, 0));
        self.client_error(
            object,
            DISPLAY_ERROR_NO_MEMORY,
            format!(
                "could not publish {why} -- the shared arena has {free} bytes free, the largest run {largest} \
                 (wc-proxy, §109)"
            ),
        );
    }

    /// Translate one event from the compositor. Returns the bytes for the local
    /// client and any descriptors that must ride beside them as `SCM_RIGHTS`.
    pub fn server_message(
        &mut self,
        header: Header,
        body: &[u8],
    ) -> Result<(Vec<u8>, Vec<std::os::fd::OwnedFd>), Error> {
        self.stats.messages_server += 1;
        if self.native_event(header.object, header.opcode) { return Ok((Vec::new(), Vec::new())); }
        // §104. Answers to objects the dma-buf path made upstream on the
        // client's behalf are this proxy's, not the client's.
        if let Some(bytes) = self.dmabuf_event(header.object, header.opcode, body) {
            return Ok((bytes, Vec::new()));
        }
        let interface = self
            .objects
            .interface(header.object)
            .ok_or(Error::UnknownObject { id: header.object, opcode: header.opcode })?
            .to_owned();
        let msg = self
            .spec
            .event(&interface, header.opcode)
            .ok_or_else(|| Error::UnknownOpcode {
                interface: interface.clone(),
                opcode: header.opcode,
            })?
            .clone();
        let spans = wire::walk(body, &msg.args, FdEncoding::InBand)
            .map_err(|e| Error::Walk(format!("{interface}.{} (event): {e}", msg.name)))?;
        if self.trace {
            self.note(format!("<- {interface}@{}.{}", header.object, msg.name));
        }

        self.record_new_ids(body, &spans, &msg.args);
        if sv_transport::frametrace::enabled()
            && trace_event(&interface, &msg.name, header.object, body, &spans)
        {
            self.trace_inputs += 1;
        }
        // §88. A frame callback this side already answered is the compositor's
        // credit coming back, not news for the client.
        if interface == "wl_callback"
            && header.opcode == op::CALLBACK_DONE
            && let Some(p) = self.pacer.as_mut()
        {
            match p.answered(header.object) {
                crate::pace::Answer::Swallow => return Ok((Vec::new(), Vec::new())),
                crate::pace::Answer::NotOurs => {}
            }
        }
        if interface == "wl_callback" && header.opcode == op::CALLBACK_DONE && sv_transport::frametrace::enabled() {
            sv_transport::frametrace::emit("px.done", format_args!("cb={} src=guest", header.object));
        }
        if interface == "wl_display"
            && header.opcode == op::DISPLAY_DELETE_ID
            && let Some(p) = self.pacer.as_mut()
            && let Some(id) = spans.first().and_then(|s| wire::word_at(body, s))
            && p.deleted(id)
        {
            // Sent after the proxy's own `done` for it; see `crate::pace`.
            return Ok((Vec::new(), Vec::new()));
        }

        // STAGE 13. The compositor's own announcements are forwarded untouched,
        // and two of them are *watched* on the way past for the same reason
        // `xdg_toplevel.set_app_id` is: this side has to know something the
        // message is not addressed to it about. `wl_data_device.enter` marks an
        // offer as a drag's rather than the clipboard's, which decides what a
        // later `receive` on it asks Windows for.
        if interface == "wl_data_device" {
            match msg.name.as_str() {
                // enter(serial, surface, x, y, id)
                "enter" => {
                    if let Some(span) = spans.get(4)
                        && let Some(id) = wire::word_at(body, span)
                        && id != 0
                    {
                        self.clip.drag_offers.insert(id);
                        self.note(format!("wl_data_device.enter: offer {id} is a drag's"));
                    }
                }
                // The drag is over one way or the other. The ids stay valid
                // until the client destroys them, but nothing more will be
                // received from them.
                // The drag is over one way or the other. An offer that was
                // *dropped* outlives this — see `dropped_offers`.
                "leave" => {
                    let forget: Vec<u32> = self
                        .clip
                        .drag_offers
                        .iter()
                        .copied()
                        .filter(|id| !self.clip.dropped_offers.contains(id))
                        .collect();
                    if !forget.is_empty() {
                        self.note(format!(
                            "wl_data_device.leave: forgetting undropped drag offers {forget:?}"
                        ));
                    }
                    for id in forget {
                        self.clip.drag_offers.remove(&id);
                    }
                }
                // The user let go over one of our surfaces. Every offer `enter`
                // announced is now one the client may still `receive` from.
                "drop" => {
                    let dropped: Vec<u32> = self.clip.drag_offers.iter().copied().collect();
                    self.note(format!("wl_data_device.drop on offers {dropped:?}"));
                    self.clip.dropped_offers.extend(dropped);
                }
                _ => {}
            }
        }

        // §102. **The presentation clock does not cross machines** (§88's
        // finding for the frame clock, and the same reason). The compositor's
        // `presented` timestamp is a reading of the *guest's* clock — Windows'
        // performance counter, epoch the guest's boot — and a client compares
        // it with its own `CLOCK_MONOTONIC`: KWin subtracts the time its frame
        // callback arrived to size its render loop's safety margin, and a
        // timestamp from another machine's clock makes that margin anything
        // from hours to negative. So the proxy says what is true on this side:
        // the frame was shown no later than now, on this machine's monotonic
        // clock, and `clock_id` names that clock. The delay from the guest's
        // tick to this moment is one crossing, which is what a presentation
        // timestamp read off another machine would be wrong by at best anyway.
        // `refresh`, `seq` and `flags` are the compositor's and pass through.
        let mut rewritten: Option<Vec<u8>> = None;
        if interface == "wp_presentation" && msg.name == "clock_id" && body.len() >= 4 {
            let mut b = body.to_vec();
            b[0..4].copy_from_slice(&(libc::CLOCK_MONOTONIC as u32).to_ne_bytes());
            rewritten = Some(b);
        }
        if interface == "wp_presentation_feedback" && msg.name == "presented" && body.len() >= 12 {
            let (secs, nanos) = monotonic_now();
            let mut b = body.to_vec();
            b[0..4].copy_from_slice(&((secs >> 32) as u32).to_ne_bytes());
            b[4..8].copy_from_slice(&(secs as u32).to_ne_bytes());
            b[8..12].copy_from_slice(&nanos.to_ne_bytes());
            rewritten = Some(b);
        }

        if interface == "wl_buffer" && header.opcode == op::BUFFER_RELEASE {
            self.native_released(header.object);
        }

        // STAGE 16. The compositor has released a buffer, which is its own word
        // for "I am no longer reading those bytes" and is the reclaimer's
        // interlock. Forwarded unchanged — the client needs it far more than
        // this proxy does.
        if interface == "wl_buffer"
            && header.opcode == op::BUFFER_RELEASE
            && let Some(b) = self.buffers.get_mut(&header.object)
        {
            b.held = false;
        }

        if interface == "wl_display"
            && header.opcode == op::DISPLAY_DELETE_ID
            && let Some(id) = wire::word_at(body, &spans[0])
        {
            // Whose destructor was this the answer to? If this proxy sent it
            // (§109: a pool re-pointed, a buffer backed or reclaimed), the answer
            // is this proxy's and **must not be forwarded**: the client still owns
            // that id. The answers come back in the order the destroys went, so
            // a count per id is exact.
            if let Some(n) = self.swallow.get_mut(&id)
                && *n > 0
            {
                *n -= 1;
                if *n == 0 {
                    self.swallow.remove(&id);
                }
                return Ok((Vec::new(), Vec::new()));
            }
            self.native_deleted(id);
            self.objects.forget(id);
            self.clip.drag_offers.remove(&id);
            self.clip.dropped_offers.remove(&id);
            // §109: a buffer the client destroyed: the compositor has finished
            // with its extent.
            if let Some(r) = self.pending_free.remove(&id) {
                self.free_backing(&r);
            }
            // A pool the client destroyed: its mapping goes with its last
            // buffer, and while buffers cut from it live on, the id is the
            // client's to reuse from this message on (§102.9).
            self.release_pool_if_done(id);
            if self.pools.get(&id).is_some_and(|p| p.zombie) {
                self.retire_pool_id(id);
            }
        }

        // Materialise a real descriptor from the *name* the compositor sent.
        // See this module's header for why that is not the forbidden direction.
        //
        // **A failure here is fatal, and that changed in stage 6.** It used to
        // be a note: the descriptor bytes were removed from the body and the
        // message was forwarded without its fd. That is the §2.4
        // desynchronisation in the direction nobody had exercised — the client
        // receives a perfectly well-formed `wl_keyboard.keymap` and calls
        // `mmap` on a descriptor it never received. libwayland aborts the
        // client, and the last thing in its log is a keymap event that looks
        // correct. Killing the connection with the descriptor's kind in the
        // message is the diagnosis; forwarding was the mystery.
        let mut fds = Vec::new();
        if msg.has_fd() {
            for span in spans.iter().filter(|s| s.kind == ArgKind::Fd) {
                let bytes = wire::bytes_at(body, span).ok_or_else(|| Error::MissingFd {
                    interface: interface.clone(),
                    message: msg.name.clone(),
                })?;
                let d = Descriptor::decode(bytes)
                    .map_err(|e| Error::Walk(format!("{interface}.{}: {e}", msg.name)))?;
                let fd = materialise(&d, &*self.transport).map_err(|e| Error::Materialise {
                    interface: interface.clone(),
                    message: msg.name.clone(),
                    descriptor: format!("{d:?}"),
                    source: e,
                })?;
                self.stats.fds_materialised += 1;
                if matches!(d, Descriptor::ArenaSlice { .. }) {
                    self.stats.fds_from_arena += 1;
                }
                fds.push(fd);
            }
        }

        let out_body = if let Some(b) = rewritten {
            b
        } else if msg.has_fd() {
            wire::splice_fds(body, &spans, FdEncoding::InBand, &[])
        } else {
            body.to_vec()
        };
        let mut out = wire::message(header.object, header.opcode, &out_body);
        // §104. The private `zwp_linux_dmabuf_v1` global rides in after the
        // compositor's `wl_shm`.
        self.dmabuf_registry(&interface, header.object, header.opcode, body, &spans, &mut out);
        self.native_registry(&interface, header.object, header.opcode, body, &spans, &mut out);
        Ok((out, fds))
    }

    /// An id has just been declared as `iface`. Anything this file remembers
    /// about a *buffer* with that id is now about somebody else's object.
    ///
    /// # STAGE 16, and it is the one regression this stage caused
    ///
    /// Wayland ids are recycled: a client destroys `wl_buffer@44`, the server
    /// acknowledges with `delete_id`, and the next `wl_compositor.create_region`
    /// is `wl_region@44`. `self.buffers` was never purged on that transition and
    /// it did not matter, because nothing ever *replayed* what was in it.
    ///
    /// [`Session::flush_deferred`] replays it. A stale `BufferInfo` left under
    /// an id that is now a live `wl_region` made the rebuild emit
    /// `wl_shm_pool.create_buffer(new_id = 44)` for an id the compositor already
    /// had, which is a protocol error and a dead client — the very failure this
    /// stage exists to remove, reintroduced by its own fix. Measured, against a
    /// real Firefox on a starved arena: `Invalid new_id: 44`, and the upstream
    /// dump showed 44 being created as a region four messages earlier.
    ///
    /// The purge belongs here rather than at `delete_id` because this is the
    /// moment the id provably means something else, and it is true whether the
    /// acknowledgement came from the compositor or was invented by
    /// [`Session::fabricate_delete_id`].
    fn forget_stale_buffer(&mut self, id: u32, iface: &str) {
        if iface != "wl_buffer" {
            self.foreign_buffers.remove(&id);
        }
        if iface != "wl_buffer"
            && let Some(stale) = self.buffers.remove(&id)
            && let Some(pool) = self.pools.get_mut(&stale.pool)
        {
            pool.buffers = pool.buffers.saturating_sub(1);
        }
    }

    fn record_new_ids(&mut self, body: &[u8], spans: &[Span], args: &[crate::spec::Arg]) {
        for (span, arg) in spans.iter().zip(args) {
            match arg.kind {
                ArgKind::NewId => {
                    if let (Some(id), Some(iface)) =
                        (wire::word_at(body, span), arg.interface.as_deref())
                    {
                        self.forget_stale_buffer(id, iface);
                        self.objects.record(id, iface);
                    }
                }
                ArgKind::NewIdUntyped => {
                    if let Some((iface, _version, id)) = wire::untyped_new_id(body, span) {
                        self.forget_stale_buffer(id, &iface);
                        if self.spec.interface(&iface).is_none() {
                            self.objects.note_unknown();
                            // **This is fatal on the next message, not on some
                            // later one.** The wording here used to say the
                            // connection would fail "if it ever carries an h",
                            // which is wrong and was corrected in stage 21
                            // after it nearly cost somebody an afternoon:
                            // `client_message` looks the opcode up in the spec
                            // *before* it walks anything (see the top of that
                            // function), so the very first request on an
                            // unknown interface is `Error::UnknownOpcode` and
                            // the connection ends. An interface with no fds
                            // anywhere in it is no safer than one full of them.
                            //
                            // So this note is a warning about the *next*
                            // message, and the fix is always the same: copy the
                            // XML into `protocols/` and add a line to
                            // `Spec::builtin`.
                            self.note(format!(
                                "client bound {iface}, which is not in the vendored protocols; \
                                 its first request cannot be walked and will end the connection"
                            ));
                        }
                        self.objects.record(id, &iface);
                    }
                }
                _ => {}
            }
        }
    }

    /// Release a pool's region once every condition in [`Pool::releasable`] is
    /// met. Called from all three places that can make one true.
    fn release_pool_if_done(&mut self, pool_id: u32) {
        if !self.pools.get(&pool_id).is_some_and(Pool::releasable) {
            return;
        }
        if self.pools.remove(&pool_id).is_some() {
            self.stats.pools_released += 1;
        }
    }

    /// Move a pool whose id the client no longer owns to a key of this proxy's
    /// own, and point its buffers there.
    ///
    /// §102.9, and it is why a nested KWin's window kept its first frame on
    /// the guest. `wl_shm` lets a client destroy a pool and keep drawing from
    /// its buffers, and once the compositor has answered the destructor with
    /// `delete_id` the client's libwayland reuses the id. KWin imports every
    /// swapchain slot as pool, buffer, pool destroy, lazily at the slot's first
    /// present, so its second slot's pool is often given its first slot's id
    /// (the guest's log: `pool 29` published twice at 3604928 bytes). Keyed on
    /// the wire id, that `create_pool` replaced the first pool's entry: the
    /// first slot's `BufferInfo::pool` then named the second pool, its commits
    /// copied the second slot's memory into the second slot's slice, and the
    /// slice the compositor read for the first slot never changed again. The
    /// replaced entry's region was never returned either.
    ///
    /// Safe because nothing addresses a zombie pool on the wire any more:
    /// `flush_deferred` and `reclaim_for` both skip zombies, and its buffers
    /// are addressed by their own ids.
    fn retire_pool_id(&mut self, id: u32) {
        let Some(pool) = self.pools.remove(&id) else {
            return;
        };
        let mut key = self.next_retired_pool;
        while self.pools.contains_key(&key) {
            key = key.checked_add(1).unwrap_or(RETIRED_POOL_KEYS);
        }
        self.next_retired_pool = key.checked_add(1).unwrap_or(RETIRED_POOL_KEYS);
        let live = pool.buffers;
        self.pools.insert(key, pool);
        for b in self.buffers.values_mut().filter(|b| b.pool == id) {
            b.pool = key;
        }
        if self.trace {
            self.note(format!(
                "pool {id}: id retired by the client with {live} buffer(s) still cut from it"
            ));
        }
    }

    /// Take the client's pool fd and map it. The pool is *not* announced yet;
    /// see [`Session::flush_deferred`].
    fn create_pool(
        &mut self,
        pool_id: u32,
        shm: u32,
        fd: std::os::fd::OwnedFd,
        size: i32,
    ) -> Result<(), Error> {
        let size = usize::try_from(size)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "negative pool size"))?;
        let client = ClientMap::new(fd, size)?;
        // §102.9. Only reachable if a `delete_id` for the old pool went past
        // without `retire_pool_id`; overwriting would hand its live buffers to
        // this pool and leak its region, so it is moved aside here too.
        if self.pools.contains_key(&pool_id) {
            self.retire_pool_id(pool_id);
        }
        self.pools.insert(
            pool_id,
            Pool {
                client,
                shm,
                size,
                zombie: false,
                buffers: 0,
                upstream_len: STUB_LEN,
                on_stub: true,
            },
        );
        Ok(())
    }

    /// The client's pool has grown: its mapping follows. Returns the size to
    /// forward, which is the compositor's pool's own (`Pool::upstream_len`).
    fn resize_pool(&mut self, pool_id: u32, size: i32) -> u32 {
        self.stats.pool_resizes += 1;
        let Some(pool) = self.pools.get_mut(&pool_id) else {
            return STUB_LEN;
        };
        let have = pool.upstream_len;
        if let Ok(size) = usize::try_from(size)
            && size > pool.size
        {
            if let Err(e) = pool.client.remap(size) {
                self.note(format!("pool {pool_id}: could not remap the client's pool: {e}"));
                return have;
            }
            pool.size = size;
        }
        have
    }

    /// Drop every surface's record of what buffer `id` owes.
    ///
    /// Called from both ends of a buffer's life, and the *creation* end is the
    /// one that is load-bearing. `wl_display.delete_id` retires ids for reuse
    /// — this proxy implements that deliberately (`delete_id_retires_an_id_for_
    /// reuse`) — so id 12 can be a fresh buffer a frame after the old id 12 was
    /// destroyed. A leftover "owes nothing" against a buffer whose extent has
    /// never been copied would skip the one copy that had to happen, and it
    /// would do it only on clients that recycle ids, which is the worst kind of
    /// bug to find later.
    fn forget_buffer_arrears(&mut self, id: u32) {
        for surface in self.surfaces.values_mut() {
            surface.stale.remove(&id);
        }
    }

    /// Turn the release policy's clock without a frame having been drawn.
    ///
    /// DYNAMIC ARENA. `commit` was the only thing that advanced it, so the
    /// policy stopped whenever the client stopped drawing — and "the client
    /// stopped drawing" is the state in which giving memory back matters most.
    /// Called from the pump's poll timeout; see the note there for why a
    /// timeout is the right unit.
    pub fn tick_idle(&mut self) {
        for (id, bytes, origin) in self.transport.frame() {
            self.stats.regions_released += 1;
            self.stats.region_bytes_released += bytes;
            self.note(format!("arena region {id} returned while idle: {bytes} bytes of {origin}"));
        }
    }

    /// The frame path: apply the pending state and move the pixels.
    fn commit(&mut self, surface_id: u32) {
        self.stats.commits += 1;
        // DYNAMIC ARENA. One frame has gone by, which is the unit the region
        // release policy is written in. A transport with no regions to return
        // returns none and this costs a virtual call; the arena may hand back a
        // region that has been empty long enough to be worth unmapping.
        for (id, bytes, origin) in self.transport.frame() {
            self.stats.regions_released += 1;
            self.stats.region_bytes_released += bytes;
            self.note(format!("arena region {id} returned: {bytes} bytes of {origin}"));
        }
        let Some(surface) = self.surfaces.get_mut(&surface_id) else {
            self.stats.verify_unreached += 1;
            return;
        };
        let attached_now = surface.pending_attached;
        if surface.pending_attached {
            surface.current = surface.pending_buffer;
            surface.pending_attached = false;
        }
        let Some(buffer_id) = surface.current else {
            surface.damage.clear();
            self.stats.verify_unreached += 1;
            return;
        };
        // §102. Nothing to copy for a buffer with no pool, ever; its damage is
        // the compositor's to read from the forwarded requests, and is dropped
        // here so it does not pile up across a run of KWin's commits.
        if self.foreign_buffers.contains(&buffer_id) {
            surface.damage.clear();
            self.stats.foreign_commits += 1;
            // §104. A dma-buf buffer is the shared allocation itself: the
            // compositor reads it by name, so this commit moved nothing.
            let native = self.native_shm.contains_buffer(buffer_id);
            if native { self.native_shm.committed(buffer_id); }
            if self.is_dmabuf_buffer(buffer_id) || native {
                if !native { self.stats.dmabuf_commits += 1; }
                if sv_transport::frametrace::enabled() {
                    sv_transport::frametrace::emit(
                        "px.commit",
                        format_args!(
                            "surf={surface_id} buf={buffer_id} attached={} copied=0 zero_copy=1",
                            attached_now as u8
                        ),
                    );
                }
            }
            return;
        }
        let Some(full_info) = self.buffers.get(&buffer_id).cloned() else {
            self.stats.verify_unreached += 1;
            return;
        };
        // §109: the buffer's own extent. An attach always backs it first, so
        // one with none is a commit naming a buffer this proxy never saw
        // attached; the damage stays for the commit that can copy it.
        let Some(region) = full_info.backing.clone() else {
            self.stats.verify_unreached += 1;
            return;
        };
        if !self.pools.contains_key(&full_info.pool) {
            self.stats.verify_unreached += 1;
            return;
        }
        let damage = std::mem::take(&mut self.surfaces.get_mut(&surface_id).expect("just read").damage);
        if let Some(b) = self.buffers.get_mut(&buffer_id) {
            // The compositor is about to be told to read these bytes, and until
            // it says `release` it may be doing so. The reclaimer reads this.
            b.held = true;
            b.last_used = Instant::now();
        }
        // The client's copy starts at the buffer's offset in its pool; this
        // side's at 0 of the buffer's extent. `info` is the buffer at 0, and
        // `src` the client's pool from the buffer's offset, so every copy
        // below lines the two up.
        let info = full_info.at_zero();
        let pool = self.pools.get(&full_info.pool).expect("just read");
        let src_all = pool.client.as_slice();
        let src = &src_all[usize::try_from(full_info.offset).unwrap_or(0).min(src_all.len())..];
        let Some(dst) = self.transport.bytes_mut(&region) else {
            self.stats.verify_unreached += 1;
            return;
        };
        let usable = src.len().min(dst.len());

        // Deliberately after every early return above: settling this buffer's
        // arrears is only honest if the copy that settles them actually
        // happens. `surfaces` is a different field from `pools` and
        // `transport`, so this borrow is disjoint from `src` and `dst`.
        let owed = match self.scope {
            CopyScope::Damage => self
                .surfaces
                .get_mut(&surface_id)
                .map_or(Stale::Everything, |s| s.arrears(buffer_id, &damage)),
            // The other two scopes copy the extent or the pool every commit, so
            // no buffer can fall behind and there is nothing to track.
            _ => Stale::Everything,
        };

        // Only for the diagnostic below, and only when it is switched on: the
        // shape of what was owed, before it is consumed.
        let owed_kind = if self.verify {
            match &owed {
                Stale::Everything => "extent".to_owned(),
                Stale::Rects(r) => format!("{} rects", r.len()),
            }
        } else {
            String::new()
        };

        // §84. The client's bytes as they were *before* the copy, so that a
        // stale row can say which side moved. Verify mode only: it is a second
        // full read of the buffer, which is the instrument's cost, not the
        // copy's, and it is taken outside the timed span.
        let before: Option<Vec<u8>> = (self.verify && attached_now)
            .then(|| extent_bytes(src, &info, usable).map(<[u8]>::to_vec))
            .flatten();
        let started = Instant::now();
        let copied = match self.scope {
            // §109: a buffer is its own extent, so the pool scope is the
            // buffer's.
            CopyScope::Pool | CopyScope::Buffer => copy_extent(src, dst, &info, usable),
            CopyScope::Damage => {
                // Not `damage` — what *this buffer's* copy owes, which is this
                // frame's damage plus every frame's since it was last on
                // screen. An empty result means the client asserted nothing
                // has changed anywhere since this buffer was last copied, and
                // taking it at its word is what the protocol means; the
                // second-guessing that used to live here was hiding the arrears
                // bug for single-buffered clients only.
                //
                // STAGE 16 adds one override, and it is a different fact from
                // arrears. §27's `Stale` tracks what changed on the *surface*
                // since this buffer was last copied, which is exactly right
                // while the copy's destination stays put. It does not — a pool
                // refused for want of room and later published, or one this
                // stage's reclaimer tore down and rebuilt, lands in *different
                // arena bytes*, and the previous tenant's pixels are in them.
                // No amount of surface damage describes that, so the first copy
                // after such a gap is the whole extent. See
                // `Surface::needs_full_copy`.
                match owed {
                    Stale::Everything => copy_extent(src, dst, &info, usable),
                    Stale::Rects(rects) if rects.is_empty() => 0,
                    // **Never pay more than the extent.** `copy_damage` copies
                    // overlapping rectangles twice, deliberately, because
                    // computing their union costs more than the duplicate copy
                    // when there are two of them. Arrears can make there be
                    // sixty, and sixty copies of the same rectangle is sixty
                    // times the buffer.
                    //
                    // Measured, and it is the regression this branch shipped
                    // for one commit: `weston-presentation-shm` damages its
                    // whole 250x250 buffer every frame and cycles buffers, so a
                    // buffer coming back round owed one full-buffer rectangle
                    // per intervening frame. `scripts/wc-proxy.sh cost` went
                    // from 168 MiB to 8583 MiB for the same 700 commits — 13
                    // MiB per copy of a 244 KiB buffer — and every assertion in
                    // that harness still passed, because the table is printed
                    // and not checked.
                    //
                    // The bound is exact rather than a heuristic: the sum of
                    // the rectangle areas is precisely what `copy_damage` will
                    // move, so comparing it against the extent picks the
                    // cheaper of the two every time and can never be wrong by
                    // more than the padding between rows.
                    Stale::Rects(rects) => {
                        if damage_area(&rects, &info) >= extent_area(&info) {
                            copy_extent(src, dst, &info, usable)
                        } else {
                            copy_damage(src, dst, &info, &rects, usable)
                        }
                    }
                }
            }
        };
        // Deliberately outside the timed span above: this is an instrument, and
        // §13.8's retracted throughput number is what happens when one is left
        // inside the measurement.
        self.stats.copy_nanos += started.elapsed().as_nanos() as u64;
        if sv_transport::frametrace::enabled() {
            // docs/waycompose-design.md §88: the proxy's half of a frame.
            sv_transport::frametrace::emit_at(
                sv_transport::frametrace::at_us(started),
                "px.commit",
                format_args!(
                    "surf={surface_id} buf={buffer_id} attached={} copied={copied} copy_us={}",
                    attached_now as u8,
                    sv_transport::frametrace::us(started.elapsed()),
                ),
            );
        }
        // **Only a commit that attached a buffer can be checked**, and this is
        // the instrument's own correctness rather than a convenience.
        //
        // A commit with no `attach` is a state-only commit — an `ack_configure`,
        // an opaque region, a subsurface position — and it does not change what
        // the surface shows. It also does not re-take ownership of the current
        // buffer, which the compositor released frames ago, so the client is
        // very likely drawing the *next* frame into it at this exact moment.
        // Comparing then measures the client's work in progress and reports a
        // difference that is nobody's fault. Measured before it was believed:
        // applied to every commit, a *fixed* proxy still reported 91 stale of
        // 936 on a scrolling Firefox, and every one logged in full carried no
        // damage and no arrears — a commit that attached nothing. Gated, the
        // same build reports 0 of 1360 and the pre-fix build still reports 3 of
        // 1509, which is what says the gate removed noise and not the finding.
        // That is also why the two are counted separately below rather than one
        // being quietly dropped.
        if self.verify {
            if !attached_now {
                self.stats.verify_skipped += 1;
            } else {
                let stale = stale_rows(src, dst, &info, usable);
                self.stats.verified += 1;
                if stale > 0 {
                    self.stats.stale_commits += 1;
                    self.stats.stale_rows_worst = self.stats.stale_rows_worst.max(stale as u64);
                    // The first few in full. A count says a bug exists; these
                    // say which one, and twenty is enough to see a pattern
                    // without turning a 4000-commit run into a 4000-line log.
                    if self.stats.stale_commits <= 20 {
                        let arena_offset = match &region.descriptor {
                            Descriptor::ArenaSlice { offset, .. } => Some(*offset),
                            _ => None,
                        };
                        let detail = before
                            .as_deref()
                            .map(|b| stale_detail(src, dst, b, &info, usable, arena_offset))
                            .unwrap_or_default();
                        self.notes.push(format!(
                            "stale: surface {surface_id} buffer {buffer_id} \
                             {}x{} stride {} offset {} -- {stale} rows differ after copying \
                             {owed_kind} (this commit damaged {} rects, first {:?}){detail}",
                            info.width,
                            info.height,
                            info.stride,
                            info.offset,
                            damage.len(),
                            damage.first(),
                        ));
                    }
                }
            }
        }
        if copied > 0 {
            self.stats.copies += 1;
            self.stats.bytes_copied += copied as u64;
        }
    }
}

/// `WC_FRAME_TRACE` (docs/waycompose-design.md §88): the requests a frame's
/// pacing is made of, as they pass on the way to the compositor.
/// §102. `CLOCK_MONOTONIC` now, as whole seconds and nanoseconds — the shape
/// of `wp_presentation_feedback.presented`'s timestamp.
fn monotonic_now() -> (u64, u32) {
    let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    // SAFETY: a valid clock id and a writable timespec.
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
    (ts.tv_sec.max(0) as u64, ts.tv_nsec.clamp(0, 999_999_999) as u32)
}

fn trace_request(interface: &str, name: &str, object: u32, body: &[u8], spans: &[wire::Span]) {
    use sv_transport::frametrace as ft;
    match (interface, name) {
        ("wl_surface", "frame") => {
            let cb = spans.first().and_then(|s| wire::word_at(body, s)).unwrap_or(0);
            ft::emit("px.frame", format_args!("surf={object} cb={cb}"));
        }
        ("wl_surface", "commit") => ft::emit("px.c", format_args!("surf={object}")),
        ("wl_surface", "attach") => {
            let buf = spans.first().and_then(|s| wire::word_at(body, s)).unwrap_or(0);
            ft::emit("px.attach", format_args!("surf={object} buf={buf}"));
        }
        _ => {}
    }
}

/// The events that answer them, on the way back to the client — and, since
/// §89, the input events, as they arrive from the compositor. Their `t` is the
/// event's own `time` argument, which the compositor's `wc.in` line carries
/// too, and is what `scripts/wc-input-latency.py` pairs the two machines on.
///
/// Returns whether this was an input event.
fn trace_event(interface: &str, name: &str, object: u32, body: &[u8], spans: &[wire::Span]) -> bool {
    use sv_transport::frametrace as ft;
    let word = |i: usize| spans.get(i).and_then(|s| wire::word_at(body, s)).unwrap_or(0);
    let fixed = |i: usize| f64::from(word(i) as i32) / 256.0;
    match (interface, name) {
        // The compositor's answer arriving; `px.done` is the client being told.
        ("wl_callback", "done") => ft::emit("px.gdone", format_args!("cb={object}")),
        ("wl_buffer", "release") => ft::emit("px.release", format_args!("buf={object}")),
        ("wl_pointer", "motion") => {
            ft::emit("px.in", format_args!("kind=motion t={} x={} y={}", word(0), fixed(1), fixed(2)));
            return true;
        }
        ("wl_pointer", "enter") => {
            ft::emit("px.in", format_args!("kind=enter x={} y={}", fixed(2), fixed(3)));
            return true;
        }
        ("wl_pointer", "button") => {
            ft::emit("px.in", format_args!("kind=button t={} b={} down={}", word(1), word(2), word(3)));
            return true;
        }
        ("wl_pointer", "axis") => {
            ft::emit("px.in", format_args!("kind=axis t={} axis={} v={}", word(0), word(1), fixed(2)));
            return true;
        }
        ("wl_keyboard", "key") => {
            ft::emit("px.in", format_args!("kind=key t={} code={} down={}", word(1), word(2), word(3)));
            return true;
        }
        _ => {}
    }
    false
}

/// Which of a client's offered mime types to actually pull. See
/// [`WANTED_TEXT`].
fn wanted_mimes(offered: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    if let Some(text) = WANTED_TEXT
        .iter()
        .find(|want| offered.iter().any(|m| m == *want))
    {
        out.push((*text).to_owned());
    }
    if offered.iter().any(|m| m == WANTED_HTML) {
        out.push(WANTED_HTML.to_owned());
    }
    if offered.iter().any(|m| m == WANTED_IMAGE) {
        out.push(WANTED_IMAGE.to_owned());
    }
    out
}

/// The most one selection may carry to the compositor, in bytes.
///
/// `wc-compositor`'s `clipboard::MAX_SELECTION_BYTES`, written down twice for
/// the reason `CHUNK` is (the two crates share an XML file and no code). A
/// test on each side pins the number.
pub(crate) const MAX_SELECTION_BYTES: usize = 32 << 20;

/// §93. Make a pulled selection fit in `cap` bytes: the text is what a paste
/// most needs, so everything else goes first, largest first; a text that does
/// not fit on its own goes too. Returns what is kept and the mime types left
/// out.
pub(crate) fn fit_selection(mut payloads: Payloads, cap: usize) -> (Payloads, Vec<String>) {
    let mut dropped = Vec::new();
    let total = |p: &Payloads| p.iter().map(|(_, b)| b.len()).sum::<usize>();
    while total(&payloads) > cap {
        let is_text = |m: &str| WANTED_TEXT.contains(&m);
        let victim = payloads
            .iter()
            .enumerate()
            .filter(|(_, (m, _))| !is_text(m))
            .max_by_key(|(_, (_, b))| b.len())
            .map(|(i, _)| i)
            .unwrap_or_else(|| {
                payloads
                    .iter()
                    .enumerate()
                    .max_by_key(|(_, (_, b))| b.len())
                    .map_or(0, |(i, _)| i)
            });
        let (mime, bytes) = payloads.remove(victim);
        dropped.push(format!("{mime} ({} bytes)", bytes.len()));
    }
    (payloads, dropped)
}

/// A Wayland string is NUL-terminated inside its length.
fn nul(s: &str) -> Vec<u8> {
    let mut v = s.as_bytes().to_vec();
    v.push(0);
    v
}

/// `pipe2(O_CLOEXEC)`, as `(read, write)`.
fn pipe() -> io::Result<(OwnedFd, OwnedFd)> {
    let mut fds = [0 as libc::c_int; 2];
    // SAFETY: a two-element array, and a flag `pipe2` accepts.
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
        return Err(io::Error::last_os_error());
    }
    use std::os::fd::FromRawFd as _;
    // SAFETY: both were just returned by `pipe2` and are owned here.
    unsafe { Ok((OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1]))) }
}

fn set_nonblocking(fd: &OwnedFd) -> io::Result<()> {
    // SAFETY: a live fd; `F_GETFL` and `F_SETFL` take no pointer arguments.
    let flags = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETFL) };
    if flags < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: as above.
    if unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// How many of the attached buffer's rows the region disagrees with the client
/// about. Zero is the only acceptable answer; see `Session::verify`.
///
/// Only `width * 4` bytes of each row are compared. The rest of the stride is
/// padding the client never draws and the compositor never reads, and
/// `copy_damage` copies it only incidentally, when a rectangle happens to be
/// full-width — so comparing it would report a difference that no eye could
/// ever see and no code could ever use.
fn stale_rows(src: &[u8], dst: &[u8], info: &BufferInfo, usable: usize) -> usize {
    let (Ok(base), Ok(stride), Ok(width)) = (
        usize::try_from(info.offset),
        usize::try_from(info.stride),
        usize::try_from(info.width),
    ) else {
        return 0;
    };
    let mut stale = 0;
    for row in 0..info.height.max(0) as usize {
        let from = base + row * stride;
        let to = from + width * 4;
        if to > usable || to > src.len() || to > dst.len() {
            break;
        }
        if src[from..to] != dst[from..to] {
            stale += 1;
        }
    }
    stale
}

/// One buffer's extent within the pool, as `copy_extent` would read it.
fn extent_bytes<'a>(src: &'a [u8], info: &BufferInfo, usable: usize) -> Option<&'a [u8]> {
    let start = usize::try_from(info.offset).ok()?;
    let end = info.end()?.min(usable).min(src.len());
    (end > start).then(|| &src[start..end])
}

/// §84. Which side moved, for the first few stale rows of one commit.
///
/// `stale_rows` runs microseconds after the copy, in the same thread, so a
/// row that differs there has changed under the copy rather than been missed by
/// it. Three facts tell the cases apart, all read here rather than inferred:
/// whether the **client's** row still equals the snapshot taken before the copy
/// (`src moved`: the client wrote a buffer it had committed); whether the
/// **destination** still holds the snapshot (`dst kept`: if not, something
/// other than this copy wrote those arena bytes, or two rows of the view are
/// the same memory); and where in the arena the row lives, to the page, so the
/// row can be held against the locate's page list and the stitched runs.
fn stale_detail(
    src: &[u8],
    dst: &[u8],
    before: &[u8],
    info: &BufferInfo,
    usable: usize,
    arena_offset: Option<u64>,
) -> String {
    let (Ok(base), Ok(stride), Ok(width)) = (
        usize::try_from(info.offset),
        usize::try_from(info.stride),
        usize::try_from(info.width),
    ) else {
        return String::new();
    };
    let mut out = String::new();
    let mut shown = 0;
    for row in 0..info.height.max(0) as usize {
        let from = base + row * stride;
        let to = from + width * 4;
        if to > usable || to > src.len() || to > dst.len() || to - base > before.len() {
            break;
        }
        if src[from..to] == dst[from..to] {
            continue;
        }
        let snap = &before[from - base..to - base];
        let differ = src[from..to].iter().zip(&dst[from..to]).filter(|(a, b)| a != b).count();
        let first = src[from..to]
            .iter()
            .zip(&dst[from..to])
            .position(|(a, b)| a != b)
            .unwrap_or(0);
        let src_moved = &src[from..to] != snap;
        let dst_kept = &dst[from..to] == snap;
        let at = arena_offset
            .map(|o| {
                let byte = o + (from + first) as u64;
                format!(", arena byte {byte} = page {} +{}", byte / 4096, byte % 4096)
            })
            .unwrap_or_default();
        out.push_str(&format!(
            "; row {row}: {differ} bytes differ from byte {first}, src moved {}, dst kept {}{at}",
            if src_moved { "YES" } else { "no" },
            if dst_kept { "yes" } else { "NO" },
        ));
        shown += 1;
        if shown == 3 {
            break;
        }
    }
    out
}

/// Copy the whole of one buffer's extent within the pool.
fn copy_extent(src: &[u8], dst: &mut [u8], info: &BufferInfo, usable: usize) -> usize {
    let Some(end) = info.end() else {
        return 0;
    };
    let Ok(start) = usize::try_from(info.offset) else {
        return 0;
    };
    let end = end.min(usable);
    if end <= start {
        return 0;
    }
    dst[start..end].copy_from_slice(&src[start..end]);
    end - start
}

/// How many pixels `copy_damage` would move for `damage`, overlaps counted as
/// many times as they would be copied — which is the point, because that is the
/// number being compared against the extent.
fn damage_area(damage: &[Rect], info: &BufferInfo) -> i64 {
    damage
        .iter()
        .filter_map(|r| r.clip(info.width, info.height))
        .map(|r| i64::from(r.w) * i64::from(r.h))
        .sum()
}

/// The pixels in one buffer's extent.
fn extent_area(info: &BufferInfo) -> i64 {
    i64::from(info.width.max(0)) * i64::from(info.height.max(0))
}

/// Copy only the damaged rows of one buffer.
///
/// Row-wise, not rectangle-wise: a rectangle's rows are `stride` apart and only
/// `w * 4` bytes of each are wanted, so this is one `copy_from_slice` per row
/// per rectangle. Overlapping rectangles copy twice, which is cheaper than
/// computing their union and is what `wc-compositor`'s blitter does with the
/// same reasoning.
fn copy_damage(
    src: &[u8],
    dst: &mut [u8],
    info: &BufferInfo,
    damage: &[Rect],
    usable: usize,
) -> usize {
    let (Ok(base), Ok(stride)) = (usize::try_from(info.offset), usize::try_from(info.stride))
    else {
        return 0;
    };
    let mut copied = 0usize;
    for r in damage {
        let Some(r) = r.clip(info.width, info.height) else {
            continue;
        };
        // **A full-width rectangle is one copy, not one per row**, and that was
        // measured rather than assumed. `weston-presentation-shm` damages its
        // whole 250x250 buffer every frame, so the row loop below did 250
        // separate 1000-byte copies where the buffer scope did one of 244 KiB —
        // and came out *twice as slow* for identical bytes (41 us against 20,
        // `scripts/wc-proxy.sh cost`). Damage-scoped copying is supposed to be
        // the cheap one; it only is if it does not shred a large contiguous run.
        //
        // The padding between rows is copied too when the stride exceeds the
        // width. That is harmless — the bytes are inside the buffer either way —
        // and it is what makes the run contiguous.
        if r.x == 0 && r.w >= info.width {
            let from = base + r.y as usize * stride;
            let to = (from + r.h as usize * stride).min(usable).min(src.len()).min(dst.len());
            if to > from {
                dst[from..to].copy_from_slice(&src[from..to]);
                copied += to - from;
            }
            continue;
        }
        let x0 = r.x as usize * 4;
        let bytes = (r.w as usize * 4).min(info.width as usize * 4 - x0);
        for row in 0..r.h as usize {
            let y = r.y as usize + row;
            let from = base + y * stride + x0;
            let to = from + bytes;
            if to > usable || to > src.len() || to > dst.len() {
                continue;
            }
            dst[from..to].copy_from_slice(&src[from..to]);
            copied += bytes;
        }
    }
    copied
}

/// Turn a descriptor that arrived from the compositor into a real local file
/// descriptor.
///
/// **This opens a name; it does not receive a handle.** The compositor sent a
/// path or an arena window; this side opens or slices it and produces an fd out
/// of memory it already reaches. Nothing was passed.
///
/// STAGE 10 gave it a second argument. An arena slice is not reachable by name
/// from anywhere — that is what makes it the kind a compositor can create
/// across a hypervisor with no shared filesystem — so the only way to read one
/// is through the mapping this process already holds, which belongs to the
/// transport (`crate::pool::PoolTransport::peer_slice`).
fn materialise(
    d: &Descriptor,
    transport: &dyn crate::pool::PoolTransport,
) -> io::Result<std::os::fd::OwnedFd> {
    use std::os::fd::{AsRawFd as _, OwnedFd};
    match d {
        Descriptor::SharedFile { path, .. } => {
            let f = std::fs::File::open(path)?;
            Ok(OwnedFd::from(f))
        }
        // `Section` is a *filesystem path* when the peer is a Unix process and
        // an *NT object name* when it is a Windows one, and only this side can
        // tell which it is looking at. A `Local\wc-…` opened as a path fails
        // with "No such file or directory", which is true and unhelpful; the
        // real answer is that the compositor was never told where to put
        // objects a client has to open.
        //
        // Detected by the prefix rather than by a flag, because the prefix is
        // what `wc_handle::platform::random_section_name` puts there on Windows
        // and it is the only thing in the descriptor that distinguishes the two
        // cases. See docs/waycompose-design.md §11.2.
        Descriptor::Section { name, .. } if name.starts_with("Local\\") || name.starts_with("Global\\") => {
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                format!(
                    "{name} is a Windows kernel object name, which a Linux process cannot open. \
                     Start wc-compositor.exe with --shared-dir and --shared-peer-prefix so that \
                     objects it creates for clients land somewhere both sides can reach."
                ),
            ))
        }
        Descriptor::Section { name, .. } => {
            let f = std::fs::File::open(name)?;
            Ok(OwnedFd::from(f))
        }
        Descriptor::ArenaSlice { arena, offset, len } => {
            // A window into the arena has no fd of its own, so one is made:
            // a fresh anonymous object with the bytes copied into it. A copy
            // rather than a mapping because the client will be given this fd
            // and must not be able to write into the arena.
            //
            // **This is a keymap, or a clipboard offer, or an icon — not a
            // frame.** It runs once per object the compositor creates, which is
            // a handful per client, and the copy is what makes the client's fd
            // a genuinely separate object it may do as it likes with. A frame
            // goes the other way and never comes near this function.
            let bytes = transport.peer_slice(*arena, *offset, *len).ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::NotFound,
                    format!(
                        "the compositor described an object as arena {arena} \
                         {offset}+{len}, which is not a range this proxy will read. Either \
                         it was started with --transport shared-file while the compositor \
                         has an --arena-outbound window, or the two disagree about the \
                         arena's number or its length, or — STAGE 12 — the range is outside \
                         the compositor's outbound window, which is the only part of a \
                         shared arena a compositor may describe to a client: everything \
                         else in it is some connection's pixels, being written now."
                    ),
                )
            })?;
            // SAFETY: a NUL-terminated literal name and a valid flag.
            let fd = unsafe {
                libc::memfd_create(c"wc-proxy-event".as_ptr(), libc::MFD_CLOEXEC)
            };
            if fd < 0 {
                return Err(io::Error::last_os_error());
            }
            // SAFETY: `fd` was just returned by `memfd_create` and is owned here.
            let owned = unsafe { <OwnedFd as std::os::fd::FromRawFd>::from_raw_fd(fd) };
            // `pwrite` rather than `write`, and it is not a style choice. A
            // sequential write leaves the descriptor's file offset at the end,
            // and the client is handed *that* descriptor: libwayland's keymap
            // path `mmap`s and does not care, but anything that reads — a
            // clipboard transfer, which is the next customer of this
            // direction — would get zero bytes and no error. Writing at
            // explicit offsets leaves the offset where a fresh descriptor's
            // should be, which is 0.
            let mut written = 0usize;
            while written < bytes.len() {
                // SAFETY: writing `bytes` through a live fd this function owns,
                // at an offset inside the range being written.
                let n = unsafe {
                    libc::pwrite(
                        owned.as_raw_fd(),
                        bytes[written..].as_ptr().cast(),
                        bytes.len() - written,
                        written as libc::off_t,
                    )
                };
                if n <= 0 {
                    return Err(io::Error::last_os_error());
                }
                written += n as usize;
            }
            Ok(owned)
        }
        // §81. A named stream becomes a real descriptor by being opened, which
        // is the whole of the Named -> Open direction and needs no relay: the
        // client is handed one end of the same FIFO the compositor created, and
        // the bytes flow between them without this process touching one.
        //
        // The **other** direction still does not work, and it is not this arm's
        // shape. Turning a client's anonymous pipe into a name means creating a
        // FIFO and copying between the two, because nothing can give an existing
        // pipe a name after the fact. That is a byte relay and a policy decision
        // about who runs it; §81.5 says where it belongs.
        Descriptor::Pipe { .. } => {
            let file = wc_handle::Handle::from_descriptor(d.clone()).open_pipe()?;
            Ok(OwnedFd::from(file))
        }
        other => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{other:?} cannot become a file descriptor on this side"),
        )),
    }
}

mod dmabuf;
mod native_shm;
pub use dmabuf::{
    dev_major, dev_minor, render_node, DRM_FORMAT_ARGB8888, DRM_FORMAT_XRGB8888,
    GLOBAL_NAME as DMABUF_GLOBAL_NAME,
};

#[cfg(test)]
mod tests;

/// §109. The stub extent every pool is created on and every unbacked buffer
/// is cut from: one page.
const STUB_LEN: u32 = 4096;
/// `wl_shm.error.invalid_stride`.
const SHM_ERROR_INVALID_STRIDE: u32 = 1;
/// `wl_display.error.no_memory`.
const DISPLAY_ERROR_NO_MEMORY: u32 = 2;

/// `wl_shm.create_pool(new_id, fd, size)`, the fd in band as the descriptor.
fn create_pool_message(shm: u32, pool: u32, descriptor: &Descriptor, len: u32) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(&pool.to_ne_bytes());
    wire::push_bytes(&mut body, &descriptor.encode());
    body.extend_from_slice(&len.to_ne_bytes());
    wire::message(shm, op::SHM_CREATE_POOL, &body)
}

/// `wl_shm_pool.create_buffer(new_id, offset, width, height, stride, format)`.
fn create_buffer_message(pool: u32, buffer: u32, offset: i32, width: i32, height: i32, stride: i32, format: u32) -> Vec<u8> {
    let mut body = Vec::new();
    for w in [buffer, offset as u32, width as u32, height as u32, stride as u32, format] {
        body.extend_from_slice(&w.to_ne_bytes());
    }
    wire::message(pool, op::POOL_CREATE_BUFFER, &body)
}
