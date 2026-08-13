//! One client connection's worth of translation.
//!
//! Deliberately free of sockets. Everything here is "bytes and descriptors in,
//! bytes and descriptors out", which is what lets the whole translation —
//! including the pool path that is the point of the stage — be driven by tests
//! rather than only by a running compositor. `wc-shmclient` was split the same
//! way in stage 3 and for the same reason (docs/design.md §8.9): the encoding
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
use std::time::Instant;

use wc_handle::Descriptor;

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
        // in docs/design.md §9 as an unclosed gap rather than hidden here.
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

struct Pool {
    client: ClientMap,
    /// `None` until the pool is *materialised*. See [`Session::flush_deferred`]
    /// for what that means and why it is worth doing.
    region: Option<Region>,
    /// The `wl_shm` this pool was created from, so that the deferred
    /// `create_pool` can be addressed to the right object.
    shm: u32,
    /// What the client says the pool is. May be less than the region, which is
    /// over-allocated by `headroom` so that a later `resize` has somewhere to
    /// go.
    size: usize,
    /// The client has destroyed the `wl_shm_pool` object.
    zombie: bool,
    /// The compositor has sent `wl_display.delete_id` for the pool, so it has
    /// certainly finished opening the name.
    acked: bool,
    /// How many `wl_buffer`s cut from this pool the client still has.
    buffers: u32,
    /// STAGE 16. **This proxy** has sent `wl_shm_pool.destroy` upstream on the
    /// client's behalf and is waiting for the compositor's `delete_id` before
    /// it dares hand the bytes to anybody else. See [`Session::reclaim_for`].
    ///
    /// The wait is docs/design.md §2.5's rule and not caution: the compositor
    /// may be mid-render out of the region, and the acknowledgement is the only
    /// moment at which it provably is not.
    teardown: bool,
    /// STAGE 16. When a commit last copied pixels out of this pool. The
    /// reclaimer takes the coldest first, and refuses anything younger than
    /// [`RECLAIM_FLOOR`].
    last_used: Instant,
    /// STAGE 16. Whether the "could not publish" note has already been made.
    /// Without it a wedged pool writes one line per message, thousands a
    /// second, and buries the line that matters.
    warned: bool,
    /// STAGE 16. Whether anything actually wants this pool to exist upstream.
    ///
    /// **Without this the reclaimer does nothing at all**, and it took writing
    /// the test to see it: [`Session::flush_deferred`] publishes every pool
    /// that has no region, so a pool whose bytes had just been reclaimed was
    /// rebuilt by the very next message the client sent, whatever that message
    /// was about. The memory came back and left again before anyone could use
    /// it.
    ///
    /// So publication is demand-driven. It is set when the pool is created —
    /// a pool the client has just handed over is wanted by definition — and
    /// whenever a request names the pool or one of its buffers
    /// ([`Session::note_demand`]). It is cleared when the reclaimer takes the
    /// pool apart. A cold pool therefore stays cold until the client comes
    /// back for it, which is the whole point of reclaiming it.
    wanted: bool,
    /// STAGE 16. The compositor has had a `wl_shm_pool` for this id at some
    /// point. Distinguishes a pool being *rebuilt* from one being announced for
    /// the first time, which is the whole difference between the `republished`
    /// and `created` counters — and those two numbers are how a reader of the
    /// log tells a healthy reclaim cycle from a thrashing one.
    ever_published: bool,
}

impl Pool {
    /// Whether the region may be released.
    ///
    /// All three conditions, and each was learned the hard way against a real
    /// client — see the note on `POOL_DESTROY` in `client_message`:
    ///
    /// * `zombie`: the client has finished with the pool object.
    /// * `buffers == 0`: the `wl_shm` spec is explicit that "the mmapped memory
    ///   will be released when all buffers that have been created from this
    ///   pool are gone", and `weston-presentation-shm` destroys its pool
    ///   immediately and then draws 784 frames from buffers cut out of it.
    /// * `acked`: the compositor has confirmed it processed the destructor, so
    ///   it cannot still be about to open the name.
    fn releasable(&self) -> bool {
        self.zombie && self.acked && self.buffers == 0
    }

    /// Whether the compositor has a `wl_shm_pool` with this id **right now**.
    ///
    /// STAGE 16, and it is the single question the whole of this file's pool
    /// handling now turns on. Before this stage `region.is_some()` answered it,
    /// because a region was published once and never taken away. It is now
    /// taken away — by [`Session::reclaim_for`], which destroys the upstream
    /// object to get the guest's memory back — so "we have bytes for it" and
    /// "the compositor knows about it" have come apart, and every message that
    /// names a pool has to ask the second question rather than the first.
    ///
    /// Getting this wrong is exactly the bug this stage exists to fix: a
    /// message forwarded to a compositor that has no such object is a protocol
    /// error, and a protocol error is a dead client.
    fn upstream(&self) -> bool {
        self.region.is_some() && !self.teardown
    }
}

#[derive(Clone, Debug)]
struct BufferInfo {
    pool: u32,
    offset: i32,
    width: i32,
    height: i32,
    stride: i32,
    /// STAGE 16. The client's own `wl_shm_pool.create_buffer` body, kept
    /// verbatim so the request can be made again.
    ///
    /// **Verbatim rather than rebuilt from the fields above**, and that is the
    /// point: re-encoding would mean this file owning a second opinion about
    /// `create_buffer`'s argument list, which is somebody else's XML and has a
    /// `format` argument nothing here has ever needed to understand. The body
    /// carries no descriptor and no string, so keeping it is thirty bytes and
    /// no lifetime question.
    create_body: Vec<u8>,
    /// STAGE 16. The compositor has a `wl_buffer` with this id right now.
    /// False for one absorbed while its pool had no region, and for one this
    /// proxy destroyed to reclaim the pool.
    upstream: bool,
    /// STAGE 16. Committed and not yet released, so the compositor may still be
    /// reading it. `wl_buffer.release` is the signal and it is the compositor's
    /// own word for "I am finished with these bytes".
    held: bool,
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

#[derive(Default)]
struct Surface {
    pending_buffer: Option<u32>,
    pending_attached: bool,
    current: Option<u32>,
    damage: Vec<Rect>,
    /// STAGE 16. A `wl_surface.attach` that could not be forwarded, because the
    /// buffer it names has no `wl_buffer` upstream and could not be given one.
    ///
    /// Held rather than dropped, because a client is entitled to attach once
    /// and then commit forever — a video does not, but a static page does, and
    /// dropping the attach outright would freeze such a surface permanently
    /// rather than until there was room. The next commit retries it.
    dropped_attach: Option<Vec<u8>>,
    /// STAGE 16. The next copy must be the buffer's whole extent rather than
    /// the damage the client declared.
    ///
    /// Set whenever an attach is dropped or a pool is republished, and it is
    /// required for correctness rather than being belt and braces: the client
    /// declares damage against what it believes the compositor already has, and
    /// after a gap in which nothing was copied that belief is wrong. A
    /// damage-scoped copy would leave the rows nobody damaged holding whatever
    /// the previous tenant of those arena bytes wrote.
    needs_full_copy: bool,
    /// Per-buffer arrears, and the fix for docs/design.md §27.
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
    /// Commits that actually moved pixels. A commit with no buffer attached is
    /// legal and common.
    pub copies: u64,
    pub bytes_copied: u64,
    pub copy_nanos: u64,
    pub pools_created: u64,
    /// Pools whose region was released after the compositor acknowledged the
    /// destructor. Should equal `pools_created` at the end of a clean run.
    pub pools_released: u64,
    /// STAGE 16. Pools whose region this proxy took back **without the client
    /// asking**, because the arena had no room and the pool was cold. See
    /// [`Session::reclaim_for`].
    ///
    /// A number worth having on its own rather than folded into
    /// `pools_released`, because the two answer different questions: released
    /// is what the client returned, reclaimed is what it did not, and
    /// docs/design.md §17.8 measured that the second is where nearly all the
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
    pub pool_resizes: u64,
    /// Resizes that could not be satisfied because the region had no headroom
    /// left. Should be zero; if it is not, the number is the argument for a
    /// larger `--pool-headroom`.
    pub resize_overflows: u64,
    pub fds_received: u64,
    pub fds_materialised: u64,
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
        // One grep-able line, with the number a harness can assert on spelled
        // out even when it is zero — a check that only prints when it fails is
        // indistinguishable from a check that never ran.
        let verify = if self.verified > 0 || self.verify_skipped > 0 {
            format!(
                "; verified {} commits ({} state-only, unverifiable), {} stale (worst {} rows)",
                self.verified, self.verify_skipped, self.stale_commits, self.stale_rows_worst
            )
        } else {
            String::new()
        };
        format!(
            "{} commits, {} copies, {mb:.2} MiB in {ms:.1} ms ({per}); \
             {} pools ({} released, {} reclaimed, {} republished, {} resizes, {} overflows, \
             {} refusals, {} attaches deferred); {} fds in, {} out{}; \
             {} client msgs, {} server msgs{verify}",
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
        )
    }
}

/// What a message could not be translated for.
#[derive(Debug)]
pub enum Error {
    /// The object's interface is unknown, so the message cannot be walked.
    /// **Fatal**, and deliberately so: forwarding a message whose shape is
    /// unknown is how a stream desynchronises silently, which is the one
    /// failure docs/design.md §2.4 says must never be allowed to happen
    /// quietly.
    UnknownObject { id: u32, opcode: u16 },
    /// The opcode is outside the interface's table.
    UnknownOpcode { interface: String, opcode: u16 },
    Walk(String),
    /// A message needed a descriptor and the queue was empty.
    MissingFd { interface: String, message: String },
    /// The message carries an `h` this proxy has no transport for. Clipboard
    /// and drag-and-drop pipes, which are stage 5.
    UnsupportedFd { interface: String, message: String },
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
            Error::UnsupportedFd { interface, message } => write!(
                f,
                "{interface}.{message} carries a descriptor this proxy cannot yet republish \
                 (clipboard and drag-and-drop pipes are stage 5)"
            ),
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
}

pub struct Session {
    spec: Spec,
    objects: Objects,
    transport: Box<dyn PoolTransport>,
    scope: CopyScope,
    headroom: u64,
    /// Every pool, including the ones the client has destroyed. A destroyed
    /// pool stays here, with `zombie` set, until the compositor acknowledges
    /// the destructor — see the note on `POOL_DESTROY` in `client_message`, and
    /// note that its buffers keep working meanwhile, which the `wl_shm` spec
    /// requires and which `weston-presentation-shm` depends on.
    pools: HashMap<u32, Pool>,
    buffers: HashMap<u32, BufferInfo>,
    surfaces: HashMap<u32, Surface>,
    pub stats: Stats,
    /// Log every message as it is walked. Off by default because it is one line
    /// per message and a running client produces thousands, but it is the only
    /// way to answer "what does this client actually *do*" — a question this
    /// stage had to answer about `wl_shm_pool.resize` before it could design
    /// for it, and got a different answer to the one it assumed.
    trace: bool,
    /// Lines worth telling the operator about, drained by the caller so this
    /// stays free of I/O.
    notes: Vec<String>,
    /// Every `xdg_toplevel.set_app_id` seen, in order, drained by the caller.
    ///
    /// The same shape as `notes` and for the same reason: this module is
    /// bytes-in / bytes-out and has no business opening an icon theme or
    /// writing to a second socket. What it can do is notice.
    app_ids: Vec<String>,
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
    /// STAGE 16. Ids **this proxy** destroyed upstream without the client
    /// asking. Their `delete_id` is the compositor answering us, not the
    /// client, and forwarding it would tell libwayland-client to recycle an id
    /// its own application still holds — which is the same desynchronisation
    /// this file's header is about, arriving from the one direction nothing had
    /// ever produced it from.
    proxy_destroyed: std::collections::HashSet<u32>,
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
    /// docs/design.md §13.8's retraction is the reason to write that down: an
    /// instrument inside the loop being timed produced a 2x that was really
    /// 1.03x. Nothing here may be used to time anything.
    verify: bool,
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

impl Session {
    pub fn new(
        spec: Spec,
        transport: Box<dyn PoolTransport>,
        scope: CopyScope,
        headroom: u64,
    ) -> Session {
        Session {
            spec,
            objects: Objects::new(),
            transport,
            scope,
            headroom: headroom.max(1),
            pools: HashMap::new(),

            buffers: HashMap::new(),
            surfaces: HashMap::new(),
            stats: Stats::default(),
            trace: false,
            notes: Vec::new(),
            app_ids: Vec::new(),
            to_client: Vec::new(),
            proxy_destroyed: std::collections::HashSet::new(),
            pressure_noted: false,
            verify: false,
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

    pub fn transport_name(&self) -> &'static str {
        self.transport.name()
    }

    pub fn take_notes(&mut self) -> Vec<String> {
        std::mem::take(&mut self.notes)
    }

    fn note(&mut self, s: String) {
        self.notes.push(s);
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

        // Materialise any pool still being sized, unless *this* message is one
        // of the two that a pool is allowed to be deferred through. Doing it
        // here — before the message is handled and before anything is emitted —
        // is what keeps the upstream ordering identical to the client's, except
        // for the resizes that were absorbed.
        let deferrable = matches!(
            (interface.as_str(), header.opcode),
            ("wl_shm", op::SHM_CREATE_POOL)
        ) || (interface == "wl_shm_pool"
            && header.opcode == op::POOL_RESIZE
            && self.pools.get(&header.object).is_some_and(|p| !p.upstream()));
        // STAGE 16. Which pools this message needs, decided before any of them
        // is published, so that `flush_deferred` publishes those and not every
        // pool that happens to be without a region. See [`Pool::wanted`].
        self.note_demand(&interface, header.opcode, header.object, body, &spans);
        let mut out = if deferrable { Vec::new() } else { self.flush_deferred() };

        // STAGE 11. The one message this program *watches* rather than
        // understands: it is forwarded unchanged, and a copy of the string goes
        // to the icon resolver, which is on this side because the icon theme is
        // (`crate::icons`). Matched on the message's **name** rather than on an
        // opcode constant, because the opcode is a position in somebody else's
        // XML and this is the only place that would notice if it moved.
        if interface == "xdg_toplevel" && msg.name == "set_app_id"
            && let Some(app_id) = spans.first().and_then(|s| wire::string_at(body, s)) {
                self.app_ids.push(app_id);
            }

        // The semantic half: everything the proxy has to understand rather than
        // merely carry.
        let descriptors: Vec<Vec<u8>> = Vec::new();
        match (interface.as_str(), header.opcode) {
            ("wl_shm", op::SHM_CREATE_POOL) => {
                let pool_id = wire::word_at(body, &spans[0]).unwrap_or(0);
                let size = wire::word_at(body, &spans[2]).unwrap_or(0) as i32;
                let fd = fds.pop_front().ok_or_else(|| Error::MissingFd {
                    interface: interface.clone(),
                    message: msg.name.clone(),
                })?;
                self.stats.fds_received += 1;
                self.create_pool(pool_id, header.object, fd, size)?;
                // Emitted later, by `flush_deferred`, at whatever size the
                // client has settled on. See that function.
                return Ok(out);
            }
            ("wl_shm_pool", op::POOL_CREATE_BUFFER) => {
                // STAGE 16. `upstream` is decided here and it decides whether
                // the message is forwarded at all: a `create_buffer` on a pool
                // the compositor does not have is the exact message that killed
                // Firefox. `flush_deferred` ran above, so this is the answer
                // after every attempt to give the pool a region has been made.
                let live = self.pools.get(&header.object).is_some_and(Pool::upstream);
                let b = BufferInfo {
                    pool: header.object,
                    offset: wire::word_at(body, &spans[1]).unwrap_or(0) as i32,
                    width: wire::word_at(body, &spans[2]).unwrap_or(0) as i32,
                    height: wire::word_at(body, &spans[3]).unwrap_or(0) as i32,
                    stride: wire::word_at(body, &spans[4]).unwrap_or(0) as i32,
                    create_body: body.to_vec(),
                    upstream: live,
                    held: false,
                };
                let id = wire::word_at(body, &spans[0]).unwrap_or(0);
                self.buffers.insert(id, b);
                self.forget_buffer_arrears(id);
                if let Some(pool) = self.pools.get_mut(&header.object) {
                    pool.buffers += 1;
                }
                if !live {
                    // Absorbed, exactly as a resize of an unannounced pool is.
                    // The request is not lost — it is kept verbatim in
                    // `create_body` and replayed by `flush_deferred` the moment
                    // the pool has bytes again.
                    return Ok(out);
                }
            }
            ("wl_shm_pool", op::POOL_RESIZE) => {
                let size = wire::word_at(body, &spans[0]).unwrap_or(0) as i32;
                if self.resize_pool(header.object, size) {
                    // Absorbed: the pool has not been announced yet, so there
                    // is nothing upstream to resize.
                    return Ok(out);
                }
            }
            // **Do not free the region here**, and this is docs/design.md
            // §2.5's lifetime rule arriving from the direction it warns about:
            //
            // > "Sending a descriptor transfers nothing… a sender that drops
            // > its handle immediately after `send` — which is legal on Unix —
            // > leaves the receiver with a name for an object that no longer
            // > exists."
            //
            // `weston-presentation-shm` does exactly that. It creates a pool,
            // creates its buffers, and destroys the `wl_shm_pool` at once,
            // which is not only legal but idiomatic: the `wl_shm` spec says the
            // memory lives until the buffers are gone. All three requests
            // reached this proxy before the compositor had read any of them, so
            // freeing on `destroy` unlinked the file *first* and the compositor
            // opened a path with nothing behind it — `File not found`, reported
            // to the client as a `wl_shm` protocol error, 0 frames presented.
            //
            // So the pool becomes a zombie and is freed when the compositor
            // says it has processed the destructor, which it does with
            // `wl_display.delete_id`. That is a real acknowledgement rather than
            // a delay, and it is the receiver-side eagerness §2.5 asks for,
            // implemented on the side that can actually observe it.
            ("wl_shm_pool", op::POOL_DESTROY) => {
                let live = self.pools.get(&header.object).is_some_and(Pool::upstream);
                if let Some(pool) = self.pools.get_mut(&header.object) {
                    pool.zombie = true;
                }
                if !live && self.pools.contains_key(&header.object) {
                    // STAGE 16. The compositor has no such object — either this
                    // proxy destroyed it to reclaim the bytes, or it was never
                    // announced — so the destructor must not be forwarded. The
                    // client is owed the acknowledgement libwayland waits for
                    // before it recycles the id, and only this side can send it.
                    self.dispose_absorbed_pool(header.object);
                    return Ok(out);
                }
                self.release_pool_if_done(header.object);
            }
            ("wl_buffer", op::BUFFER_DESTROY) => {
                if let Some(b) = self.buffers.remove(&header.object) {
                    if let Some(pool) = self.pools.get_mut(&b.pool) {
                        pool.buffers = pool.buffers.saturating_sub(1);
                    }
                    if !b.upstream {
                        // Same absorption, same reason. Note the ordering: the
                        // buffer is out of `self.buffers` before this, so a
                        // later republish of its pool will not resurrect a
                        // buffer the client has finished with.
                        self.fabricate_delete_id(header.object);
                        self.dispose_absorbed_pool(b.pool);
                        self.release_pool_if_done(b.pool);
                        return Ok(out);
                    }
                    self.release_pool_if_done(b.pool);
                }
                self.forget_buffer_arrears(header.object);
            }
            ("wl_surface", op::SURFACE_DESTROY) => {
                self.surfaces.remove(&header.object);
            }
            ("wl_surface", op::SURFACE_ATTACH) => {
                let buffer = wire::word_at(body, &spans[0]).unwrap_or(0);
                let s = self.surfaces.entry(header.object).or_default();
                s.pending_attached = true;
                s.pending_buffer = (buffer != 0).then_some(buffer);
                // STAGE 16. An attach naming a buffer the compositor does not
                // have is the second message that could desynchronise the
                // stream, and unlike `create_buffer` it cannot simply be
                // absorbed and replayed later from a table — the client may
                // never send another one. So it is held on the surface and
                // retried at every commit.
                if buffer != 0 && !self.buffer_is_upstream(buffer) {
                    let s = self.surfaces.entry(header.object).or_default();
                    s.dropped_attach = Some(body.to_vec());
                    s.needs_full_copy = true;
                    self.stats.attaches_deferred += 1;
                    return Ok(out);
                }
                self.surfaces.entry(header.object).or_default().dropped_attach = None;
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
                // STAGE 16. The retry point. `flush_deferred` ran at the top of
                // this call, so if the pool found room the held attach can go
                // now — ahead of the commit, which is where the client put it.
                if let Some(held) = self
                    .surfaces
                    .get(&header.object)
                    .and_then(|s| s.dropped_attach.clone())
                {
                    let buffer = held
                        .get(..4)
                        .and_then(|b| b.try_into().ok())
                        .map_or(0, u32::from_ne_bytes);
                    if self.buffer_is_upstream(buffer) {
                        out.extend_from_slice(&wire::message(
                            header.object,
                            op::SURFACE_ATTACH,
                            &held,
                        ));
                        if let Some(s) = self.surfaces.get_mut(&header.object) {
                            s.dropped_attach = None;
                        }
                    }
                }
                self.commit(header.object);
            }
            _ => {}
        }

        let out_body = if msg.has_fd() {
            // Every `h` must have produced a descriptor above. Splicing a
            // zero-length one instead would be the §2.4 failure in its purest
            // form: a well-formed message the compositor parses happily and
            // then cannot map, or worse, a length the parser walks past.
            //
            // Reachable today only for `wl_data_offer.receive`, which is
            // clipboard transfer and out of scope for this stage. It is an
            // error with the message's name in it rather than a silent
            // truncation, so the first client that tries gets a diagnosis
            // rather than a mystery.
            if descriptors.len() != msg.fd_count() {
                return Err(Error::UnsupportedFd {
                    interface: interface.clone(),
                    message: msg.name.clone(),
                });
            }
            wire::splice_fds(body, &spans, FdEncoding::OutOfBand, &descriptors)
        } else {
            body.to_vec()
        };
        out.extend_from_slice(&wire::message(header.object, header.opcode, &out_body));
        Ok(out)
    }

    /// Announce every pool that has finished being sized, and return the
    /// `wl_shm.create_pool` messages that do it.
    ///
    /// # Why `create_pool` is delayed at all
    ///
    /// Because of a measurement, and it is worth recording what was measured
    /// because the obvious design is wrong.
    ///
    /// A shared region cannot grow once the peer has mapped it. A Windows
    /// section's size is fixed at `CreateFileMappingW` time (docs/design.md
    /// §2.5) and an arena slice's neighbours are already allocated, so the only
    /// ways to honour `wl_shm_pool.resize` are to over-allocate up front, or to
    /// re-key the pool onto a new object — which the protocol gives no way to
    /// express, because the pool's id belongs to the client.
    ///
    /// Over-allocation needs a factor, and the factor turned out to be
    /// unguessable. **`weston-terminal` creates a 4096-byte pool and then grows
    /// it nine times, by doubling, to 4190208 bytes — a factor of 1023.** No
    /// headroom anyone would write down covers that.
    ///
    /// What the same trace showed is the way out: all nine resizes arrive
    /// **back to back, before any other request**, because toytoolkit sizes its
    /// pool during `display_create()` and only then makes a surface. So the
    /// proxy holds `create_pool` until the client does something that is not a
    /// resize, and announces the pool once, at the size the client settled on.
    /// Zero over-allocation and zero failed resizes for the measured client,
    /// and `--pool-headroom` remains for a client that resizes *later*, which
    /// this one does not but a window that is dragged bigger will.
    ///
    /// This is a delay, not a reordering: nothing else is emitted while a pool
    /// is deferred, because any other message flushes first.
    /// # STAGE 16: what happens when there is no room, which used to be death
    ///
    /// The old version of this function noted the failure and moved on, and
    /// docs/design.md §17.8 described the consequence as "the client's window
    /// stops updating". **That was wrong, and §20.2 is the correction.** An
    /// unannounced pool is an object the compositor does not have; the very
    /// next `wl_shm_pool.create_buffer` was forwarded to it anyway, and a
    /// request on an unknown object is a protocol error. The compositor dropped
    /// the connection and Firefox exited with `channel error`. The allocation
    /// failure was survivable; forwarding past it was not.
    ///
    /// So there are now three answers in order, and only the third is visible
    /// to a user:
    ///
    /// 1. **Ask for the memory back.** [`Session::reclaim_for`] destroys the
    ///    upstream objects of pools nothing is using and returns their bytes.
    ///    This is where the guest's RAM comes from, and it is demand-driven —
    ///    nothing is taken while there is room.
    /// 2. **Try again.** Reclamation cannot complete synchronously (the
    ///    compositor has to acknowledge first), so the retry that matters is
    ///    the next message's, and there is always a next message.
    /// 3. **Leave the pool unannounced, and make sure nothing is ever said
    ///    about it.** Every request naming a pool with no upstream object is
    ///    absorbed by `client_message` rather than forwarded, so the stream
    ///    stays valid and the connection stays open. The surface freezes; the
    ///    client lives.
    fn flush_deferred(&mut self) -> Vec<u8> {
        let pending: Vec<u32> = self
            .pools
            .iter()
            .filter(|(_, p)| p.region.is_none() && !p.teardown && p.wanted && !p.zombie)
            .map(|(id, _)| *id)
            .collect();
        let mut out = Vec::new();
        for pool_id in pending {
            let (size, shm, republish) = {
                let p = &self.pools[&pool_id];
                // A pool the compositor has had before is one this proxy took
                // apart, not a new one. "Has buffers it does not know about"
                // was the first spelling of this and it was wrong: a pool
                // refused for want of room collects absorbed buffers too, and
                // counting those as rebuilds made a full arena look like a
                // thrashing reclaimer.
                (p.size, p.shm, p.ever_published)
            };
            let region_size = (size as u64).saturating_mul(self.headroom);
            let region = match self.transport.create(region_size) {
                Ok(r) => r,
                Err(first) => {
                    self.stats.alloc_refusals += 1;
                    out.extend_from_slice(&self.reclaim_for(region_size));
                    match self.transport.create(region_size) {
                        Ok(r) => r,
                        Err(e) => {
                            // Once per pool, not once per message: a wedged pool
                            // is touched thousands of times a second and the
                            // line that matters is the first one.
                            let p = self.pools.get_mut(&pool_id).expect("just read");
                            if !p.warned {
                                p.warned = true;
                                self.note(format!(
                                    "pool {pool_id}: could not publish {region_size} bytes: {e} \
                                     -- the pool is held back and every request naming it is \
                                     absorbed, so this surface stops updating until there is \
                                     room. The connection is not affected."
                                ));
                            }
                            let _ = first;
                            continue;
                        }
                    }
                }
            };
            let p = self.pools.get_mut(&pool_id).expect("just read");
            p.warned = false;
            p.last_used = Instant::now();
            p.ever_published = true;
            if republish {
                self.stats.pools_republished += 1;
            } else {
                self.stats.pools_created += 1;
            }
            self.note(format!(
                "pool {pool_id}: {size} bytes from the client -> {}{}",
                region.describe(),
                if republish { " (rebuilt)" } else { "" }
            ));
            let mut body = Vec::new();
            body.extend_from_slice(&pool_id.to_ne_bytes());
            wire::push_bytes(&mut body, &region.descriptor.encode());
            body.extend_from_slice(&(size as u32).to_ne_bytes());
            out.extend_from_slice(&wire::message(shm, op::SHM_CREATE_POOL, &body));
            self.pools.get_mut(&pool_id).expect("just read").region = Some(region);

            // And every buffer of it the compositor does not have. Two kinds
            // arrive here and they are deliberately not distinguished: one the
            // client created while there was no room, and one this proxy
            // destroyed to reclaim the pool. Both are "the client believes this
            // buffer exists and the compositor does not", and the cure is the
            // client's own request, replayed.
            let mut rebuilt: Vec<(u32, Vec<u8>)> = self
                .buffers
                .iter()
                .filter(|(_, b)| b.pool == pool_id && !b.upstream)
                .map(|(id, b)| (*id, b.create_body.clone()))
                .collect();
            // In id order, which is creation order for a libwayland client, so
            // the compositor sees them in the order it would have.
            rebuilt.sort_by_key(|(id, _)| *id);
            for (buffer_id, create_body) in rebuilt {
                out.extend_from_slice(&wire::message(
                    pool_id,
                    op::POOL_CREATE_BUFFER,
                    &create_body,
                ));
                if let Some(b) = self.buffers.get_mut(&buffer_id) {
                    b.upstream = true;
                }
                if self.trace {
                    self.note(format!("re-created buffer {buffer_id} in pool {pool_id}"));
                }
            }
        }
        out
    }

    /// Take back the guest's memory from pools nothing is using, until `needed`
    /// bytes have been promised.
    ///
    /// # STAGE 16, and it is the answer to docs/design.md §17.10 item 1
    ///
    /// §15.11 proposed releasing a pool when its last buffer dies. §17.8 then
    /// measured what that would be worth against the client it was designed
    /// for: Firefox destroys **one buffer in three and no pools at all**, so
    /// the proposal would free one pool of four in a 25-second run and none of
    /// the 88 in a startup. The plan was much weaker than it looked, and the
    /// reason is that it waits for a signal a real client does not send.
    ///
    /// **So this does not wait for a signal.** A `wl_shm` pool's bytes in the
    /// arena are a *copy*; the original is the client's own object, which this
    /// proxy still holds mapped (`Pool::client`). Nothing is lost by throwing
    /// the copy away, which means the compositor's `wl_shm_pool` is not a
    /// resource the client owns at all — it is a cache this proxy chose to
    /// populate, and a cache may be evicted. What the protocol lacks is a way
    /// to re-point an existing pool at new bytes, and that is worked around
    /// rather than defeated: destroy the objects, and rebuild them from the
    /// client's own requests the moment it touches the pool again
    /// ([`Session::flush_deferred`]).
    ///
    /// # The three conditions, each ruling out a specific wrong answer
    ///
    /// * **Not `zombie`.** The client must still own the pool's id. If it has
    ///   destroyed the object, the id may already have been recycled and a
    ///   rebuilt `create_pool` would collide with whatever now holds it. This
    ///   is also why `weston-presentation-shm` is untouched by any of this: it
    ///   destroys its pool immediately and draws 784 frames from the buffers,
    ///   so its pool is a zombie from the first frame and never a candidate.
    /// * **No buffer attached, and none `held`.** A buffer that is a surface's
    ///   current or pending contents is being displayed, and one the compositor
    ///   has not `release`d may be being read this instant. Destroying either
    ///   would blank a window or tear a frame. `wl_buffer.release` is the
    ///   compositor's own statement that it is finished.
    /// * **Colder than [`RECLAIM_FLOOR`].** Against thrash, not against error.
    ///
    /// # What it costs, stated plainly
    ///
    /// A rebuilt pool is one `create_pool` plus one `create_buffer` per live
    /// buffer, and a full-extent copy on the next commit instead of a damage
    /// copy. For the pools this targets — cold ones — that is paid once, if
    /// ever. The number to watch is `republished` in the stats line: if it
    /// approaches `reclaimed`, the floor is too low or the arena is too small,
    /// and both are visible without a debugger.
    fn reclaim_for(&mut self, needed: u64) -> Vec<u8> {
        // Buffers that must not lose their upstream object, by the two
        // conditions above. Collected once rather than asked per pool.
        let mut pinned: std::collections::HashSet<u32> = self
            .buffers
            .iter()
            .filter(|(_, b)| b.held)
            .map(|(id, _)| *id)
            .collect();
        for s in self.surfaces.values() {
            pinned.extend(s.current);
            pinned.extend(s.pending_buffer);
        }
        let pinned_pools: std::collections::HashSet<u32> = pinned
            .iter()
            .filter_map(|id| self.buffers.get(id))
            .map(|b| b.pool)
            .collect();

        let now = Instant::now();
        let mut cold: Vec<(std::time::Duration, u32, u64)> = self
            .pools
            .iter()
            .filter(|(id, p)| {
                p.upstream()
                    && !p.zombie
                    && !pinned_pools.contains(*id)
                    && now.duration_since(p.last_used) >= RECLAIM_FLOOR
            })
            .map(|(id, p)| {
                (
                    now.duration_since(p.last_used),
                    *id,
                    p.region.as_ref().map_or(0, |r| r.size),
                )
            })
            .collect();
        // Coldest first. The tie-break is the id, so that a session which
        // reclaims twice in one run does the same thing both times.
        cold.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));

        let mut out = Vec::new();
        let mut promised = 0u64;
        for (idle, pool_id, size) in cold {
            if promised >= needed {
                break;
            }
            // The buffers first, then the pool. The compositor processes a
            // stream in order, so its `delete_id` for the pool is proof it has
            // finished with every buffer named before it — which is what lets
            // one acknowledgement stand for the whole teardown.
            let mut ids: Vec<u32> = self
                .buffers
                .iter()
                .filter(|(_, b)| b.pool == pool_id && b.upstream)
                .map(|(id, _)| *id)
                .collect();
            ids.sort_unstable();
            for buffer_id in ids {
                out.extend_from_slice(&wire::message(buffer_id, op::BUFFER_DESTROY, &[]));
                self.buffers.get_mut(&buffer_id).expect("just listed").upstream = false;
                self.proxy_destroyed.insert(buffer_id);
            }
            out.extend_from_slice(&wire::message(pool_id, op::POOL_DESTROY, &[]));
            self.proxy_destroyed.insert(pool_id);
            let p = self.pools.get_mut(&pool_id).expect("just listed");
            p.teardown = true;
            // And it stays gone until the client asks for it again. See
            // `Pool::wanted`: without this the next message rebuilds it.
            p.wanted = false;
            promised += size;
            self.note(format!(
                "pool {pool_id}: reclaiming {size} bytes, idle {} ms -- the client never asked \
                 for them back and the arena needs {needed}",
                idle.as_millis()
            ));
        }
        // Said once, not once per message. A wedged pool asks on every message
        // the client sends, which for Firefox is thousands a second, and the
        // first run of this harness produced a log that was 90% this line —
        // burying the three lines that said what had actually happened. Same
        // reasoning as `Pool::warned`, and the same lesson as §17.8's: a
        // diagnostic nobody can read is not a diagnostic.
        if promised < needed && !self.pressure_noted {
            self.pressure_noted = true;
            self.note(format!(
                "arena pressure: {promised} of {needed} bytes could be reclaimed; the rest is \
                 in pools that are attached, unreleased or newer than {} ms. Further shortfalls \
                 are counted in `refusals` rather than logged.",
                RECLAIM_FLOOR.as_millis()
            ));
        }
        if promised > 0 {
            self.pressure_noted = false;
        }
        out
    }

    /// Translate one event from the compositor. Returns the bytes for the local
    /// client and any descriptors that must ride beside them as `SCM_RIGHTS`.
    pub fn server_message(
        &mut self,
        header: Header,
        body: &[u8],
    ) -> Result<(Vec<u8>, Vec<std::os::fd::OwnedFd>), Error> {
        self.stats.messages_server += 1;
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
            // STAGE 16. Whose destructor was this the answer to?
            //
            // If this proxy sent the destructor, the answer is addressed to
            // this proxy and **must not be forwarded**: the client still owns
            // that id, and telling libwayland-client to recycle an id its
            // application is still holding is a desynchronisation of exactly
            // the kind this file's header is about. It is also the moment the
            // arena bytes finally become safe to hand to somebody else, which
            // is why the whole teardown waits for it.
            if self.proxy_destroyed.remove(&id) {
                if let Some(pool) = self.pools.get_mut(&id)
                    && pool.teardown
                {
                    pool.teardown = false;
                    if let Some(region) = pool.region.take() {
                        self.transport.destroy(&region);
                        self.stats.pools_reclaimed += 1;
                    }
                    // The client may have destroyed it while the teardown was
                    // in flight, in which case there is nothing to rebuild and
                    // it is owed its acknowledgement.
                    self.dispose_absorbed_pool(id);
                }
                return Ok((Vec::new(), Vec::new()));
            }
            self.objects.forget(id);
            // The compositor has finished with this object. For a pool that
            // means it has certainly opened the region by now, so one of the
            // three conditions on releasing it is met. See the note on
            // `POOL_DESTROY`.
            if let Some(pool) = self.pools.get_mut(&id) {
                pool.acked = true;
            }
            self.release_pool_if_done(id);
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

        let out_body = if msg.has_fd() {
            wire::splice_fds(body, &spans, FdEncoding::InBand, &[])
        } else {
            body.to_vec()
        };
        Ok((wire::message(header.object, header.opcode, &out_body), fds))
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

    /// Whether the compositor has a `wl_buffer` with this id right now.
    ///
    /// STAGE 16. `false` for a buffer whose `create_buffer` was absorbed, and
    /// for one the reclaimer destroyed. Both mean the same thing to every
    /// caller: a message naming it must not be forwarded.
    fn buffer_is_upstream(&self, buffer: u32) -> bool {
        self.buffers.get(&buffer).is_some_and(|b| b.upstream)
    }

    /// Finish with a pool whose destructor was absorbed.
    ///
    /// STAGE 16. The client has destroyed a pool the compositor does not have,
    /// so nothing was forwarded and nothing will acknowledge it. This is where
    /// the acknowledgement is invented — but **only once the last buffer has
    /// gone**, because sending it is what frees the id in the client, and an id
    /// freed while buffers still refer to the pool could be reused for another
    /// object while this proxy still has the pool in its table.
    ///
    /// A no-op for a pool that is not in that state, so that the two call sites
    /// which cannot easily tell may both call it.
    fn dispose_absorbed_pool(&mut self, pool_id: u32) {
        let ready = self
            .pools
            .get(&pool_id)
            .is_some_and(|p| p.zombie && !p.upstream() && !p.teardown && p.buffers == 0);
        if ready {
            self.pools.remove(&pool_id);
            self.fabricate_delete_id(pool_id);
        }
    }

    /// Release a pool's region once every condition in [`Pool::releasable`] is
    /// met. Called from all three places that can make one true.
    fn release_pool_if_done(&mut self, pool_id: u32) {
        if !self.pools.get(&pool_id).is_some_and(Pool::releasable) {
            return;
        }
        if let Some(pool) = self.pools.remove(&pool_id)
            && let Some(region) = &pool.region
        {
            self.transport.destroy(region);
            self.stats.pools_released += 1;
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
        self.pools.insert(
            pool_id,
            Pool {
                client,
                region: None,
                shm,
                size,
                zombie: false,
                acked: false,
                buffers: 0,
                teardown: false,
                last_used: Instant::now(),
                warned: false,
                wanted: true,
                ever_published: false,
            },
        );
        Ok(())
    }

    /// Note that this message wants a pool to exist upstream, before anything
    /// is published. See [`Pool::wanted`].
    ///
    /// Peeking at the message before handling it, the same way `deferrable`
    /// does, is what lets `flush_deferred` run once at the top of
    /// `client_message` and still know which pools this particular message
    /// needs — rather than publishing every pool that happens to lack a region,
    /// which is the bug this exists to fix.
    fn note_demand(&mut self, interface: &str, opcode: u16, object: u32, body: &[u8], spans: &[Span]) {
        let pool = match interface {
            // **Only `create_buffer`**, and the exclusions are the point.
            //
            // `destroy` must never mark a pool wanted: republishing a pool in
            // order to forward its destructor is not merely wasteful, it is
            // wrong — the pool would be built and torn down in the same batch,
            // and it was doing exactly that until a test caught it.
            //
            // `resize` need not, because a resize of a pool with no upstream
            // object is absorbed and recorded in `Pool::size`, so a later
            // rebuild already happens at the size the client settled on.
            "wl_shm_pool" if opcode == op::POOL_CREATE_BUFFER => Some(object),
            // An attach names a buffer, and a buffer names a pool. A commit
            // carries no buffer of its own, so it asks after the attach that
            // was held back for want of one.
            "wl_surface" if opcode == op::SURFACE_ATTACH => spans
                .first()
                .and_then(|s| wire::word_at(body, s))
                .filter(|b| *b != 0)
                .and_then(|b| self.buffers.get(&b))
                .map(|b| b.pool),
            "wl_surface" if opcode == op::SURFACE_COMMIT => self
                .surfaces
                .get(&object)
                .and_then(|s| s.dropped_attach.as_ref())
                .and_then(|held| held.get(..4))
                .and_then(|b| b.try_into().ok())
                .map(u32::from_ne_bytes)
                .and_then(|b| self.buffers.get(&b))
                .map(|b| b.pool),
            _ => None,
        };
        if let Some(p) = pool.and_then(|p| self.pools.get_mut(&p)) {
            p.wanted = true;
        }
    }

    /// Returns true if the resize was **absorbed** — the pool has not been
    /// announced yet, so the request must not be forwarded.
    fn resize_pool(&mut self, pool_id: u32, size: i32) -> bool {
        self.stats.pool_resizes += 1;
        let Ok(size) = usize::try_from(size) else {
            return false;
        };
        let Some(pool) = self.pools.get_mut(&pool_id) else {
            return false;
        };
        // STAGE 16: `!upstream()` rather than `region.is_none()`. A resize
        // forwarded to a pool the compositor is in the middle of destroying is
        // a request on an object that may already be gone, which is the same
        // fatal shape as the `create_buffer` in §20.2.
        let deferred = !pool.upstream();
        if pool.upstream()
            && let Some(region) = &pool.region
            && size as u64 > region.size
        {
            self.stats.resize_overflows += 1;
            let have = region.size;
            self.note(format!(
                "pool {pool_id}: resize to {size} exceeds the {have}-byte region; \
                 raise --pool-headroom. The compositor will keep the old size."
            ));
            return false;
        }
        if let Err(e) = pool.client.remap(size) {
            self.note(format!("pool {pool_id}: could not remap the client's pool: {e}"));
            return deferred;
        }
        pool.size = size;
        deferred
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

    /// The frame path: apply the pending state and move the pixels.
    fn commit(&mut self, surface_id: u32) {
        self.stats.commits += 1;
        let Some(surface) = self.surfaces.get_mut(&surface_id) else {
            return;
        };
        let attached_now = surface.pending_attached;
        if surface.pending_attached {
            surface.current = surface.pending_buffer;
            surface.pending_attached = false;
        }
        // STAGE 16: taken, not drained, until it is known that a copy will
        // happen. A commit that copies nothing must leave the client's damage
        // where it is, or the rows it named are never copied by anybody.
        let full = surface.needs_full_copy;
        let Some(buffer_id) = surface.current else {
            surface.damage.clear();
            return;
        };
        let Some(info) = self.buffers.get(&buffer_id).cloned() else {
            return;
        };
        let Some(pool) = self.pools.get_mut(&info.pool) else {
            return;
        };

        let Some(region) = pool.region.clone().filter(|_| pool.upstream()) else {
            // A commit against a pool the compositor does not have: refused for
            // want of room, or mid-teardown. Already noted, and the surface
            // keeps whatever it last showed. The damage list is deliberately
            // **not** consumed on the way out — see `needs_full_copy`.
            return;
        };
        pool.last_used = Instant::now();
        // The copy is going to happen, so the damage really is consumed and the
        // "everything, once" flag really is discharged.
        let damage = std::mem::take(&mut self.surfaces.get_mut(&surface_id).expect("just read").damage);
        if full && let Some(s) = self.surfaces.get_mut(&surface_id) {
            s.needs_full_copy = false;
        }
        if let Some(b) = self.buffers.get_mut(&buffer_id) {
            // The compositor is about to be told to read these bytes, and until
            // it says `release` it may be doing so. The reclaimer reads this.
            b.held = true;
        }
        let pool = self.pools.get_mut(&info.pool).expect("just read");
        let src = pool.client.as_slice();
        let Some(dst) = self.transport.bytes_mut(&region) else {
            return;
        };
        let usable = src.len().min(dst.len()).min(pool.size);

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

        let started = Instant::now();
        let copied = match self.scope {
            CopyScope::Pool => {
                dst[..usable].copy_from_slice(&src[..usable]);
                usable
            }
            CopyScope::Buffer => copy_extent(src, dst, &info, usable),
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
                    _ if full => copy_extent(src, dst, &info, usable),
                    Stale::Everything => copy_extent(src, dst, &info, usable),
                    Stale::Rects(rects) if rects.is_empty() => 0,
                    Stale::Rects(rects) => copy_damage(src, dst, &info, &rects, usable),
                }
            }
        };
        // Deliberately outside the timed span above: this is an instrument, and
        // §13.8's retracted throughput number is what happens when one is left
        // inside the measurement.
        self.stats.copy_nanos += started.elapsed().as_nanos() as u64;
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
                        self.notes.push(format!(
                            "stale: surface {surface_id} buffer {buffer_id} \
                             {}x{} stride {} offset {} -- {stale} rows differ after copying \
                             {owed_kind} (this commit damaged {} rects, first {:?})",
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
        // cases. See docs/design.md §11.2.
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
        other => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{other:?} cannot become a file descriptor on this side"),
        )),
    }
}

#[cfg(test)]
mod tests;
