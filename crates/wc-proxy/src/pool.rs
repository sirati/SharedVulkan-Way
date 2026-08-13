//! Where a `wl_shm` pool's bytes go so that the compositor can read them
//! **without anything being passed**.
//!
//! # The rule, restated because it is the whole design
//!
//! No file descriptor and no handle crosses the boundary, ever, even when both
//! sides happen to be on one machine. A descriptor created locally out of
//! memory that side already reaches is fine; a descriptor *passed* is not. A
//! name crossing, which the peer then opens itself, is the sanctioned pattern.
//!
//! The client's pool fd is created by the client and consumed by this process.
//! It never goes anywhere. What crosses is one of the two things a
//! [`PoolTransport`] can produce, and neither is a descriptor:
//!
//! | transport | what crosses | what the peer does with it |
//! |---|---|---|
//! | [`SharedFile`] | a path | opens it, maps it |
//! | [`Arena`] | `(arena, offset, len)` | arithmetic; it mapped the region before any traffic |
//!
//! # Why the pool cannot be arena-backed directly, which is the question the
//! brief asked
//!
//! It would be better if the client's pool *were* the shared region, because
//! then a commit would cost nothing at all. It cannot be, and the reason is
//! structural rather than an implementation gap:
//!
//! **`wl_shm.create_pool` is the client handing the server memory it has
//! already allocated.** The protocol has no request in the other direction — no
//! way for a server to say "allocate your next pool here". The client calls
//! `memfd_create` (or `shm_open`, or `mkstemp`) inside libwayland, sizes it,
//! maps it, draws into it, and only then tells us it exists. By the time this
//! proxy learns of a pool, the pages are chosen. Nothing on Linux can re-point
//! an existing mapping at another object's pages, so there is no trick
//! available: the bytes must be copied at least once on this side of the
//! boundary.
//!
//! The honest way to state it: **a copy per frame is the price of the client
//! being unmodified**, and it is a price paid on the host, which has spare CPU,
//! rather than in the guest, which has the GPU. An arena-backed pool is
//! reachable only by a client that allocates from the arena — which is what
//! `docs/goal-firefox.md`'s accelerated path is, where Firefox's WebRender
//! renders through shared_vulkan into arena memory and no `wl_shm` pool exists
//! at all. That is why this seam matters more than the copy it currently
//! performs.
//!
//! So the measurable question is not *whether* to copy but *how much*, which is
//! [`CopyScope`], and *into what*, which is the two transports.

use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};

use wc_handle::{Descriptor, Handle, MappingMut};

/// How much of a pool is copied when a surface commits.
///
/// All three are correct; they differ in how much they copy and in how much
/// they have to understand to do it. Measured in `docs/design.md` §9.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CopyScope {
    /// The whole pool, every commit. Understands nothing beyond "this surface's
    /// buffer lives in that pool" — the simplest correct proxy there is, and
    /// the one whose cost is set by the client's allocation habits rather than
    /// by what it drew.
    Pool,
    /// The attached buffer's extent within the pool. Needs `create_buffer`'s
    /// offset, stride and height.
    Buffer,
    /// Only the rows the client damaged, within the attached buffer. Needs all
    /// of the above plus `wl_surface.damage`/`damage_buffer` accumulation.
    Damage,
}

impl CopyScope {
    pub fn parse(s: &str) -> Option<CopyScope> {
        match s {
            "pool" => Some(CopyScope::Pool),
            "buffer" => Some(CopyScope::Buffer),
            "damage" => Some(CopyScope::Damage),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            CopyScope::Pool => "pool",
            CopyScope::Buffer => "buffer",
            CopyScope::Damage => "damage",
        }
    }
}

/// A published region: what the peer will be told, and the key this side uses
/// to find the memory again.
#[derive(Clone, Debug)]
pub struct Region {
    /// Exactly the bytes that cross. Never contains a descriptor of any kind
    /// the peer would have to be *given* rather than open.
    pub descriptor: Descriptor,
    pub size: u64,
    key: u64,
}

impl Region {
    /// A one-line description for the log, so that a run says which mechanism
    /// carried its pixels rather than leaving it to be inferred.
    pub fn describe(&self) -> String {
        match &self.descriptor {
            Descriptor::SharedFile { path, size } => format!("shared-file {path} ({size} bytes)"),
            Descriptor::ArenaSlice { arena, offset, len } => {
                format!("arena {arena} slice {offset}+{len}")
            }
            other => format!("{other:?}"),
        }
    }
}

/// Somewhere a pool's bytes can live such that only a name or an offset
/// crosses.
pub trait PoolTransport {
    /// For the log and for the harness's "which transport served this" check.
    fn name(&self) -> &'static str;

    /// Publish `size` bytes. Fails rather than degrading: a transport that
    /// quietly fell back to another would defeat the point of naming which one
    /// ran.
    fn create(&mut self, size: u64) -> io::Result<Region>;

    /// Stop publishing. The bytes may be reused by a later [`create`].
    ///
    /// [`create`]: PoolTransport::create
    fn destroy(&mut self, region: &Region);

    /// The region's bytes, for writing.
    ///
    /// One virtual call per commit rather than one per row: the copy loop above
    /// this is the hot path and the transport is not.
    fn bytes_mut(&mut self, region: &Region) -> Option<&mut [u8]>;

    /// Bytes currently published, for the log.
    fn published(&self) -> u64;
}

// ---------------------------------------------------------------------------
// SharedFile: one object per pool, named by path
// ---------------------------------------------------------------------------

/// One file per pool, in a directory both sides can reach.
///
/// The straightforward transport, and the one that works under Wine with no
/// hypervisor in the picture. Its cost is an object creation per pool — a
/// `open(O_CREAT)`, an `ftruncate` and an `mmap` here, and a `CreateFileW` plus
/// `CreateFileMappingW` plus `MapViewOfFile` there — which is cheap on a local
/// filesystem and is exactly what is *not* cheap across a hypervisor, where the
/// peer's open is a round trip through a filesystem daemon. That is the
/// measurement that decides between the two, and it is in `docs/design.md` §9.
pub struct SharedFile {
    dir: PathBuf,
    /// How the peer spells `dir`. `Z:\tmp\wc-pools` for Wine; a drive letter on
    /// the guest's virtio-fs mount for the real thing. There is no way to
    /// derive this and a proxy that guessed would be wrong on the deployment
    /// that matters, so it is required rather than defaulted.
    peer_prefix: String,
    next: u64,
    live: HashMap<u64, Live>,
    published: u64,
}

struct Live {
    /// Holds the object open. docs/design.md §2.5's lifetime rule: the creator
    /// must outlive the peer's use of the name, and for a file that is also
    /// what keeps it off the filesystem afterwards, since the `Handle`'s drop
    /// unlinks it.
    _handle: Handle,
    view: MappingMut,
    size: u64,
}

impl SharedFile {
    pub fn new(dir: &Path, peer_prefix: &str) -> io::Result<SharedFile> {
        std::fs::create_dir_all(dir)?;
        // The pool files are readable by their path, so the directory is the
        // only access control there is. 0700 is what an `XDG_RUNTIME_DIR` uses
        // and is the same standard the compositor's own socket is held to.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
        }
        Ok(SharedFile {
            dir: dir.to_path_buf(),
            peer_prefix: peer_prefix.to_owned(),
            next: 0,
            live: HashMap::new(),
            published: 0,
        })
    }
}

impl PoolTransport for SharedFile {
    fn name(&self) -> &'static str {
        "shared-file"
    }

    fn create(&mut self, size: u64) -> io::Result<Region> {
        if size == 0 {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "a pool cannot be empty"));
        }
        let key = self.next;
        self.next += 1;
        // 128 bits of randomness in the name, for the same reason
        // `wc_handle`'s section names carry it: the name is the only thing
        // between this pool and any other process that can see the directory.
        let stem = format!("wc-pool-{key:04x}-{}", random_hex());
        let local = self.dir.join(&stem);
        let local_str = local.to_string_lossy().into_owned();
        let peer = format!("{}{}", self.peer_prefix, stem);
        let handle = Handle::create_shared_file(&local_str, &peer, size)?;
        let view = handle.map_mut()?;
        self.published += size;
        self.live.insert(key, Live { _handle: handle, view, size });
        Ok(Region { descriptor: Descriptor::SharedFile { path: peer, size }, size, key })
    }

    fn destroy(&mut self, region: &Region) {
        if let Some(live) = self.live.remove(&region.key) {
            self.published -= live.size;
        }
    }

    fn bytes_mut(&mut self, region: &Region) -> Option<&mut [u8]> {
        Some(self.live.get_mut(&region.key)?.view.as_mut_slice())
    }

    fn published(&self) -> u64 {
        self.published
    }
}

// ---------------------------------------------------------------------------
// Arena: one region for everything, sliced
// ---------------------------------------------------------------------------

/// Every pool is a slice of one region both sides mapped once, before any
/// Wayland traffic.
///
/// This is `Descriptor::ArenaSlice`, reserved in the type since stage 1
/// (docs/design.md §2.2 (c)) and constructed here for the first time. In the
/// real deployment the region is shared_vulkan's passthrough arena — guest RAM
/// the host found by walking QEMU's flat view (its design.md §5.1) — and this
/// process is handed a range of it. In the same-machine test the region is one
/// file, attached by both sides from a flag.
///
/// **What is identical between those two cases is the part that matters**: no
/// object is created per pool, nothing is opened per pool, and the descriptor
/// is three integers. What differs is only who provided the pages, which is
/// exactly the seam the two projects meet at.
///
/// # The allocator
///
/// First fit over a sorted free list, coalescing on release, 4096-byte
/// granularity. The granularity is not arbitrary: it is `sv_arena::GRANULARITY`
/// (shared_vulkan design.md §5.2), which is the measured
/// `minImportedHostPointerAlignment` on the target *and* the page granularity
/// of the shared mapping. A pool that begins on a page boundary is also a pool
/// whose peer-side mapping can be a whole number of pages, which is what makes
/// the peer's side of a slice free rather than merely cheap.
///
/// First fit rather than anything cleverer because a `wl_shm` client holds two
/// or three pools at a time. If that changes the allocator is 60 lines and it
/// is tested; replacing it does not touch the wire.
/// Where an [`Arena`]'s bytes actually are.
///
/// STAGE 7 SPLIT THIS IN TWO, and docs/design.md §12 is why. Everything above
/// this enum — the allocator, the free list, the descriptor, the compositor's
/// side of it — is identical for both, which is exactly the claim §9.10 item 3
/// made and this stage had to cash: "substituting the real provider changes
/// `Arena::open_or_create` and nothing above it."
enum Backing {
    /// A file both sides open. The same-machine test shape, and — measured in
    /// §10.3 and §10.7 — **not a usable live surface** on a virtio-fs export,
    /// because two live mappings of one file on that mount do not see each
    /// other's stores.
    File {
        /// Held for its lifetime rule, not for its bytes.
        _handle: Handle,
        view: MappingMut,
    },
    /// The guest's own RAM, found in QEMU's memory backend and stitched into
    /// one view. Not a file, and therefore not subject to a mount's caching.
    GuestRam(crate::guestram::StitchedRegion),
}

impl Backing {
    fn as_mut_slice(&mut self) -> &mut [u8] {
        match self {
            Backing::File { view, .. } => view.as_mut_slice(),
            Backing::GuestRam(r) => r.as_mut_slice(),
        }
    }
}

pub struct Arena {
    id: u32,
    backing: Backing,
    /// How the region was obtained, for the one log line that says it.
    origin: String,
    free: Vec<(u64, u64)>,
    live: HashMap<u64, (u64, u64)>,
    next: u64,
    published: u64,
    size: u64,
}

/// The allocator's granularity, and the alignment of every offset it returns.
/// See the note on [`Arena`].
pub const ARENA_GRANULARITY: u64 = 4096;

impl Arena {
    /// Take the region at `local_path` as arena `id`, creating it only if it is
    /// not already there.
    ///
    /// **Open-or-create rather than create**, and that is not a convenience.
    /// An arena is provided *out of band*, before either side starts, by
    /// whatever brought the two sides together — shared_vulkan's cold-path
    /// handshake in the real deployment, a harness here. Both the compositor
    /// and this process attach to the same region, and whichever starts first
    /// must not be able to pull it out from under the other. An earlier version
    /// unlinked and re-created it, and the compositor mapped a file that no
    /// longer had a name: it read zeroes forever and reported nothing wrong.
    ///
    /// The peer is told `id` and nothing else about it. Describing the arena in
    /// the Wayland stream instead would mean the first pool of the first client
    /// decided where every later one lived.
    pub fn open_or_create(id: u32, local_path: &Path, size: u64) -> io::Result<Arena> {
        let size = size.div_ceil(ARENA_GRANULARITY) * ARENA_GRANULARITY;
        if let Some(parent) = local_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let path = local_path.to_string_lossy().into_owned();
        // The peer path is unused: an arena slice names no path at all. It is
        // passed as the local one so the descriptor the `Handle` carries is at
        // least truthful about the object this process holds.
        let handle = if local_path.exists() {
            Handle::open_shared_file(&path, &path, size)?
        } else {
            Handle::create_shared_file(&path, &path, size)?
        };
        let view = handle.map_mut()?;
        Ok(Arena::over(id, Backing::File { _handle: handle, view }, &path, size))
    }

    /// Take the region `wc-arena-host` located in the guest as arena `id`.
    ///
    /// STAGE 7, and the whole of §10.13 item 1 on this side. The plan names an
    /// object and a list of runs; [`crate::guestram`] opens the one and maps the
    /// others. Nothing about the *arena* changes — the allocator below is the
    /// same code, the descriptor is the same three integers, and the compositor
    /// cannot tell which of these two constructors ran.
    ///
    /// # The size comes from the plan, not from a flag
    ///
    /// `--arena-size` is meaningless here: the guest allocated the region and
    /// its length is a fact about what it allocated. A flag that disagreed
    /// would either truncate the arena — leaving guest pages nobody uses — or
    /// run off the end of it, and the second is a fault in the proxy on the
    /// first large client. Taking it from the plan removes the disagreement.
    pub fn from_plan(id: u32, plan_path: &Path) -> io::Result<Arena> {
        let plan = crate::guestram::Plan::read(plan_path)?;
        let size = plan.len;
        if size % ARENA_GRANULARITY != 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("a {size}-byte arena is not a whole number of {ARENA_GRANULARITY}-byte pages"),
            ));
        }
        let region = crate::guestram::StitchedRegion::attach(&plan)?;
        let origin = format!(
            "guest RAM, run 0x{:016x}, {} run(s) of {}",
            plan.run_id,
            region.runs(),
            region.origin()
        );
        Ok(Arena::over(id, Backing::GuestRam(region), &origin, size))
    }

    fn over(id: u32, backing: Backing, origin: &str, size: u64) -> Arena {
        Arena {
            id,
            backing,
            origin: origin.to_owned(),
            free: vec![(0, size)],
            live: HashMap::new(),
            next: 0,
            published: 0,
            size,
        }
    }

    pub fn id(&self) -> u32 {
        self.id
    }

    /// Where the region came from, for the log line that is the only difference
    /// a reader of the output can see between the two substrates.
    pub fn origin(&self) -> &str {
        &self.origin
    }

    pub fn size(&self) -> u64 {
        self.size
    }

    /// Free bytes, and the largest contiguous run of them. The second number is
    /// the one that matters: an arena with room for a pool and nowhere to put
    /// it is the fragmentation failure shared_vulkan's design.md §5.7 is about,
    /// and reporting only the total would hide it.
    pub fn free_bytes(&self) -> (u64, u64) {
        (
            self.free.iter().map(|(_, l)| *l).sum(),
            self.free.iter().map(|(_, l)| *l).max().unwrap_or(0),
        )
    }

    fn release(&mut self, offset: u64, len: u64) {
        let at = self.free.partition_point(|(o, _)| *o < offset);
        self.free.insert(at, (offset, len));
        // Coalesce with the neighbour on each side. Done here rather than at
        // allocation time so that the free list is always in its canonical
        // form, which is what makes `free_bytes`'s second number meaningful.
        let mut i = 0;
        while i + 1 < self.free.len() {
            let (o, l) = self.free[i];
            let (o2, l2) = self.free[i + 1];
            if o + l == o2 {
                self.free[i] = (o, l + l2);
                self.free.remove(i + 1);
            } else {
                i += 1;
            }
        }
    }
}

impl PoolTransport for Arena {
    fn name(&self) -> &'static str {
        "arena"
    }

    fn create(&mut self, size: u64) -> io::Result<Region> {
        if size == 0 {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "a pool cannot be empty"));
        }
        let want = size.div_ceil(ARENA_GRANULARITY) * ARENA_GRANULARITY;
        let idx = self.free.iter().position(|(_, l)| *l >= want).ok_or_else(|| {
            let (total, largest) = self.free_bytes();
            io::Error::new(
                io::ErrorKind::OutOfMemory,
                format!(
                    "arena {} has no run of {want} bytes ({total} free, largest {largest}); \
                     raise --arena-size",
                    self.id
                ),
            )
        })?;
        let (offset, len) = self.free[idx];
        if len == want {
            self.free.remove(idx);
        } else {
            self.free[idx] = (offset + want, len - want);
        }
        let key = self.next;
        self.next += 1;
        self.live.insert(key, (offset, want));
        self.published += want;
        Ok(Region {
            descriptor: Descriptor::ArenaSlice { arena: self.id, offset, len: size },
            size,
            key,
        })
    }

    fn destroy(&mut self, region: &Region) {
        if let Some((offset, len)) = self.live.remove(&region.key) {
            self.published -= len;
            self.release(offset, len);
        }
    }

    fn bytes_mut(&mut self, region: &Region) -> Option<&mut [u8]> {
        let (offset, _) = *self.live.get(&region.key)?;
        let start = usize::try_from(offset).ok()?;
        let len = usize::try_from(region.size).ok()?;
        self.backing.as_mut_slice().get_mut(start..start + len)
    }

    fn published(&self) -> u64 {
        self.published
    }
}

/// 128 bits from `/dev/urandom`, hex encoded.
fn random_hex() -> String {
    use std::io::Read as _;
    let mut bytes = [0u8; 16];
    if std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut bytes))
        .is_err()
    {
        // Greppable, exactly as `wc_handle`'s degraded name is: refusing to run
        // would be worse, and a caller must be able to tell.
        return format!("weak-{:x}", std::process::id());
    }
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("wc-proxy-test-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    /// The claim both transports must satisfy: bytes written here are readable
    /// by a peer that was told **only the descriptor**. Read back through
    /// `wc_handle::Handle::map`, which is the compositor's own path.
    fn round_trip(t: &mut dyn PoolTransport) {
        let region = t.create(8192).unwrap();
        {
            let bytes = t.bytes_mut(&region).unwrap();
            assert_eq!(bytes.len(), 8192);
            bytes[0..4].copy_from_slice(b"HEAD");
            bytes[8188..8192].copy_from_slice(b"TAIL");
        }
        // Only these bytes cross.
        let wire = region.descriptor.encode();
        let peer = Handle::from_descriptor(Descriptor::decode(&wire).unwrap());
        let map = peer.map().unwrap();
        assert_eq!(map.len(), 8192, "{}", t.name());
        assert_eq!(&map.as_slice()[0..4], b"HEAD", "{}", t.name());
        assert_eq!(&map.as_slice()[8188..8192], b"TAIL", "{}", t.name());
    }

    #[test]
    fn a_shared_file_region_is_readable_from_its_descriptor_alone() {
        let d = tmpdir("sf");
        // The peer prefix is how the *peer* spells the directory, and the peer
        // in this test is this process, so it is the local path. In a real run
        // it is `Z:\tmp\wc-pools\` or a virtio-fs drive letter, and the whole
        // point of the parameter is that nothing can derive one from the other.
        let mut t = SharedFile::new(&d, &format!("{}/", d.display())).unwrap();
        round_trip(&mut t);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn an_arena_slice_is_readable_from_its_descriptor_alone() {
        let d = tmpdir("ar");
        std::fs::create_dir_all(&d).unwrap();
        let mut t = Arena::open_or_create(7100, &d.join("arena"), 1 << 20).unwrap();
        wc_handle::arena::attach(7100, &d.join("arena").to_string_lossy(), 1 << 20).unwrap();
        round_trip(&mut t);
        let _ = std::fs::remove_dir_all(&d);
    }

    /// Two pools must not overlap. The failure this rules out is the worst one
    /// available — two surfaces silently sharing bytes — and it would present
    /// as one window showing the other's contents.
    #[test]
    fn arena_slices_never_overlap() {
        let d = tmpdir("ar2");
        std::fs::create_dir_all(&d).unwrap();
        let mut a = Arena::open_or_create(7101, &d.join("arena"), 1 << 20).unwrap();
        let mut spans = Vec::new();
        for size in [4096u64, 100, 8192, 1, 65536, 4097] {
            let r = a.create(size).unwrap();
            let Descriptor::ArenaSlice { offset, len, .. } = r.descriptor else {
                panic!("wrong kind")
            };
            assert_eq!(offset % ARENA_GRANULARITY, 0, "offset {offset} is not page aligned");
            assert_eq!(len, size, "the descriptor must carry what the client asked for");
            spans.push((offset, size.div_ceil(ARENA_GRANULARITY) * ARENA_GRANULARITY));
        }
        spans.sort();
        for w in spans.windows(2) {
            assert!(w[0].0 + w[0].1 <= w[1].0, "{:?} overlaps {:?}", w[0], w[1]);
        }
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn freed_arena_space_is_reused_and_coalesced() {
        let d = tmpdir("ar3");
        std::fs::create_dir_all(&d).unwrap();
        let mut a = Arena::open_or_create(7102, &d.join("arena"), 64 * 4096).unwrap();
        let regions: Vec<_> = (0..8).map(|_| a.create(4096).unwrap()).collect();
        assert_eq!(a.published(), 8 * 4096);
        for r in &regions {
            a.destroy(r);
        }
        assert_eq!(a.published(), 0);
        // Coalesced back into one run, which is the property that lets a long
        // run of create/destroy cycles not fragment the arena to death.
        let (total, largest) = a.free_bytes();
        assert_eq!(total, 64 * 4096);
        assert_eq!(largest, 64 * 4096, "the free list did not coalesce");
        // And the whole thing is allocatable again.
        assert!(a.create(64 * 4096).is_ok());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn an_over_large_pool_is_a_named_error_rather_than_a_panic() {
        let d = tmpdir("ar4");
        std::fs::create_dir_all(&d).unwrap();
        let mut a = Arena::open_or_create(7103, &d.join("arena"), 16 * 4096).unwrap();
        let err = a.create(17 * 4096).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::OutOfMemory);
        assert!(err.to_string().contains("--arena-size"), "{err}");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn an_empty_pool_is_refused_by_both() {
        let d = tmpdir("z");
        std::fs::create_dir_all(&d).unwrap();
        assert!(SharedFile::new(&d, "Z:\\").unwrap().create(0).is_err());
        assert!(Arena::open_or_create(7104, &d.join("a"), 4096).unwrap().create(0).is_err());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn shared_file_names_do_not_repeat() {
        let d = tmpdir("sf2");
        let mut t = SharedFile::new(&d, "Z:\\tmp\\").unwrap();
        let a = t.create(4096).unwrap();
        let b = t.create(4096).unwrap();
        assert_ne!(a.descriptor, b.descriptor);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn copy_scope_parses_exactly_the_three() {
        for s in ["pool", "buffer", "damage"] {
            assert_eq!(CopyScope::parse(s).unwrap().as_str(), s);
        }
        assert!(CopyScope::parse("none").is_none());
    }
}
