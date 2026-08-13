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
}

#[derive(Clone, Copy, Debug)]
struct BufferInfo {
    pool: u32,
    offset: i32,
    width: i32,
    height: i32,
    stride: i32,
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

#[derive(Default)]
struct Surface {
    pending_buffer: Option<u32>,
    pending_attached: bool,
    current: Option<u32>,
    damage: Vec<Rect>,
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

    // --- the clipboard, stage 13 -------------------------------------------
    //
    // Counted separately from `fds_received` / `fds_materialised` even though
    // both of those move too, because §14.7's line is an assertion about the
    // *boundary* and these are an assertion about a feature. See §22.4 for
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
        format!(
            "{} commits, {} copies, {mb:.2} MiB in {ms:.1} ms ({per}); \
             {} pools ({} released, {} resizes, {} overflows); {} fds in, {} out{}; \
             {} client msgs, {} server msgs{clip}",
            self.commits,
            self.copies,
            self.pools_created,
            self.pools_released,
            self.pool_resizes,
            self.resize_overflows,
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
    pub const POOL_CREATE_BUFFER: u16 = 0;
    pub const POOL_DESTROY: u16 = 1;
    pub const POOL_RESIZE: u16 = 2;
    pub const BUFFER_DESTROY: u16 = 0;
    pub const SURFACE_DESTROY: u16 = 0;
    pub const SURFACE_ATTACH: u16 = 1;
    pub const SURFACE_DAMAGE: u16 = 2;
    pub const SURFACE_COMMIT: u16 = 6;
    pub const SURFACE_DAMAGE_BUFFER: u16 = 9;
    pub const DISPLAY_DELETE_ID: u16 = 1;
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
/// (docs/design.md §22.3); pulling all five would open five pipes, wake the
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
    /// descriptor that used to end the connection (docs/design.md §22.1).
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
/// So the proxy answers them itself. It never forwards `create_data_source`,
/// so the compositor has no source of ours to send `send` on; it never forwards
/// `receive`, so the compositor is never asked to write into a descriptor. What
/// it *does* forward, unchanged, is every announcement: `wl_data_device.
/// data_offer`, `wl_data_offer.offer` and `wl_data_device.selection` are plain
/// bytes and the compositor is much better placed to decide who may see the
/// selection than a proxy is.
#[derive(Default)]
struct Clip {
    /// Client-created `wl_data_source`s and the mime types each has offered.
    sources: HashMap<u32, Vec<String>>,
    /// The source that owns the selection on this connection, if any.
    selection: Option<u32>,
    /// A source in the middle of a drag, if any.
    dragging: Option<u32>,
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
    /// Messages this program is sending as though it were the compositor, and
    /// the descriptors that ride beside them.
    to_client: Vec<u8>,
    to_client_fds: Vec<OwnedFd>,
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
    clip: Clip,
    data_ops: DataOps,
}

impl Session {
    pub fn new(
        spec: Spec,
        transport: Box<dyn PoolTransport>,
        scope: CopyScope,
        headroom: u64,
    ) -> Session {
        let data_ops = DataOps::from(&spec);
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
            clip: Clip { next_token: 1, ..Clip::default() },
            data_ops,
        }
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
            && self.pools.get(&header.object).is_some_and(|p| p.region.is_none()));
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
                let b = BufferInfo {
                    pool: header.object,
                    offset: wire::word_at(body, &spans[1]).unwrap_or(0) as i32,
                    width: wire::word_at(body, &spans[2]).unwrap_or(0) as i32,
                    height: wire::word_at(body, &spans[3]).unwrap_or(0) as i32,
                    stride: wire::word_at(body, &spans[4]).unwrap_or(0) as i32,
                };
                let id = wire::word_at(body, &spans[0]).unwrap_or(0);
                self.buffers.insert(id, b);
                if let Some(pool) = self.pools.get_mut(&header.object) {
                    pool.buffers += 1;
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
                if let Some(pool) = self.pools.get_mut(&header.object) {
                    pool.zombie = true;
                }
                self.release_pool_if_done(header.object);
            }
            ("wl_buffer", op::BUFFER_DESTROY) => {
                if let Some(b) = self.buffers.remove(&header.object) {
                    if let Some(pool) = self.pools.get_mut(&b.pool) {
                        pool.buffers = pool.buffers.saturating_sub(1);
                    }
                    self.release_pool_if_done(b.pool);
                }
            }
            ("wl_surface", op::SURFACE_DESTROY) => {
                self.surfaces.remove(&header.object);
            }
            ("wl_surface", op::SURFACE_ATTACH) => {
                let buffer = wire::word_at(body, &spans[0]).unwrap_or(0);
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
            }
            _ => {}
        }

        let out_body = if msg.has_fd() {
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
                     got one (docs/design.md §22.2).",
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
            // The source is created **here and nowhere else**. The compositor
            // never learns the id, which is exactly what makes it impossible
            // for it to send `wl_data_source.send` — the event that would carry
            // a descriptor the wrong way across the boundary.
            ("wl_data_device_manager", "create_data_source") => {
                if let Some(id) = wire::word_at(body, &spans[0]) {
                    self.clip.sources.insert(id, Vec::new());
                }
                true
            }
            ("wl_data_source", "offer") => {
                if let Some(mime) = wire::string_at(body, &spans[0])
                    && let Some(list) = self.clip.sources.get_mut(&object)
                    && !list.contains(&mime)
                {
                    list.push(mime);
                }
                true
            }
            ("wl_data_source", "set_actions") => {
                if let Some(actions) = wire::word_at(body, &spans[0]) {
                    self.clip.drag_actions = actions;
                }
                true
            }
            ("wl_data_source", "destroy") => {
                self.clip.sources.remove(&object);
                if self.clip.selection == Some(object) {
                    self.clip.selection = None;
                }
                if self.clip.dragging == Some(object) {
                    self.clip.dragging = None;
                }
                // The id is the client's and the compositor never knew it, so
                // the `wl_display.delete_id` that lets libwayland reuse the
                // number has to come from here. Without it the client's id
                // allocator leaks one number per copy — harmless for an hour
                // and not for a day.
                self.forget_client_object(object);
                true
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
                        self.begin_paste(mime, fd);
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

    /// Retire an id the compositor never knew about.
    fn forget_client_object(&mut self, id: u32) {
        self.objects.forget(id);
        self.clip
            .to_client
            .extend_from_slice(&wire::message(1, op::DISPLAY_DELETE_ID, &id.to_ne_bytes()));
    }

    /// One event on a client-owned `wl_data_source`, sent as though this were
    /// the compositor.
    fn source_event(&mut self, source: u32, opcode: u16, body: &[u8]) {
        self.clip
            .to_client
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
    fn begin_paste(&mut self, mime: String, fd: OwnedFd) {
        let token = self.clip.next_token;
        self.clip.next_token = self.clip.next_token.wrapping_add(1).max(1);
        // Non-blocking on **the write end this process now holds**. A pipe's
        // two ends are separate open file descriptions, so this cannot make the
        // client's own reads non-blocking; without it a payload larger than the
        // pipe buffer would block the pump on a client that is not reading yet.
        let _ = set_nonblocking(&fd);
        self.stats.pastes_served += 1;
        let source = if self.clip.dragging.is_some() { Source::Drag } else { Source::Clipboard };
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
            self.clip.ready = Some(Pulled { payloads, for_drag });
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
            std::mem::take(&mut self.clip.to_client),
            std::mem::take(&mut self.clip.to_client_fds),
        )
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
    fn flush_deferred(&mut self) -> Vec<u8> {
        let pending: Vec<u32> = self
            .pools
            .iter()
            .filter(|(_, p)| p.region.is_none())
            .map(|(id, _)| *id)
            .collect();
        let mut out = Vec::new();
        for pool_id in pending {
            let (size, shm) = {
                let p = &self.pools[&pool_id];
                (p.size, p.shm)
            };
            let region_size = (size as u64).saturating_mul(self.headroom);
            let region = match self.transport.create(region_size) {
                Ok(r) => r,
                Err(e) => {
                    // Leave it deferred rather than announcing a pool with no
                    // memory behind it: the compositor would map nothing and
                    // the client would see a protocol error from the wrong
                    // place. The next commit reports zero copies, which is what
                    // the harness checks.
                    self.note(format!("pool {pool_id}: could not publish {region_size} bytes: {e}"));
                    continue;
                }
            };
            self.stats.pools_created += 1;
            self.note(format!(
                "pool {pool_id}: {size} bytes from the client -> {}",
                region.describe()
            ));
            let mut body = Vec::new();
            body.extend_from_slice(&pool_id.to_ne_bytes());
            wire::push_bytes(&mut body, &region.descriptor.encode());
            body.extend_from_slice(&(size as u32).to_ne_bytes());
            out.extend_from_slice(&wire::message(shm, op::SHM_CREATE_POOL, &body));
            self.pools.get_mut(&pool_id).expect("just read").region = Some(region);
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

        if interface == "wl_display"
            && header.opcode == op::DISPLAY_DELETE_ID
            && let Some(id) = wire::word_at(body, &spans[0])
        {
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

    fn record_new_ids(&mut self, body: &[u8], spans: &[Span], args: &[crate::spec::Arg]) {
        for (span, arg) in spans.iter().zip(args) {
            match arg.kind {
                ArgKind::NewId => {
                    if let (Some(id), Some(iface)) =
                        (wire::word_at(body, span), arg.interface.as_deref())
                    {
                        self.objects.record(id, iface);
                    }
                }
                ArgKind::NewIdUntyped => {
                    if let Some((iface, _version, id)) = wire::untyped_new_id(body, span) {
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
        self.pools.insert(pool_id, Pool { client, region: None, shm, size, zombie: false, acked: false, buffers: 0 });
        Ok(())
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
        let deferred = pool.region.is_none();
        if let Some(region) = &pool.region
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

    /// The frame path: apply the pending state and move the pixels.
    fn commit(&mut self, surface_id: u32) {
        self.stats.commits += 1;
        let Some(surface) = self.surfaces.get_mut(&surface_id) else {
            return;
        };
        if surface.pending_attached {
            surface.current = surface.pending_buffer;
            surface.pending_attached = false;
        }
        let damage = std::mem::take(&mut surface.damage);
        let Some(buffer_id) = surface.current else {
            return;
        };
        let Some(info) = self.buffers.get(&buffer_id).copied() else {
            return;
        };
        let Some(pool) = self.pools.get_mut(&info.pool) else {
            return;
        };

        let Some(region) = pool.region.clone() else {
            // A commit against a pool that was never announced. Only reachable
            // if the transport refused to publish it, which is already noted.
            return;
        };
        let src = pool.client.as_slice();
        let Some(dst) = self.transport.bytes_mut(&region) else {
            return;
        };
        let usable = src.len().min(dst.len()).min(pool.size);

        let started = Instant::now();
        let copied = match self.scope {
            CopyScope::Pool => {
                dst[..usable].copy_from_slice(&src[..usable]);
                usable
            }
            CopyScope::Buffer => copy_extent(src, dst, &info, usable),
            CopyScope::Damage => {
                if damage.is_empty() {
                    // An empty damage list on a commit with a buffer attached
                    // means the client asserted nothing changed. Taking it at
                    // its word is what the protocol means; second-guessing it
                    // would defeat the point of damage tracking. But the
                    // *first* commit of a buffer has nothing in the region yet,
                    // so a buffer whose extent has never been copied is copied
                    // whole regardless of what the client said.
                    copy_extent(src, dst, &info, usable)
                } else {
                    copy_damage(src, dst, &info, &damage, usable)
                }
            }
        };
        self.stats.copy_nanos += started.elapsed().as_nanos() as u64;
        if copied > 0 {
            self.stats.copies += 1;
            self.stats.bytes_copied += copied as u64;
        }
    }
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
    out
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
