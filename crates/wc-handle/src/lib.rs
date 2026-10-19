//! What a Wayland `h` argument becomes when the operating system has no
//! `SCM_RIGHTS`.
//!
//! # The problem, stated exactly
//!
//! Windows AF_UNIX carries no ancillary data. Microsoft say so directly:
//! "There is no support for ancillary data in the Windows unix socket
//! implementation." So the `h` argument — which on Linux is a real file
//! descriptor duplicated into the receiving process by the kernel — has no
//! transport.
//!
//! Windows' own substitute, `DuplicateHandle`, is **push**-shaped: the sender
//! writes into the receiver's handle table, and therefore needs a process
//! handle for the receiver with `PROCESS_DUP_HANDLE` rights. For a compositor
//! that is a bad trade — it would mean holding a duplicate-rights handle on
//! every client process, which is a far stronger capability than a display
//! server has any business holding.
//!
//! # The decision
//!
//! **Named kernel objects, opened by name, with the name and the object's kind
//! carried in-band in the message body.** No `DuplicateHandle`, no rights on the
//! peer, no side channel.
//!
//! This falls out of a fact phase 1 established on the real hardware: a *named*
//! section object needs no `DuplicateHandle` relationship at all, because the
//! peer opens it by name. `DuplicateHandle` remains as an escape hatch
//! ([`Descriptor::NtHandle`]) for objects that genuinely cannot be named, but
//! nothing on the critical path needs it — `CreateFileMappingW`,
//! `CreateNamedPipeW` and even Vulkan's `VkExportMemoryWin32HandleInfoKHR` all
//! take a name.
//!
//! # Why in-band, and what it costs
//!
//! The research (§2.2) observes that libwayland's fd stream "is already a
//! separate, strictly-ordered side channel, matched positionally to `h`
//! arguments", and proposes replacing `SCM_RIGHTS` with another ordered side
//! channel. We do not. A descriptor is a variable-length byte string, wayland's
//! wire format already has a variable-length argument encoding (`array`: a
//! `u32` length followed by the bytes, padded to 4), and an `h` argument
//! occupies **zero** bytes of message body today. So encoding `h` exactly like
//! `array` costs nothing structurally and removes an entire class of problem:
//!
//! * no second connection to open, order against, or deadlock on;
//! * no handle table to keep synchronised between two processes;
//! * no `MAX_FDS_OUT = 28` cap, and no need for libwayland's "send exactly one
//!   byte of real data when the control buffer is full" workaround;
//! * the type of the object is on the wire, so we never have to infer it. This
//!   is the cost waypipe could not avoid — it needed a full protocol parser
//!   because "one cannot determine which passed file descriptor corresponds to
//!   which message without parsing the message". We escape it because on
//!   Windows there is no fd to begin with: the code that creates the object
//!   always knows what it created, so it emits a typed token from the start.
//!
//! **The cost is that the Windows byte stream is not libwayland's byte
//! stream.** A stock `libwayland-client` cannot talk to us. This is acceptable,
//! and it is worth being precise about why rather than waving at it:
//!
//! * There is no stock libwayland-client on Windows to be incompatible with.
//!   GTK4 hard-disables its Wayland backend on Windows and QtWayland is not
//!   built for Windows at all, so the population of unpatched Windows Wayland
//!   clients is exactly zero and will stay that way until somebody ports a
//!   toolkit (research §3).
//! * It does not foreclose the "Linux clients over a proxy" topology
//!   (Architecture B). In that topology the proxy terminates the Wayland
//!   connection and re-materialises fds at the boundary anyway — it speaks
//!   ordinary Wayland to the Linux client and ours to the compositor. The
//!   deviation is confined to the hop that was always going to be special.
//! * The deviation is *only* on Windows. The Unix build of the fork still uses
//!   `SCM_RIGHTS` and is byte-compatible with libwayland, which is what lets the
//!   protocol layer be tested against a real third-party client at all.
//!
//! # Security note, stated plainly
//!
//! A named object can be opened by anyone who can guess or learn its name,
//! whereas a `DuplicateHandle` target is exactly one process. We mitigate with
//! 128 bits of randomness per name and by creating in the per-logon-session
//! `Local\` namespace rather than `Global\`, so another user's processes cannot
//! reach it even knowing the name. That is weaker than the Unix model, where an
//! fd is a capability that cannot be guessed at all. It is a real difference and
//! it should be revisited if this ever hosts a mutually-distrusting client set;
//! `CreatePrivateNamespace` with a boundary descriptor is the next step and is
//! deliberately not taken yet, because it needs the client to be launched by us.

//! # Stage 4: the two kinds a *cross-machine* peer can open
//!
//! Everything above is about two Windows processes. Stage 4 puts a Linux proxy
//! on one side of the boundary and the Windows compositor on the other
//! (docs/goal-firefox.md), and there the `Local\` namespace is no help at all:
//! a Linux process can neither create an NT object name nor open one. The rule
//! does not relax — **no descriptor and no handle crosses, ever, even when both
//! sides happen to be on one machine** — so the question is only which *names*
//! both namespaces understand.
//!
//! Two do, and they are the two kinds added here:
//!
//! * [`Descriptor::SharedFile`] — a **path** on a filesystem both sides can
//!   reach. Under Wine that is the `Z:` drive; on the real guest it is the
//!   virtio-fs mount. The peer calls `CreateFileW` + `CreateFileMappingW` on it
//!   itself, which is the sanctioned pattern with a different namespace, not a
//!   new mechanism. Measured with `scripts/probe-filemap.c`: a Wine process's
//!   view of a `Z:` path is coherent with a Linux `MAP_SHARED` mmap of the same
//!   file in both directions, with no flush.
//!
//! * [`Descriptor::ArenaSlice`] — reserved since stage 1 and now constructed.
//!   Nothing is opened at all: both sides mapped the arena before any Wayland
//!   traffic, out of band, so only `(arena, offset, len)` crosses. This is the
//!   real deployment's answer, because there is no filesystem call cheap enough
//!   to make per pool across a hypervisor and no name a guest can mint that a
//!   host can open.
//!
//! Note what is *absent* from both: any duplication, any `PROCESS_DUP_HANDLE`,
//! any ancillary data. `SharedFile` names an object; `ArenaSlice` names a
//! window into one that is already mapped.

use std::fmt;
use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

pub mod arena;
/// §107: the shared BAR cut into per-session regions.
pub mod barpool;
/// GROWTH. The host→guest channel that asks for another region, as a message
/// on shared_vulkan's existing control region and rings rather than as a
/// mechanism of its own.
pub mod grow;
/// IVSHMEM ARENA. The header at the front of an ivshmem region, encoded and
/// decoded by the same code on both sides of the hypervisor.
pub mod ivshmem;
#[cfg(target_os = "macos")]
pub mod iosection;
mod platform;
pub mod publish;
pub mod macboot;

/// An ivshmem device by root-bus slot, mapped read-write and cached, for the
/// GPU sessions' arena (waycompose-design.md §87). The view owns the device
/// handle; dropping it is the release.
pub use platform::{MappedView, map_ivshmem_slot};

/// How many objects of each kind this process has mapped.
///
/// See [`map_counts`]. Deliberately process-global and deliberately not
/// resettable: they answer "which mechanism actually carried these pixels",
/// which is a question about the run, not about a moment in it.
mod counters {
    use super::AtomicU64;
    pub static SECTION: AtomicU64 = AtomicU64::new(0);
    pub static SHARED_FILE: AtomicU64 = AtomicU64::new(0);
    pub static ARENA_SLICE: AtomicU64 = AtomicU64::new(0);
}

/// How many objects of each kind have been mapped: `(sections, shared files,
/// arena slices)`.
///
/// This exists because of docs/waycompose-design.md §8.4's rule about the pixel-source
/// seam: *"a stage that added a second provider with no way to tell which one
/// ran would not really have added it."* Stage 4 adds two, and both arrive
/// through the same `wl_shm` code in smithay, so the compositor cannot tell
/// them apart from its own side. [`Handle::map`] can, because it is the
/// function that opens them, and this is what it reports.
pub fn map_counts() -> (u64, u64, u64) {
    (
        counters::SECTION.load(Ordering::Relaxed),
        counters::SHARED_FILE.load(Ordering::Relaxed),
        counters::ARENA_SLICE.load(Ordering::Relaxed),
    )
}

/// How big a descriptor is allowed to be on the wire.
///
/// A bound is required because the receiving side allocates from it. The real
/// descriptors are tens of bytes; this is loose enough to never be hit by
/// anything legitimate and tight enough that a hostile length cannot ask for a
/// gigabyte.
pub const MAX_DESCRIPTOR_LEN: usize = 1024;

/// The kinds of thing a Wayland `h` argument can be.
///
/// Every variant here corresponds to a row of the research's §2.4 table, which
/// enumerates what an fd actually *is* at each use site. The table's one
/// genuinely unportable row — `linux-dmabuf`'s per-plane dma-buf fds — has no
/// variant and never will: it is a Linux kernel ABI by name and by content, and
/// the answer is a replacement protocol rather than a shimmed fd.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Descriptor {
    /// A shared memory object: `wl_shm.create_pool`, `wl_keyboard.keymap`, an
    /// ICC profile, a gamma ramp.
    ///
    /// On Windows a named file-mapping section; on Unix a file at a path. In
    /// both cases the peer opens it *by name* and maps it.
    Section {
        /// Name the peer opens. On Windows this is a kernel object name; on
        /// Unix it is a filesystem path.
        name: String,
        /// Size in bytes at creation.
        ///
        /// Note that on Windows this is fixed for the life of the section:
        /// `CreateFileMapping` takes a *maximum* and there is no API to grow an
        /// existing section, nor any `mremap`. `wl_shm_pool.resize` therefore
        /// cannot be honoured in place, and the two workable answers —
        /// `SEC_RESERVE` with a large maximum plus incremental commit, or
        /// re-keying the pool onto a new section at the cost of a copy — are a
        /// `wl_shm` problem for stage 3, not a transport problem.
        size: u64,
    },
    /// A byte stream: clipboard and drag-and-drop transfer
    /// (`wl_data_offer.receive`, `wl_data_source.send`), `weston-debug`.
    ///
    /// A named pipe on Windows, a FIFO on Unix.
    Pipe {
        /// Name the peer opens.
        name: String,
        /// True if the *receiver* of this descriptor is meant to read from it.
        /// Wayland's pipe fds are unidirectional and which way round they go is
        /// per-request, so it has to be carried rather than assumed.
        peer_reads: bool,
    },
    /// An NT handle already duplicated into the receiving process.
    ///
    /// The escape hatch, for objects that cannot be named. Requires the sender
    /// to hold `PROCESS_DUP_HANDLE` on the receiver, which is exactly the
    /// requirement the rest of this design exists to avoid — so a descriptor of
    /// this kind is a deliberate, local exception and not a fallback that gets
    /// taken by accident. Nothing constructs one yet.
    NtHandle {
        /// The handle value, valid in the *receiving* process.
        value: u64,
        /// What kind of object it is, so the receiver knows what to do with it.
        object_kind: u32,
    },
    /// A shared-memory object named by a **filesystem path** rather than by an
    /// entry in the NT object namespace.
    ///
    /// ADDED FOR STAGE 4. `Section` is the right kind between two Windows
    /// processes and is unreachable between a Linux one and a Windows one:
    /// `Local\wc-…` is a name only the NT object manager understands. A path is
    /// the name both namespaces share — the `Z:` drive under Wine, the
    /// virtio-fs mount on the real guest.
    ///
    /// The peer opens the path itself, exactly as it opens a section by name.
    /// Nothing is duplicated and nothing is passed; this is the same rule with
    /// a different namespace.
    SharedFile {
        /// The path **as the peer will spell it**, which is generally not how
        /// the creator spells it: the same bytes live at `/tmp/wc-pool-…` on
        /// Linux and at `Z:\tmp\wc-pool-…` inside Wine. The creator therefore
        /// keeps its own path in the object it holds open and puts the peer's
        /// spelling here. Getting this backwards produces a descriptor that
        /// looks perfectly well-formed and cannot be opened, so the asymmetry
        /// is in the type's documentation rather than in a caller's memory.
        path: String,
        /// Size in bytes. Unlike a Windows section this *can* grow — a file can
        /// be extended and re-mapped — but the peer is told a size and maps
        /// exactly that, so growth still needs a new descriptor.
        size: u64,
    },
    /// A window into the phase-1 shared arena: `(arena, offset, len)`.
    ///
    /// This is the variant that makes phase 1 load-bearing rather than
    /// incidental — a `wl_shm` pool that is a slice of the arena needs no new
    /// kernel object at all, and a client rendering with the forwarded Vulkan
    /// has already put its pixels there. It was deliberately part of the type
    /// from stage 1 so that constructing it later would not be a wire-format
    /// break, and stage 4 constructs it.
    ///
    /// **Nothing is opened when one of these is received.** The arena was
    /// mapped by both sides out of band, before any Wayland traffic, which is
    /// what makes this the only kind that can be minted per pool at frame rate
    /// across a hypervisor. See [`arena`] for the receiving side.
    ArenaSlice {
        /// Which arena, in phase 1's numbering.
        arena: u32,
        /// Byte offset into it.
        offset: u64,
        /// Length in bytes.
        len: u64,
    },
}

const KIND_SECTION: u32 = 1;
const KIND_PIPE: u32 = 2;
const KIND_NT_HANDLE: u32 = 3;
const KIND_ARENA_SLICE: u32 = 4;
const KIND_SHARED_FILE: u32 = 5;

/// A descriptor that could not be understood.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecodeError(&'static str);

impl fmt::Display for DecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "malformed handle descriptor: {}", self.0)
    }
}

impl std::error::Error for DecodeError {}

impl From<DecodeError> for io::Error {
    fn from(e: DecodeError) -> Self {
        io::Error::new(io::ErrorKind::InvalidData, e)
    }
}

/// A little-endian reader that cannot run off the end.
struct Reader<'a>(&'a [u8]);

impl<'a> Reader<'a> {
    fn u32(&mut self) -> Result<u32, DecodeError> {
        let (head, tail) = self.0.split_at_checked(4).ok_or(DecodeError("truncated u32"))?;
        self.0 = tail;
        Ok(u32::from_le_bytes(head.try_into().unwrap()))
    }
    fn u64(&mut self) -> Result<u64, DecodeError> {
        let (head, tail) = self.0.split_at_checked(8).ok_or(DecodeError("truncated u64"))?;
        self.0 = tail;
        Ok(u64::from_le_bytes(head.try_into().unwrap()))
    }
    fn string(&mut self) -> Result<String, DecodeError> {
        let len = self.u32()? as usize;
        if len > MAX_DESCRIPTOR_LEN {
            return Err(DecodeError("name longer than MAX_DESCRIPTOR_LEN"));
        }
        let (head, tail) = self.0.split_at_checked(len).ok_or(DecodeError("truncated string"))?;
        self.0 = tail;
        String::from_utf8(head.to_vec()).map_err(|_| DecodeError("name is not UTF-8"))
    }
}

fn put_string(out: &mut Vec<u8>, s: &str) {
    out.extend_from_slice(&(s.len() as u32).to_le_bytes());
    out.extend_from_slice(s.as_bytes());
}

impl Descriptor {
    /// Serialise to the bytes that ride in the message body.
    ///
    /// Little-endian rather than native-endian, unlike the surrounding wayland
    /// wire format. That is a deliberate small inconsistency: wayland is
    /// native-endian because a Unix socket cannot cross an endianness boundary,
    /// but a descriptor may one day cross a proxy (Architecture B), and a
    /// format that is only correct when both ends agree by accident is a bug
    /// waiting for a big-endian day that may never come but costs nothing to
    /// prevent.
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(32);
        match self {
            Self::Section { name, size } => {
                out.extend_from_slice(&KIND_SECTION.to_le_bytes());
                out.extend_from_slice(&size.to_le_bytes());
                put_string(&mut out, name);
            }
            Self::Pipe { name, peer_reads } => {
                out.extend_from_slice(&KIND_PIPE.to_le_bytes());
                out.extend_from_slice(&u32::from(*peer_reads).to_le_bytes());
                put_string(&mut out, name);
            }
            Self::NtHandle { value, object_kind } => {
                out.extend_from_slice(&KIND_NT_HANDLE.to_le_bytes());
                out.extend_from_slice(&value.to_le_bytes());
                out.extend_from_slice(&object_kind.to_le_bytes());
            }
            Self::ArenaSlice { arena, offset, len } => {
                out.extend_from_slice(&KIND_ARENA_SLICE.to_le_bytes());
                out.extend_from_slice(&arena.to_le_bytes());
                out.extend_from_slice(&offset.to_le_bytes());
                out.extend_from_slice(&len.to_le_bytes());
            }
            Self::SharedFile { path, size } => {
                out.extend_from_slice(&KIND_SHARED_FILE.to_le_bytes());
                out.extend_from_slice(&size.to_le_bytes());
                put_string(&mut out, path);
            }
        }
        out
    }

    /// Parse bytes that arrived in a message body.
    pub fn decode(bytes: &[u8]) -> Result<Self, DecodeError> {
        if bytes.len() > MAX_DESCRIPTOR_LEN {
            return Err(DecodeError("descriptor longer than MAX_DESCRIPTOR_LEN"));
        }
        let mut r = Reader(bytes);
        let out = match r.u32()? {
            KIND_SECTION => {
                let size = r.u64()?;
                Self::Section { name: r.string()?, size }
            }
            KIND_PIPE => {
                let peer_reads = r.u32()? != 0;
                Self::Pipe { name: r.string()?, peer_reads }
            }
            KIND_NT_HANDLE => {
                Self::NtHandle { value: r.u64()?, object_kind: r.u32()? }
            }
            KIND_ARENA_SLICE => {
                Self::ArenaSlice { arena: r.u32()?, offset: r.u64()?, len: r.u64()? }
            }
            KIND_SHARED_FILE => {
                let size = r.u64()?;
                Self::SharedFile { path: r.string()?, size }
            }
            _ => return Err(DecodeError("unknown descriptor kind")),
        };
        if !r.0.is_empty() {
            return Err(DecodeError("trailing bytes after descriptor"));
        }
        Ok(out)
    }
}

/// A live handle to whatever a `h` argument referred to.
///
/// This is the type that stands in for `OwnedFd` in the forked
/// `wayland-backend` on Windows. It is `Clone` because the protocol layer needs
/// to duplicate a handle when a message is written (upstream calls
/// `try_clone_to_owned()` on the `BorrowedFd` at that point), and a refcounted
/// clone is the honest analogue of `dup()`: both ends observe the same object
/// and the object lives until the last reference goes.
///
/// It carries the descriptor, and optionally a keepalive for the OS object when
/// this process is the one that created it. The creator must hold the section
/// open, because on Windows a named section with no open handles is destroyed
/// even though the name is still spellable — closing it before the peer opens
/// it is a race the descriptor alone cannot express.
#[derive(Clone)]
pub struct Handle {
    descriptor: Descriptor,
    keepalive: Option<Arc<Owned>>,
}

/// What a `Handle` holds when this process is the one that made the object.
///
/// STAGE 10 SPLIT THIS IN TWO, and the split is the crate's write rule stated
/// as a type. Before, "owns something" and "is an OS object" were the same
/// thing, because the only way to create something for a peer was to create an
/// OS object. A compositor cutting a keymap out of an arena creates no object
/// at all — it takes bytes out of a region that already exists — so the
/// keepalive is now either the object or the allocation.
///
/// Both arms mean exactly the same thing to every caller: **this process made
/// these bytes and must hold them for as long as the protocol object they back**
/// (docs/waycompose-design.md §2.5). What differs is what release means — closing a
/// section and unlinking a file for one, returning a range to the outbound
/// window's free list for the other.
enum Owned {
    Object(platform::OwnedObject),
    /// A FIFO this process created for a peer to open. §81.
    ///
    /// Held for the same reason as `Object`: the name is only live while the
    /// creator holds the object, and a peer that has not opened it by then
    /// loses the race.
    Pipe(platform::OwnedPipe),
    /// An allocation out of this process's outbound arena window. The only
    /// value in this crate that carries a write path into an arena, and not
    /// constructible from anything that arrived on the wire — see
    /// [`arena::Gift`].
    Gift(arena::Gift),
}

impl fmt::Debug for Handle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Handle")
            .field("descriptor", &self.descriptor)
            .field("owned", &self.keepalive.is_some())
            .finish()
    }
}

/// Two handles are equal when they name the same object.
///
/// This mirrors what upstream does for real fds, where `PartialEq` on an
/// `Argument::Fd` compares `as_raw_fd()` values. Naming the same object is a
/// stronger and more useful statement than upstream's, which compares only
/// numbers and so calls two `dup()`s of one file unequal.
impl PartialEq for Handle {
    fn eq(&self, other: &Self) -> bool {
        self.descriptor == other.descriptor
    }
}

impl Eq for Handle {}

impl Handle {
    /// Wrap a descriptor received from a peer. Owns nothing.
    pub fn from_descriptor(descriptor: Descriptor) -> Self {
        Self { descriptor, keepalive: None }
    }

    /// What this handle refers to.
    pub fn descriptor(&self) -> &Descriptor {
        &self.descriptor
    }

    /// True if this process created the object and is keeping it alive.
    pub fn is_owner(&self) -> bool {
        self.keepalive.is_some()
    }

    /// Create a new shared-memory object of `size` bytes and return a handle to
    /// it. The returned handle owns the object; the descriptor may be sent to a
    /// peer, which can then [`map`](Handle::map) it.
    pub fn create_section(size: u64) -> io::Result<Self> {
        let name = platform::random_section_name();
        let object = platform::create_section(&name, size)?;
        Ok(Self {
            descriptor: Descriptor::Section { name, size },
            keepalive: Some(Arc::new(Owned::Object(object))),
        })
    }

    /// Cut `size` bytes out of this process's outbound arena window
    /// ([`arena::reserve_outbound`]) and return a handle that owns them.
    ///
    /// ADDED FOR STAGE 10. The descriptor is a [`Descriptor::ArenaSlice`], which
    /// is three integers and names nothing: no path, no kernel object, nothing
    /// the peer has to open and therefore nothing a filesystem has to be able to
    /// carry. That is the whole point — it is the only kind of object a
    /// compositor can create for a client across a hypervisor without a shared
    /// filesystem existing at all.
    ///
    /// Fails, rather than falling back, when there is no window or the window is
    /// full. See [`create_shared`](Handle::create_shared) for why the choice is
    /// made once rather than per call.
    pub fn create_arena_gift(size: u64) -> io::Result<Self> {
        let gift = arena::give(size)?;
        Ok(Self {
            descriptor: gift.descriptor(),
            keepalive: Some(Arc::new(Owned::Gift(gift))),
        })
    }

    /// Create an object of `size` bytes **for a peer to open**, wherever
    /// [`publish`] says such objects go.
    ///
    /// ADDED FOR STAGE 6, and the difference from [`create_section`] is which
    /// namespace the peer has to be able to reach:
    ///
    /// * With no published directory this *is* `create_section` — a
    ///   pagefile-backed `Local\wc-…` on Windows, a file in the temporary
    ///   directory on Unix. Right when both processes are on one machine.
    /// * With one, the object is a file in that directory and the descriptor
    ///   carries the path **as the peer spells it**. That is the only kind a
    ///   process on the other side of the boundary can open, because an NT
    ///   object name means nothing to a Linux process (docs/waycompose-design.md §9.4).
    ///
    /// The caller does not choose, and that is deliberate: every call site that
    /// creates an object for a peer wants the same answer, and the answer is a
    /// property of the deployment rather than of the call. A caller that
    /// genuinely wants a same-machine section still says so by name.
    ///
    /// **The lifetime rule applies unchanged** (docs/waycompose-design.md §2.5): the
    /// returned handle must be held for at least as long as the protocol object
    /// it backs. For a file-backed object it is stronger than a courtesy —
    /// dropping the handle deletes the file.
    ///
    /// # STAGE 10 puts a third answer in front of the other two
    ///
    /// If this process has an **outbound arena window**, the object is a slice
    /// of it and nothing is created at all. That is preferred over a published
    /// directory, and the order is the argument:
    ///
    /// * An arena slice needs no filesystem both sides can reach. A published
    ///   directory needs one, and docs/waycompose-design.md §10.4 measured what such a
    ///   filesystem costs on the deployment that matters — 330× slower to touch
    ///   — and §10.3 measured that it is not even coherent for a live mapping.
    ///   The keymap survived it because a keymap is a closed write (§12.11);
    ///   that is luck the next server-created object should not have to have.
    /// * The arena is already there. The compositor mapped it before the socket
    ///   existed; every other transport here is a per-object `CreateFileW` or
    ///   `CreateFileMappingW` on a path.
    ///
    /// So the full order is: **arena window, then published directory, then a
    /// session-local section** — most specific to least, each one available only
    /// when the deployment set it up. A caller still says nothing and gets the
    /// answer the deployment chose, which is unchanged and is the point.
    ///
    /// [`create_section`]: Handle::create_section
    pub fn create_shared(size: u64) -> io::Result<Self> {
        if arena::outbound().is_some() {
            return Self::create_arena_gift(size);
        }
        match publish::mint() {
            Some((local, peer)) => Self::create_shared_file(&local, &peer, size),
            None => Self::create_section(size),
        }
    }

    /// Create a **byte stream** a peer opens by name, and describe it as
    /// [`Descriptor::Pipe`].
    ///
    /// §81. `Descriptor::Pipe` has existed since stage 1 and until now nothing
    /// constructed one; this is what a clipboard or drag transfer needs when the
    /// two ends speak different `h` dialects. An out-of-band client hands the
    /// compositor a real pipe; an in-band source must be handed a *name*; and
    /// only a named stream can be both.
    ///
    /// `peer_reads` is recorded in the descriptor rather than inferred, because
    /// Wayland's pipe `h` arguments are unidirectional and **which way round
    /// they go is per request**: `wl_data_source.send` hands the source a
    /// descriptor to *write*, `wl_data_offer.receive` hands the requester one to
    /// *read*. A shim that assumed either would be wrong half the time.
    ///
    /// The creator keeps the other end — [`Handle::pipe_end`] — and must hold
    /// this `Handle` for as long as the transfer, exactly as for a section.
    pub fn create_pipe(peer_reads: bool) -> io::Result<Self> {
        let (local, peer) = publish::mint().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::Unsupported,
                "no directory for objects a peer must open: start the compositor with \
                 --shared-dir and --shared-peer-prefix",
            )
        })?;
        let pipe = platform::create_pipe(&local)?;
        Ok(Self {
            descriptor: Descriptor::Pipe { name: peer, peer_reads },
            keepalive: Some(Arc::new(Owned::Pipe(pipe))),
        })
    }

    /// The end of a [`Handle::create_pipe`] stream this process kept.
    ///
    /// `None` for any other kind of handle, and for a pipe descriptor that
    /// merely *arrived* — the receiver opens it with [`Handle::open_pipe`]
    /// instead, because it is the peer and not the creator.
    pub fn pipe_end(&self) -> Option<&std::fs::File> {
        match self.keepalive.as_deref() {
            Some(Owned::Pipe(pipe)) => Some(pipe.as_file()),
            _ => None,
        }
    }

    /// Open a [`Descriptor::Pipe`] that arrived from a peer.
    ///
    /// The direction is taken from the descriptor: `peer_reads` is written by
    /// the *creator* and is true when the side that receives the descriptor —
    /// this side — is meant to read. Resolving it here rather than at each call
    /// site is deliberate: a boolean meaning the opposite thing depending on who
    /// is asking is how a clipboard transfer ends up reading its own writes.
    pub fn open_pipe(&self) -> io::Result<std::fs::File> {
        match &self.descriptor {
            Descriptor::Pipe { name, peer_reads } => platform::open_pipe(name, !*peer_reads),
            other => Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{other:?} is not a stream and cannot be opened as one"),
            )),
        }
    }

    /// Create a shared-memory object at `local_path` and describe it to the
    /// peer as `peer_path`.
    ///
    /// The two paths are the same bytes seen from two namespaces —
    /// `/tmp/wc-pool-3f…` here, `Z:\tmp\wc-pool-3f…` in the guest — and the
    /// caller supplies both because only the caller knows how the peer reaches
    /// this filesystem. Nothing in this crate can derive one from the other,
    /// and a crate that guessed would be wrong on the deployment that matters.
    ///
    /// ADDED FOR STAGE 4; see the module header.
    pub fn create_shared_file(
        local_path: &str,
        peer_path: &str,
        size: u64,
    ) -> io::Result<Self> {
        let object = platform::create_file_object(local_path, size)?;
        Ok(Self {
            descriptor: Descriptor::SharedFile { path: peer_path.to_owned(), size },
            keepalive: Some(Arc::new(Owned::Object(object))),
        })
    }

    /// Take writable ownership of a shared-memory object **somebody else
    /// provided**.
    ///
    /// # Why this is not `create_shared_file` with a different flag
    ///
    /// Because of who deletes it. A created object is this process's to remove
    /// when it is done — that is what makes a crashed compositor not leave
    /// shared-memory files behind, and it mirrors a Windows section ceasing to
    /// exist with its last handle. An object that was provided is not: an arena
    /// belongs to whatever brought the two sides together (shared_vulkan's
    /// cold-path handshake in the real deployment, a harness here), it outlives
    /// every process that maps it, and a peer that unlinked it on exit would
    /// take the region away from everybody else.
    ///
    /// So this opens rather than creates, and drops rather than deletes. It is
    /// the *provider's* half of an arena — the side that writes — and the
    /// read-only half is [`arena::attach`].
    pub fn open_shared_file(
        local_path: &str,
        peer_path: &str,
        size: u64,
    ) -> io::Result<Self> {
        let object = platform::open_file_object(local_path, size)?;
        Ok(Self {
            descriptor: Descriptor::SharedFile { path: peer_path.to_owned(), size },
            keepalive: Some(Arc::new(Owned::Object(object))),
        })
    }

    /// Map this handle's object into the current address space, read-only.
    ///
    /// Fails for descriptor kinds that are not memory.
    ///
    /// **This is the one place that knows how each kind is reached**, which is
    /// why the counters live here too: [`map_counts`] is what lets a caller
    /// state *which* mechanism served a frame rather than assert it. A stage
    /// that added a second way for pixels to arrive with no way to tell which
    /// one ran would not really have added it (docs/waycompose-design.md §8.4).
    pub fn map(&self) -> io::Result<Mapping> {
        match &self.descriptor {
            Descriptor::Section { name, size } => {
                let len = usize::try_from(*size)
                    .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "section too large"))?;
                let inner = platform::map_section(name, len)?;
                counters::SECTION.fetch_add(1, Ordering::Relaxed);
                Ok(Mapping { inner: MappingInner::Owned(inner), offset: 0, len })
            }
            Descriptor::SharedFile { path, size } => {
                let len = usize::try_from(*size).map_err(|_| {
                    io::Error::new(io::ErrorKind::InvalidInput, "shared file too large")
                })?;
                let inner = platform::map_file(path, len)?;
                counters::SHARED_FILE.fetch_add(1, Ordering::Relaxed);
                Ok(Mapping { inner: MappingInner::Owned(inner), offset: 0, len })
            }
            Descriptor::ArenaSlice { arena, offset, len } => {
                let mapping = arena::window(*arena, *offset, *len)?;
                counters::ARENA_SLICE.fetch_add(1, Ordering::Relaxed);
                Ok(mapping)
            }
            other => Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{other:?} is not a mappable object"),
            )),
        }
    }

    /// Map the object this handle owns into the address space, **writable**,
    /// and keep the mapping.
    ///
    /// # Why the creator only
    ///
    /// The rule is unchanged from [`write_at`](Handle::write_at): a handle
    /// decoded from the wire owns nothing and gets no write path. That is what
    /// keeps a compositor's view of a client's buffer read-only, which is the
    /// correct default and is what `vendor/smithay`'s `shm/pool.rs` relies on
    /// (docs/waycompose-design.md §7.4). This is the *producer's* side of the same
    /// section: a client drawing frames needs one persistent view, because
    /// mapping and unmapping per damaged row would cost more than the drawing.
    ///
    /// # Not available for a gift, and that is deliberate — STAGE 10
    ///
    /// An arena gift has no `map_mut`, and refusing it is not an omission. A
    /// `MappingMut` is a writable view that outlives the call and hands out
    /// `&mut [u8]`; over arena memory that is a lasting, aliasable write
    /// capability into a region the peer is copying pixels through, held by the
    /// side of the boundary that is supposed to be reading. The one thing a
    /// server-created object actually needs is to be filled in once, and
    /// [`write_at`](Handle::write_at) does that with a bounded copy and no
    /// lasting reference. A caller that genuinely needs a persistent writable
    /// view of shared memory is a *producer* of frames, which on this side of
    /// the boundary nothing is.
    pub fn map_mut(&self) -> io::Result<MappingMut> {
        let Some(owned) = self.keepalive.as_ref() else {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "not the owner of this object",
            ));
        };
        let Owned::Object(object) = &**owned else {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "an arena gift has no writable mapping: use write_at, which is bounded by the \
                 gift and leaves no reference into the region behind it",
            ));
        };
        let size = self.owned_object_size()?;
        let len = usize::try_from(size)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "section too large"))?;
        platform::map_section_mut(object, len).map(|inner| MappingMut { inner, len })
    }

    /// The size of the OS object this handle *created*, for the two writer
    /// paths.
    ///
    /// `Section` and `SharedFile` differ in how the peer names the object and
    /// not at all in what the creator holds, so both are writable by their
    /// creator. `ArenaSlice` is not, and is not reached here: an arena slice has
    /// no OS object behind it, and how much of it may be written is the *gift's*
    /// business rather than the descriptor's — `write_at` takes the `Owned::Gift`
    /// arm before it ever asks this. A slice that arrived from the wire has no
    /// gift and therefore still lands here, and is still refused.
    fn owned_object_size(&self) -> io::Result<u64> {
        match &self.descriptor {
            Descriptor::Section { size, .. } | Descriptor::SharedFile { size, .. } => Ok(*size),
            other => Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{other:?} is not an object this process created"),
            )),
        }
    }

    /// Write into the object this handle owns.
    ///
    /// Only available to the creator: a handle decoded from the wire has no
    /// write path, which is the right default for a compositor reading client
    /// buffers.
    pub fn write_at(&self, offset: u64, bytes: &[u8]) -> io::Result<()> {
        let Some(owned) = self.keepalive.as_ref() else {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "not the owner of this object",
            ));
        };
        match &**owned {
            // §81: a stream has no offsets. Writing "at" one is a category error
            // rather than a bounds failure, and the caller wants `pipe_end()`.
            Owned::Pipe(_) => Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "this handle is a stream, not a mapping: write to `pipe_end()` instead",
            )),
            Owned::Object(object) => {
                let size = &self.owned_object_size()?;
                let end = offset
                    .checked_add(bytes.len() as u64)
                    .ok_or_else(|| {
                        io::Error::new(io::ErrorKind::InvalidInput, "write overflows")
                    })?;
                if end > *size {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "write past end of section",
                    ));
                }
                platform::write_section(object, offset as usize, bytes)
            }
            // The bound is the gift's own extent, chosen by the allocator, so
            // no argument to this function reaches another gift or a client's
            // pool. See `arena::Gift::write_at`.
            Owned::Gift(gift) => gift.write_at(offset, bytes),
        }
    }
}

/// A writable view of a shared-memory object this process created.
///
/// See [`Handle::map_mut`]. There is no `Sync` here and there deliberately is
/// on [`Mapping`]: a read-only view can be shared freely, a writable one is
/// exclusive to whoever holds the `&mut`.
pub struct MappingMut {
    inner: platform::WritableView,
    len: usize,
}

impl MappingMut {
    /// The mapped bytes, for writing.
    pub fn as_mut_slice(&mut self) -> &mut [u8] {
        // SAFETY: `inner` holds a live writable mapping of `len` bytes for as
        // long as `self` lives, and `&mut self` means no other reference into
        // it exists in this process.
        //
        // The peer may read these bytes concurrently. That is the entire point
        // of a shared buffer and is exactly the race `wl_buffer.release`
        // exists to arbitrate: a well-behaved client writes only into a buffer
        // the compositor has released.
        unsafe { std::slice::from_raw_parts_mut(self.inner.ptr_mut(), self.len) }
    }

    /// The same bytes, for reading, without giving up the writable mapping.
    ///
    /// ADDED FOR STAGE 10: the proxy holds one writable view of an arena and
    /// now has to *read* a range of it that the compositor wrote — the outbound
    /// window. Taking `&self` rather than `&mut self` is the whole of the
    /// difference and it is what lets the read happen while the write path is
    /// borrowed elsewhere.
    pub fn as_slice(&self) -> &[u8] {
        // SAFETY: `inner` holds a live mapping of `len` bytes for as long as
        // `self` lives; `&self` rules out a `&mut [u8]` from `as_mut_slice`
        // existing at the same time. The peer may still write these bytes,
        // which is the shared-memory hazard `Mapping::as_slice` documents.
        unsafe { std::slice::from_raw_parts(self.inner.ptr(), self.len) }
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
}

impl fmt::Debug for MappingMut {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MappingMut").field("len", &self.len).finish()
    }
}

/// What a [`Mapping`] is a view of.
///
/// Two cases, and the second is why this enum exists rather than a bare
/// `MappedView`. A `Section` or a `SharedFile` is mapped for this one handle
/// and unmapped when the handle's mapping is dropped. An `ArenaSlice` is a
/// *window* into a region that was mapped once, before any Wayland traffic, and
/// that outlives every pool cut from it — so the window must keep the region
/// alive without owning it, and must not unmap on drop.
enum MappingInner {
    Owned(platform::MappedView),
    /// A window into a region held by the [`arena`] registry.
    Window(Arc<platform::MappedView>),
}

impl MappingInner {
    fn ptr(&self) -> *const u8 {
        match self {
            MappingInner::Owned(v) => v.ptr(),
            MappingInner::Window(v) => v.ptr(),
        }
    }
}

/// A read-only view of a shared-memory object, or of a window into one.
pub struct Mapping {
    inner: MappingInner,
    /// Bytes from the start of `inner` at which this view begins. Always 0 for
    /// an owned mapping; the slice offset for an arena window.
    offset: usize,
    len: usize,
}

impl Mapping {
    /// The mapped bytes.
    pub fn as_slice(&self) -> &[u8] {
        // SAFETY: `inner` holds a live mapping of at least `len` bytes for as
        // long as `self` lives, and the mapping is read-only, so no other
        // reference can be creating a `&mut` to the same bytes through us.
        //
        // The honest caveat, which is a real one and is a `wl_shm` problem
        // rather than a soundness fiction: the *peer* can write these bytes at
        // any time, so this is a shared-memory read and not an exclusive
        // borrow. On Linux the analogous hazard is a client shrinking a pool
        // with `ftruncate` and faulting the compositor, which is why libwayland
        // installs a `SIGBUS` handler. On Windows a section cannot shrink, so
        // that entire attack class does not exist here — a genuine, if small,
        // way in which the Windows port is safer than the original.
        //
        // For an arena window `offset` was bounds-checked against the region's
        // length by `arena::window` before this value was constructed, so
        // `ptr + offset .. + len` is inside the one mapping the `Arc` keeps
        // alive.
        unsafe { std::slice::from_raw_parts(self.inner.ptr().add(self.offset), self.len) }
    }

    /// Length of the mapping in bytes.
    pub fn len(&self) -> usize {
        self.len
    }

    /// Whether the mapping is empty.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
}

impl fmt::Debug for Mapping {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Mapping").field("len", &self.len).finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn section_descriptor_round_trips() {
        let d = Descriptor::Section { name: "Local\\wc-0123456789abcdef".into(), size: 4096 };
        assert_eq!(Descriptor::decode(&d.encode()).unwrap(), d);
    }

    #[test]
    fn every_kind_round_trips() {
        for d in [
            Descriptor::Section { name: "s".into(), size: u64::MAX },
            Descriptor::Pipe { name: "p".into(), peer_reads: true },
            Descriptor::Pipe { name: String::new(), peer_reads: false },
            Descriptor::NtHandle { value: 0xdead_beef_dead_beef, object_kind: 7 },
            Descriptor::ArenaSlice { arena: 3, offset: 1 << 40, len: 64 << 20 },
        ] {
            assert_eq!(Descriptor::decode(&d.encode()).unwrap(), d, "round trip failed for {d:?}");
        }
    }

    #[test]
    fn truncated_descriptors_are_rejected_rather_than_panicking() {
        let full = Descriptor::Section { name: "abcdef".into(), size: 4096 }.encode();
        for n in 0..full.len() {
            assert!(
                Descriptor::decode(&full[..n]).is_err(),
                "prefix of length {n} decoded when it should not have"
            );
        }
    }

    #[test]
    fn trailing_bytes_are_rejected() {
        let mut bytes = Descriptor::Section { name: "a".into(), size: 1 }.encode();
        bytes.push(0);
        assert!(Descriptor::decode(&bytes).is_err());
    }

    #[test]
    fn unknown_kind_is_rejected() {
        assert!(Descriptor::decode(&0xffff_ffffu32.to_le_bytes()).is_err());
    }

    #[test]
    fn absurd_name_length_is_rejected_without_allocating() {
        // kind = Section, size = 0, then a name length of 4 GiB.
        let mut bytes = KIND_SECTION.to_le_bytes().to_vec();
        bytes.extend_from_slice(&0u64.to_le_bytes());
        bytes.extend_from_slice(&u32::MAX.to_le_bytes());
        assert!(Descriptor::decode(&bytes).is_err());
    }

    #[test]
    fn non_utf8_name_is_rejected() {
        let mut bytes = KIND_SECTION.to_le_bytes().to_vec();
        bytes.extend_from_slice(&0u64.to_le_bytes());
        bytes.extend_from_slice(&2u32.to_le_bytes());
        bytes.extend_from_slice(&[0xff, 0xfe]);
        assert!(Descriptor::decode(&bytes).is_err());
    }

    /// The end-to-end claim this whole crate exists to support: a descriptor is
    /// enough. One party creates an object and writes to it, only the encoded
    /// descriptor bytes cross, and the other party reads back what was written
    /// having been told nothing else.
    #[test]
    fn a_descriptor_alone_carries_the_memory() {
        let owner = Handle::create_section(4096).unwrap();
        owner.write_at(0, b"the quick brown fox").unwrap();
        owner.write_at(4000, b"tail").unwrap();

        // Only these bytes cross. Everything else about `owner` is dropped on
        // the floor to make sure the peer is not secretly using it.
        let wire = owner.descriptor().encode();

        let peer = Handle::from_descriptor(Descriptor::decode(&wire).unwrap());
        assert!(!peer.is_owner());
        let map = peer.map().unwrap();
        assert_eq!(map.len(), 4096);
        assert_eq!(&map.as_slice()[..19], b"the quick brown fox");
        assert_eq!(&map.as_slice()[4000..4004], b"tail");
    }

    #[test]
    fn two_sections_never_collide() {
        let a = Handle::create_section(64).unwrap();
        let b = Handle::create_section(64).unwrap();
        assert_ne!(a.descriptor(), b.descriptor());
    }

    #[test]
    fn a_decoded_handle_cannot_write() {
        let owner = Handle::create_section(64).unwrap();
        let peer = Handle::from_descriptor(owner.descriptor().clone());
        assert_eq!(
            peer.write_at(0, b"x").unwrap_err().kind(),
            io::ErrorKind::PermissionDenied
        );
    }

    #[test]
    fn a_decoded_handle_cannot_map_writable() {
        // The same rule as `write_at`, and the one that keeps a compositor's
        // view of a client buffer read-only.
        let owner = Handle::create_section(64).unwrap();
        let peer = Handle::from_descriptor(owner.descriptor().clone());
        assert_eq!(
            peer.map_mut().unwrap_err().kind(),
            io::ErrorKind::PermissionDenied
        );
    }

    #[test]
    fn a_writable_mapping_is_seen_through_a_separate_read_only_one() {
        // The contract that makes `map_mut` worth having: the write must land
        // in the *shared* object, not in a private copy. On Unix that is the
        // difference between `MAP_SHARED` and `MAP_PRIVATE`, and `MAP_PRIVATE`
        // would pass every test that only read back through the same mapping —
        // so this deliberately reads through a different one, opened by name,
        // exactly as the peer process does.
        let owner = Handle::create_section(4096).unwrap();
        {
            let mut view = owner.map_mut().unwrap();
            let bytes = view.as_mut_slice();
            assert_eq!(bytes.len(), 4096);
            bytes[0..4].copy_from_slice(b"HEAD");
            bytes[4092..4096].copy_from_slice(b"TAIL");
        }
        let reader = Handle::from_descriptor(owner.descriptor().clone());
        let mapping = reader.map().unwrap();
        // Two offsets, as in the stage-1 shm probe: a wrong-length or zeroed
        // mapping would still produce the first one by accident.
        assert_eq!(&mapping.as_slice()[0..4], b"HEAD");
        assert_eq!(&mapping.as_slice()[4092..4096], b"TAIL");
    }

    #[test]
    fn writes_past_the_end_are_refused() {
        let owner = Handle::create_section(64).unwrap();
        assert!(owner.write_at(60, b"12345").is_err());
        assert!(owner.write_at(u64::MAX, b"x").is_err());
    }

    #[test]
    fn non_memory_descriptors_do_not_map() {
        let h = Handle::from_descriptor(Descriptor::Pipe { name: "p".into(), peer_reads: true });
        assert!(h.map().is_err());
    }
}

