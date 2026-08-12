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

use std::fmt;
use std::io;
use std::sync::Arc;

mod platform;

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
    /// A window into the phase-1 shared arena: `(arena, offset, len)`.
    ///
    /// Reserved, not yet constructed. This is the variant that makes phase 1
    /// load-bearing rather than incidental — a `wl_shm` pool that is already a
    /// slice of the arena needs no new kernel object at all, and a client
    /// rendering with the forwarded Vulkan has already put its pixels there. It
    /// is deliberately part of the type from the start so that adding it later
    /// is not a wire-format break.
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
    keepalive: Option<Arc<platform::OwnedObject>>,
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
            keepalive: Some(Arc::new(object)),
        })
    }

    /// Map this handle's object into the current address space, read-only.
    ///
    /// Fails for descriptor kinds that are not memory.
    pub fn map(&self) -> io::Result<Mapping> {
        match &self.descriptor {
            Descriptor::Section { name, size } => {
                let len = usize::try_from(*size)
                    .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "section too large"))?;
                platform::map_section(name, len).map(|inner| Mapping { inner, len })
            }
            other => Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{other:?} is not a mappable object"),
            )),
        }
    }

    /// Write into the object this handle owns.
    ///
    /// Only available to the creator: a handle decoded from the wire has no
    /// write path, which is the right default for a compositor reading client
    /// buffers and is why there is no `map_mut`.
    pub fn write_at(&self, offset: u64, bytes: &[u8]) -> io::Result<()> {
        let Some(object) = self.keepalive.as_ref() else {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "not the owner of this object",
            ));
        };
        let Descriptor::Section { size, .. } = &self.descriptor else {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "not a section"));
        };
        let end = offset
            .checked_add(bytes.len() as u64)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "write overflows"))?;
        if end > *size {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "write past end of section"));
        }
        platform::write_section(object, offset as usize, bytes)
    }
}

/// A read-only view of a shared-memory object.
pub struct Mapping {
    inner: platform::MappedView,
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
        unsafe { std::slice::from_raw_parts(self.inner.ptr(), self.len) }
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
