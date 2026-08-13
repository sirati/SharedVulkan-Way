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
    pub messages_client: u64,
    pub messages_server: u64,
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
        format!(
            "{} commits, {} copies, {mb:.2} MiB in {ms:.1} ms ({per}); \
             {} pools ({} released, {} resizes, {} overflows); {} fds in, {} out; \
             {} client msgs, {} server msgs",
            self.commits,
            self.copies,
            self.pools_created,
            self.pools_released,
            self.pool_resizes,
            self.resize_overflows,
            self.fds_received,
            self.fds_materialised,
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
}

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
                let fd = materialise(&d).map_err(|e| Error::Materialise {
                    interface: interface.clone(),
                    message: msg.name.clone(),
                    descriptor: format!("{d:?}"),
                    source: e,
                })?;
                self.stats.fds_materialised += 1;
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
                            self.note(format!(
                                "client bound {iface}, which is not in the vendored protocols; \
                                 messages on it cannot be walked and the connection will fail \
                                 if it ever carries an h"
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
fn materialise(d: &Descriptor) -> io::Result<std::os::fd::OwnedFd> {
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
        // cases. See docs/design.md §10.
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
        Descriptor::ArenaSlice { .. } => {
            // A window into the arena has no fd of its own, so one is made:
            // a fresh anonymous object with the bytes copied into it. A copy
            // rather than a mapping because the client will be given this fd
            // and must not be able to write into the arena.
            let mapping = wc_handle::Handle::from_descriptor(d.clone()).map()?;
            let bytes = mapping.as_slice();
            // SAFETY: a NUL-terminated literal name and a valid flag.
            let fd = unsafe {
                libc::memfd_create(c"wc-proxy-event".as_ptr(), libc::MFD_CLOEXEC)
            };
            if fd < 0 {
                return Err(io::Error::last_os_error());
            }
            // SAFETY: `fd` was just returned by `memfd_create` and is owned here.
            let owned = unsafe { <OwnedFd as std::os::fd::FromRawFd>::from_raw_fd(fd) };
            let mut written = 0usize;
            while written < bytes.len() {
                // SAFETY: writing `bytes` through a live fd this function owns.
                let n = unsafe {
                    libc::write(
                        owned.as_raw_fd(),
                        bytes[written..].as_ptr().cast(),
                        bytes.len() - written,
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
