//! The Unix side of the handle shim.
//!
//! Unix has `SCM_RIGHTS` and does not need any of this to talk Wayland. It is
//! implemented anyway, for two reasons that are worth stating because otherwise
//! this file looks like dead code:
//!
//! 1. **It makes the shim testable on the machine that builds it.** The Windows
//!    guest is not reachable from here and Wine is not Windows, so a shim whose
//!    only proof was "it compiles" would be the weakest part of the project.
//! 2. **It is the Architecture B path.** If Linux clients ever reach a Windows
//!    compositor through a proxy, the proxy has to materialise a real Unix
//!    object from a descriptor that arrived from Windows, and this is that code.
//!
//! The Unix analogue of a named section is a **file at a path**, not a `memfd`.
//! That looks like a step down until you notice the contract: the peer opens the
//! object *by name*, and a `memfd` has no name to open. A file does.

use std::fs::{File, OpenOptions};
use std::io::{self, Read};
use std::os::fd::AsRawFd;
use std::os::unix::fs::FileExt;
use std::path::PathBuf;

/// The object, held open by its creator.
///
/// Dropping it unlinks the path as well as closing the file, so a crashed or
/// finished compositor does not leave shared-memory files behind. That mirrors
/// the Windows behaviour, where a section with no open handles simply ceases to
/// exist along with its name.
#[derive(Debug)]
pub struct OwnedObject {
    file: File,
    path: PathBuf,
    /// False for an object this process only *opened* — an arena somebody else
    /// provided. See `Handle::open_shared_file` for why the distinction is
    /// about who may delete it rather than about who may write to it.
    unlink_on_drop: bool,
}

impl Drop for OwnedObject {
    fn drop(&mut self) {
        if self.unlink_on_drop {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

/// A read-only mapping of an object.
#[derive(Debug)]
pub struct MappedView {
    ptr: *const u8,
    len: usize,
}

// SAFETY: the pointer is a private read-only mapping owned solely by this
// struct; sending it to another thread moves the whole mapping with it, and
// sharing it hands out `&[u8]` which is what a read-only shared mapping is.
unsafe impl Send for MappedView {}
unsafe impl Sync for MappedView {}

impl MappedView {
    pub fn ptr(&self) -> *const u8 {
        self.ptr
    }

    /// §110: a view of memory somebody else mapped and keeps mapped for the
    /// life of the process (the GPU device's BAR in the shared compositor).
    /// Its length is 0, so dropping it unmaps nothing.
    pub(crate) fn borrowed(ptr: *const u8) -> MappedView {
        MappedView { ptr, len: 0 }
    }
}

impl Drop for MappedView {
    fn drop(&mut self) {
        if self.len > 0 {
            // SAFETY: `ptr`/`len` are exactly what `mmap` returned and this
            // runs once, in `Drop`.
            unsafe {
                libc::munmap(self.ptr as *mut libc::c_void, self.len);
            }
        }
    }
}

/// 128 bits from the operating system, hex encoded.
///
/// `/dev/urandom` rather than a hash of the clock and the pid: the name is the
/// only thing standing between a section and any other process on the machine,
/// so it has to be unguessable rather than merely unique. See the security note
/// in the crate docs.
pub fn random_section_name() -> String {
    format!("{}/{}", std::env::temp_dir().display(), random_token())
}

/// The unguessable part of a name, without deciding what namespace it goes in.
///
/// Split out of [`random_section_name`] for `crate::publish`, which mints file
/// names in a directory the caller chose rather than in the platform's default
/// one. The randomness requirement is identical and so is the degraded path;
/// only the prefix differs, and having one source of the bytes means a future
/// change to how they are drawn cannot apply to one caller and not the other.
pub fn random_token() -> String {
    let mut bytes = [0u8; 16];
    if File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut bytes))
        .is_err()
    {
        // Refusing to run because /dev/urandom is missing would be worse than
        // the degraded name, but a caller must be able to tell. The marker in
        // the name is deliberate and greppable.
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        bytes[..16].copy_from_slice(&now.to_le_bytes());
        return format!("wc-weak-{}", hex(&bytes));
    }
    format!("wc-{}", hex(&bytes))
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(s, "{b:02x}");
    }
    s
}

/// Create the object. `name` is a filesystem path on this platform.
pub fn create_section(name: &str, size: u64) -> io::Result<OwnedObject> {
    let path = PathBuf::from(name);
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true) // never adopt somebody else's file
        .open(&path)?;
    file.set_len(size)?;
    Ok(OwnedObject { file, path, unlink_on_drop: true })
}

/// Open an object somebody else created, for writing, without taking on the
/// job of deleting it. See `crate::Handle::open_shared_file`.
pub fn open_file_object(name: &str, size: u64) -> io::Result<OwnedObject> {
    let path = PathBuf::from(name);
    let file = OpenOptions::new().read(true).write(true).open(&path)?;
    let have = file.metadata()?.len();
    if have < size {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{name} is {have} bytes, not the {size} it was said to be"),
        ));
    }
    Ok(OwnedObject { file, path, unlink_on_drop: false })
}

/// Open the object by name and map it read-only.
pub fn map_section(name: &str, len: usize) -> io::Result<MappedView> {
    let file = File::open(name)?;
    if len == 0 {
        return Ok(MappedView { ptr: std::ptr::NonNull::<u8>::dangling().as_ptr(), len: 0 });
    }
    // SAFETY: a fresh anonymous-address request (`addr = null`) with a length
    // we checked is non-zero, against a file we just opened and hold open for
    // the duration of the call. `mmap` keeps its own reference to the
    // description, so dropping `file` afterwards is fine.
    let ptr = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            len,
            libc::PROT_READ,
            libc::MAP_SHARED,
            file.as_raw_fd(),
            0,
        )
    };
    if ptr == libc::MAP_FAILED {
        return Err(io::Error::last_os_error());
    }
    Ok(MappedView { ptr: ptr.cast::<u8>().cast_const(), len })
}

/// The granularity every stamp and every locate step works in. Not a host page
/// size: it is the *guest-physical* page size the host's scan sweeps in, and it
/// stays 4096 whatever the allocator did (shared_vulkan docs/waycompose-design.md §5.1).
pub const REGION_PAGE: usize = 4096;

/// What [`alloc_locked_region`] made. See the Windows twin.
pub struct LockedRegion {
    pub view: MappedView,
    pub len: usize,
    pub locked: bool,
    pub pinning: &'static str,
}

/// Commit at least `len` bytes of this process's own memory, pin them, and let
/// the caller stamp every 4096-byte page before the region is handed back
/// read-only.
///
/// STAGE 7. The Windows implementation is the shipping one — a guest-RAM arena
/// only means anything inside the VM — and this is its Unix twin, for the same
/// reason every other function in this file has one: it is what makes the path
/// testable on the machine that builds it, and it is what `wc-release-vm.sh`
/// runs inside a Linux guest.
///
/// §85: the same rule as on Windows. `mlock` is `VirtualLock`'s twin in the way
/// that matters: it keeps pages resident and Linux still migrates them
/// (compaction moves mlocked pages unless `vm.compact_unevictable_allowed` is 0),
/// so it is the diagnostic path, taken only when asked for by name. The default
/// is `MAP_HUGETLB`, from the pool `vm.nr_hugepages` reserves: those pages are
/// never swapped and kcompactd does not move them. They can still be migrated by
/// an explicit operation (memory offlining, a contiguous allocation that asks
/// for it), so this is the stronger of the two Linux pinnings and **not** a
/// measured equivalent of Windows' large pages; nothing in §84 or §85 measured a
/// Linux guest. With no hugetlb pages reserved, which is the default, this
/// refuses and the message says how to reserve them.
///
/// It is **not** a mock. What it cannot reproduce is the only part that matters
/// on the guest — that these pages are inside a VM and findable from outside it
/// — and no amount of Linux-side testing would have said anything about that.
/// What it does reproduce is the registry plumbing, the stamp layout and the
/// lifetime, which are where the ordinary bugs live.
pub fn alloc_locked_region(
    len: usize,
    small_pages_diagnostic: bool,
    stamp: &dyn Fn(u32, &mut [u8]),
) -> io::Result<LockedRegion> {
    if len == 0 || !len.is_multiple_of(REGION_PAGE) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("a guest-RAM region must be a non-zero multiple of {REGION_PAGE} bytes"),
        ));
    }
    // §103.3: macOS's large pages are XNU superpages. On x86_64 they are 2 MiB,
    // physically contiguous and always wired, so they are this function's
    // `MEM_LARGE_PAGES` with no privilege needed, and the host's sweep finds a
    // whole superpage per stamp run. There is no Apple Silicon equivalent
    // (VM_FLAGS_SUPERPAGE_SIZE_2MB is x86_64-only), which is fine: an Apple
    // Silicon Mac is the VM *host* in §103's role (b), never a guest.
    #[cfg(target_os = "macos")]
    if !small_pages_diagnostic {
        // §103.15: with a keeper, every window is a shared object the keeper
        // holds too, so the host never writes into pages this process's death
        // gave back to macOS. Superpages cannot be shared that way.
        if let Some(k) = KEEPER.get() {
            return alloc_kept(len, stamp, k.as_ref());
        }
        return alloc_superpages(len, stamp);
    }
    #[cfg(target_os = "macos")]
    let (len, flags) = (len, libc::MAP_PRIVATE | libc::MAP_ANONYMOUS);
    #[cfg(not(target_os = "macos"))]
    let (len, flags) = if small_pages_diagnostic {
        (len, libc::MAP_PRIVATE | libc::MAP_ANONYMOUS)
    } else {
        let huge = hugetlb_page_size();
        let rounded = len.div_ceil(huge).checked_mul(huge).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "arena too large")
        })?;
        (rounded, libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | libc::MAP_HUGETLB)
    };
    // SAFETY: an anonymous mapping with a null address hint and a length checked
    // non-zero above; no descriptor is involved. The result is checked.
    let ptr = unsafe {
        libc::mmap(std::ptr::null_mut(), len, libc::PROT_READ | libc::PROT_WRITE, flags, -1, 0)
    };
    if ptr == libc::MAP_FAILED {
        let e = io::Error::last_os_error();
        if small_pages_diagnostic {
            return Err(e);
        }
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            crate::arena::refusal_text(&format!(
                "mmap(MAP_HUGETLB, {len} bytes) failed: {e} (reserve {} pages of {} KiB with \
                 `sysctl vm.nr_hugepages=N`; this machine has {} free)",
                len / hugetlb_page_size(),
                hugetlb_page_size() / 1024,
                hugetlb_free().map_or("an unknown number".into(), |n| n.to_string()),
            )),
        ));
    }
    // Owns the mapping from here, so every early return unmaps it.
    let view = MappedView { ptr: ptr.cast::<u8>().cast_const(), len };
    // SAFETY: `ptr` is a live `len`-byte read-write mapping owned by `view`, and
    // no other reference to it exists yet.
    let bytes = unsafe { std::slice::from_raw_parts_mut(ptr.cast::<u8>(), len) };
    for (i, page) in bytes.chunks_exact_mut(REGION_PAGE).enumerate() {
        stamp(i as u32, page);
    }
    if !small_pages_diagnostic {
        // hugetlb pages are faulted in by the stamps above and never swapped.
        return Ok(LockedRegion { view, len, locked: true, pinning: "hugetlb" });
    }
    // Best effort, exactly as on Windows: `RLIMIT_MEMLOCK` is small for an
    // ordinary account and a region that is merely resident is still a region.
    // SAFETY: the mapping is live and `len` bytes long.
    let locked = unsafe { libc::mlock(ptr, len) } == 0;
    Ok(LockedRegion { view, len, locked, pinning: if locked { "mlock" } else { "none" } })
}

/// §103.15, §111.9: who holds this process's guest-RAM windows besides it.
/// Called with a window's length; returns a descriptor of a shared-memory
/// object of at least that length which the holder (the broker) has already
/// mapped and wired, and keeps until the host has let go of it. An error
/// refuses the window.
#[cfg(target_os = "macos")]
pub type RegionKeeper = Box<dyn Fn(usize) -> io::Result<std::os::fd::OwnedFd> + Send + Sync>;

#[cfg(target_os = "macos")]
static KEEPER: std::sync::OnceLock<RegionKeeper> = std::sync::OnceLock::new();

/// §103.15: install the keeper, before the first window is made.
#[cfg(target_os = "macos")]
pub fn set_region_keeper(k: RegionKeeper) -> bool {
    KEEPER.set(k).is_ok()
}

/// §103.15. A window that outlives this process until the host is done with
/// it. The host writes into guest RAM it located; when a process dies, macOS
/// frees its wired pages at once and reuses them, and a proxy that wrote
/// pixels into them for the rest of its lease corrupted kernel memory (three
/// panics, "element modified after free", the payload Console's background).
/// So the window is a shared-memory object that the keeper (§111.9: the
/// broker, a LaunchDaemon) made, mapped and wired *before* this process
/// stamps it, so before the host can find it, and holds until the host says
/// it has stopped. The object has no name: only descriptors reach it.
#[cfg(target_os = "macos")]
fn alloc_kept(len: usize, stamp: &dyn Fn(u32, &mut [u8]), keeper: &(dyn Fn(usize) -> io::Result<std::os::fd::OwnedFd> + Send + Sync)) -> io::Result<LockedRegion> {
    use std::os::fd::AsRawFd;
    let fd = keeper(len).map_err(|e| io::Error::new(e.kind(), format!("the region keeper gave no window of {len} bytes: {e}")))?;
    // SAFETY: a zeroed stat is a valid out-parameter; the descriptor is live.
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    if unsafe { libc::fstat(fd.as_raw_fd(), &mut st) } != 0 || (st.st_size as u64) < len as u64 {
        return Err(io::Error::other(format!("the region keeper's object is {} bytes, not {len}", st.st_size)));
    }
    // SAFETY: a shared mapping of an object at least `len` bytes long; checked below.
    let ptr = unsafe { libc::mmap(std::ptr::null_mut(), len, libc::PROT_READ | libc::PROT_WRITE, libc::MAP_SHARED, fd.as_raw_fd(), 0) };
    if ptr == libc::MAP_FAILED {
        return Err(io::Error::last_os_error());
    }
    // The mapping keeps the object; the descriptor closes here.
    drop(fd);
    let view = MappedView { ptr: ptr.cast::<u8>().cast_const(), len };
    // Wired here too, as alloc_wired does: the keeper's wiring is what
    // outlives this process, this one is what the host's locate expects of
    // a live session.
    // SAFETY: the mapping is live and `len` bytes long.
    if unsafe { libc::mlock(ptr, len) } != 0 {
        let e = io::Error::last_os_error();
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            crate::arena::refusal_text(&format!("wired pages (mlock: {e}) for {len} bytes; check vm.user_wire_limit")),
        ));
    }
    // SAFETY: `ptr` is a live `len`-byte read-write mapping owned by `view`.
    let bytes = unsafe { std::slice::from_raw_parts_mut(ptr.cast::<u8>(), len) };
    for (i, page) in bytes.chunks_exact_mut(REGION_PAGE).enumerate() {
        stamp(i as u32, page);
    }
    Ok(LockedRegion { view, len, locked: true, pinning: "wired+kept" })
}

/// §103.3. The macOS twin of the hugetlb path: `mmap` with
/// `VM_FLAGS_SUPERPAGE_SIZE_2MB` in the descriptor argument, which is XNU's
/// documented spelling for an anonymous superpage mapping. The kernel either
/// returns physically contiguous, wired 2 MiB pages or fails; it never falls
/// back to small pages, so "superpages" in the report is a fact about the
/// memory rather than a request. Measured in the Sonoma VM on 2026-10-19:
/// 1 GiB in one call, wired-page count up by exactly the mapping.
#[cfg(target_os = "macos")]
fn alloc_superpages(len: usize, stamp: &dyn Fn(u32, &mut [u8])) -> io::Result<LockedRegion> {
    /// `SUPERPAGE_SIZE_2MB << VM_FLAGS_SUPERPAGE_SHIFT`, from
    /// `<mach/vm_statistics.h>`; spelled out so no libc version has to have it.
    const VM_FLAGS_SUPERPAGE_SIZE_2MB: libc::c_int = 2 << 16;
    const SUPERPAGE: usize = 2 << 20;
    let len = len
        .div_ceil(SUPERPAGE)
        .checked_mul(SUPERPAGE)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "arena too large"))?;
    // SAFETY: an anonymous private mapping with a null hint; the "descriptor"
    // is XNU's superpage selector, not a file. The result is checked.
    let ptr = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            len,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_PRIVATE | libc::MAP_ANON,
            VM_FLAGS_SUPERPAGE_SIZE_2MB,
            0,
        )
    };
    if ptr == libc::MAP_FAILED {
        let e = io::Error::last_os_error();
        eprintln!(
            "wc-handle: no superpages for {len} bytes ({e}); taking wired 4 KiB pages, which \
             XNU does not move (§103.3 measured it)"
        );
        return alloc_wired(len, stamp, e);
    }
    let view = MappedView { ptr: ptr.cast::<u8>().cast_const(), len };
    // SAFETY: `ptr` is a live `len`-byte read-write mapping owned by `view`.
    let bytes = unsafe { std::slice::from_raw_parts_mut(ptr.cast::<u8>(), len) };
    for (i, page) in bytes.chunks_exact_mut(REGION_PAGE).enumerate() {
        stamp(i as u32, page);
    }
    Ok(LockedRegion { view, len, locked: true, pinning: "superpages" })
}

/// §103.3. macOS's second tier: `mlock`, which on XNU wires the pages, and a
/// wired page is never paged, compressed or migrated (XNU has no compaction of
/// wired memory; the superpage allocator's contiguous search skips them). That
/// is a reading of the kernel and it was measured, not assumed: on the 4 GiB VM,
/// where superpages are not available at all, `scripts/mac-pinning.sh` located
/// one 64 MiB arena ten times across two sessions with up to 150 MB pushed into
/// the compressor between locates, and every plan was identical and every
/// verify complete. Windows' `VirtualLock` failed the same test (§84.5), which
/// is why this tier exists on macOS only; elsewhere small pages remain the
/// diagnostic.
#[cfg(target_os = "macos")]
fn alloc_wired(len: usize, stamp: &dyn Fn(u32, &mut [u8]), superpage_error: io::Error) -> io::Result<LockedRegion> {
    // SAFETY: an anonymous private mapping with a null hint; checked below.
    let ptr = unsafe {
        libc::mmap(std::ptr::null_mut(), len, libc::PROT_READ | libc::PROT_WRITE, libc::MAP_PRIVATE | libc::MAP_ANON, -1, 0)
    };
    if ptr == libc::MAP_FAILED {
        return Err(io::Error::last_os_error());
    }
    let view = MappedView { ptr: ptr.cast::<u8>().cast_const(), len };
    // Wire before stamping, the order §84.5 found matters: a page stamped and
    // then moved by the wiring would carry its stamp to the wrong frame.
    // SAFETY: the mapping is live and `len` bytes long.
    if unsafe { libc::mlock(ptr, len) } != 0 {
        let e = io::Error::last_os_error();
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            crate::arena::refusal_text(&format!(
                "neither superpages ({superpage_error}) nor wired pages (mlock: {e}) for {len} bytes; \
                 check vm.user_wire_limit"
            )),
        ));
    }
    // SAFETY: `ptr` is a live `len`-byte read-write mapping owned by `view`.
    let bytes = unsafe { std::slice::from_raw_parts_mut(ptr.cast::<u8>(), len) };
    for (i, page) in bytes.chunks_exact_mut(REGION_PAGE).enumerate() {
        stamp(i as u32, page);
    }
    Ok(LockedRegion { view, len, locked: true, pinning: "wired" })
}

/// `Hugepagesize:` from `/proc/meminfo`, which is the size `MAP_HUGETLB` with
/// no size bits uses; 2 MiB when it cannot be read.
fn hugetlb_page_size() -> usize {
    meminfo_kib("Hugepagesize:").map_or(2 << 20, |k| k as usize * 1024)
}

fn hugetlb_free() -> Option<u64> {
    meminfo_kib("HugePages_Free:")
}

fn meminfo_kib(key: &str) -> Option<u64> {
    let text = std::fs::read_to_string("/proc/meminfo").ok()?;
    let line = text.lines().find(|l| l.starts_with(key))?;
    line[key.len()..].split_whitespace().next()?.parse().ok()
}

/// Create an object named by a filesystem path.
///
/// On Unix this is `create_section` under another name, and that identity is
/// the point rather than an accident: the Unix analogue of a named section was
/// always a file at a path (see this module's header). The two names exist so
/// that the *Windows* side, where they are genuinely different system calls,
/// can be reached by one spelling from the shared code in `lib.rs`.
pub fn create_file_object(path: &str, size: u64) -> io::Result<OwnedObject> {
    create_section(path, size)
}

/// Open an object by filesystem path and map it read-only. See
/// [`create_file_object`] for why this is `map_section` under another name.
pub fn map_file(path: &str, len: usize) -> io::Result<MappedView> {
    map_section(path, len)
}

/// Open an object by filesystem path and map it **writable**, as a region.
///
/// STAGE 10, and it is the local twin of [`alloc_locked_region`] rather than a
/// second `map_file`. The type it returns is the same [`MappedView`] the arena
/// registry holds, and the reason a *writable* mapping can come back through a
/// type documented as read-only is the same one `alloc_locked_region` already
/// relies on: the view is not what decides who may write. The registry does —
/// only a `provided` arena has a write path at all, and only its declared
/// outbound window can be reached through it (`crate::arena`).
///
/// This exists so that the compositor side of an arena can be a *file* on a
/// machine with no hypervisor. Under Wine the region cannot be guest RAM,
/// because nothing outside the VM can locate pages when there is no VM; a file
/// both sides open is the substrate that leg has, and everything above
/// `attach` is identical for the two.
pub fn map_file_mut(path: &str, len: usize) -> io::Result<MappedView> {
    let file = OpenOptions::new().read(true).write(true).open(path)?;
    let have = file.metadata()?.len();
    if have < len as u64 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{path} is {have} bytes, not the {len} it was said to be"),
        ));
    }
    if len == 0 {
        return Ok(MappedView { ptr: std::ptr::NonNull::<u8>::dangling().as_ptr(), len: 0 });
    }
    // SAFETY: as `map_section`, with write access added. `MAP_SHARED` because
    // the entire point is that the peer sees these stores; `MAP_PRIVATE` would
    // compile, run, and show the peer nothing forever.
    let ptr = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            len,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_SHARED,
            file.as_raw_fd(),
            0,
        )
    };
    if ptr == libc::MAP_FAILED {
        return Err(io::Error::last_os_error());
    }
    Ok(MappedView { ptr: ptr.cast::<u8>().cast_const(), len })
}

/// Write into an object this process owns.
///
/// `pwrite` rather than a writable mapping: it needs no `unsafe`, and the
/// offsets involved are small and infrequent — this path exists to seed a
/// keymap or a test pattern, not to blit frames.
pub fn write_section(object: &OwnedObject, offset: usize, bytes: &[u8]) -> io::Result<()> {
    object.file.write_all_at(bytes, offset as u64)
}

/// Map an object this process owns, writable, and keep the mapping.
///
/// The Unix half of `Handle::map_mut`. `MAP_SHARED`, because the whole point is
/// that the peer sees the writes; `MAP_PRIVATE` would compile, run, and quietly
/// show the compositor an empty buffer forever.
pub fn map_section_mut(object: &OwnedObject, len: usize) -> io::Result<WritableView> {
    if len == 0 {
        return Ok(WritableView {
            ptr: std::ptr::NonNull::<u8>::dangling().as_ptr(),
            len: 0,
        });
    }
    // SAFETY: a live fd for an object of at least `len` bytes; the kernel
    // chooses the address.
    let ptr = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            len,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_SHARED,
            object.file.as_raw_fd(),
            0,
        )
    };
    if ptr == libc::MAP_FAILED {
        return Err(io::Error::last_os_error());
    }
    Ok(WritableView {
        ptr: ptr.cast::<u8>(),
        len,
    })
}

/// A writable mapping of an object this process owns.
#[derive(Debug)]
pub struct WritableView {
    ptr: *mut u8,
    len: usize,
}

// SAFETY: the mapping is owned solely by this struct and moves with it. No
// `Sync`: handing out `&mut [u8]` needs `&mut self`, which is exclusive.
unsafe impl Send for WritableView {}

impl WritableView {
    /// The same mapping, for reading. See `crate::MappingMut::as_slice`.
    pub fn ptr(&self) -> *const u8 {
        self.ptr.cast_const()
    }

    pub fn ptr_mut(&mut self) -> *mut u8 {
        self.ptr
    }
}

impl Drop for WritableView {
    fn drop(&mut self) {
        if self.len > 0 {
            // SAFETY: `ptr`/`len` are exactly what `mmap` returned, unmapped
            // once.
            unsafe {
                libc::munmap(self.ptr.cast::<libc::c_void>(), self.len);
            }
        }
    }
}

/// This process's resident set, in bytes, from `/proc/self/statm`.
///
/// The Linux analogue of the Windows working set, for the development guest of
/// §13 step 7. See [`crate::arena::committed_bytes`].
pub fn working_set_bytes() -> Option<u64> {
    let s = std::fs::read_to_string("/proc/self/statm").ok()?;
    let rss_pages: u64 = s.split_whitespace().nth(1)?.parse().ok()?;
    Some(rss_pages * 4096)
}

// ---------------------------------------------------------------- pipes ---
//
// STAGE §81. The third pair in this module's contract, after "named section"
// and "object at a path": a **byte stream a peer opens by name**.
//
// `Descriptor::Pipe` has existed since stage 1 and nothing constructed or
// opened one, on either platform. It is what a clipboard or drag transfer needs
// when the two ends speak different `h` dialects: an out-of-band client hands
// the compositor a real pipe, an in-band source has to be handed a *name*, and
// nothing could make the name.
//
// The Unix analogue is a FIFO. It is a filesystem object, so it has a name, and
// it is a stream, so it has the semantics `wl_data_offer.receive` expects.

/// A FIFO this process created, held open so that its name stays live.
///
/// Dropping it unlinks the path as well as closing the descriptor, exactly as
/// [`OwnedObject`] does and for the same reason.
#[derive(Debug)]
pub struct OwnedPipe {
    /// Held `O_RDWR`. See [`create_pipe`].
    keepalive: File,
    path: PathBuf,
}

impl OwnedPipe {
    /// The end this process kept.
    pub fn as_file(&self) -> &File {
        &self.keepalive
    }
}

impl Drop for OwnedPipe {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Create a FIFO at `path` and hold it open.
///
/// # Why the keepalive is `O_RDWR`, which is the one subtle thing here
///
/// A FIFO with no writer gives its readers EOF, and a FIFO with no reader gives
/// its writers `SIGPIPE`. Worse, an `open()` for reading **blocks until a writer
/// arrives** and vice versa — which in a compositor's event loop is not a
/// slow path, it is a deadlock.
///
/// Opening the creator's own end `O_RDWR` fixes all three at once: the FIFO
/// always has at least one reader and one writer, so the peer's `open()` returns
/// immediately, no end sees a spurious EOF, and nobody is killed by `SIGPIPE`
/// because the other end went first. It costs one descriptor and it is the
/// standard answer; it is written down here because a reader who has not hit
/// this before will read `O_RDWR` on a unidirectional object as a mistake.
pub fn create_pipe(path: &str) -> io::Result<OwnedPipe> {
    let c_path = std::ffi::CString::new(path)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "a NUL in a pipe path"))?;
    // Best-effort: a stale node from a crashed run must not make this fail.
    let _ = std::fs::remove_file(path);
    // SAFETY: a NUL-terminated path this function owns for the call.
    if unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let keepalive = match OpenOptions::new().read(true).write(true).open(path) {
        Ok(f) => f,
        Err(e) => {
            let _ = std::fs::remove_file(path);
            return Err(e);
        }
    };
    Ok(OwnedPipe { keepalive, path: PathBuf::from(path) })
}

/// Open a FIFO somebody else created, for reading or for writing.
///
/// `for_write` is the *opener's* direction, which is the inverse of
/// `Descriptor::Pipe`'s `peer_reads` as seen from the creator. The caller
/// resolves that; this function is told plainly which end it wants, because a
/// boolean that means the opposite thing depending on who is asking is how a
/// clipboard transfer ends up reading its own writes.
pub fn open_pipe(path: &str, for_write: bool) -> io::Result<File> {
    OpenOptions::new().read(!for_write).write(for_write).open(path)
}

/// IVSHMEM ARENA. An ivshmem device is a PCI device in a *guest*, reached
/// through a Windows driver; there is nothing on this side to open. The VM
/// host's half maps the file QEMU backs the BAR with, which is
/// `wc_proxy::guestram`'s business and needs no platform call. Tests stand a
/// writable file mapping in for the BAR (`crate::arena`'s tests).
pub fn map_ivshmem_slot(slot: u8) -> io::Result<(MappedView, u64, String)> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        format!("ivshmem slot {slot:#04x}: an ivshmem device is a Windows guest's, not this machine's"),
    ))
}

/// See [`map_ivshmem_slot`].
///
/// **One stand-in, for a Linux test of the §89 path end to end**: with
/// `WC_IVSHMEM_STANDIN=<file>` set, that file is mapped read-write as device
/// `index`'s BAR, so a Linux compositor and a proxy on one machine run the
/// launcher's whole ivshmem layout — header, arena, rings — with a file where
/// the device would be (`scripts/wc-rings.sh`). It exists only here: the
/// compositor the launcher runs is the Windows one, whose `map_ivshmem` maps a
/// device and nothing else, so this cannot turn the product's arena into a
/// file (§28.2's rule is about that compositor, and it still holds).
pub fn map_ivshmem(index: u32) -> io::Result<(MappedView, u64, String)> {
    if let Some(path) = std::env::var_os("WC_IVSHMEM_STANDIN") {
        let path = path.to_string_lossy().into_owned();
        let len = std::fs::metadata(&path)?.len();
        let view = map_file_mut(&path, len as usize)?;
        return Ok((view, len, format!("stand-in file {path} for device {index}")));
    }
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        format!("ivshmem device {index}: an ivshmem device is a Windows guest's, not this machine's"),
    ))
}

/// Read-only checked window of an already-open host memory backend. The fd
/// is opened and authenticated by the host, never by a guest pathname.
#[cfg(target_os = "linux")]
pub(crate) fn map_file_window(file: &File, offset: u64, len: usize) -> io::Result<MappedView> {
    let end = offset.checked_add(len as u64)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "mapping window overflows"))?;
    if len == 0 || end > file.metadata()?.len() || offset > i64::MAX as u64 {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "mapping window exceeds backend"));
    }
    // SAFETY: nonempty checked in-file window, live owned fd and a null hint.
    // The kernel validates page alignment. Mapping retains its own file ref.
    let ptr = unsafe { libc::mmap(std::ptr::null_mut(), len, libc::PROT_READ,
        libc::MAP_SHARED, file.as_raw_fd(), offset as libc::off_t) };
    if ptr == libc::MAP_FAILED { return Err(io::Error::last_os_error()); }
    Ok(MappedView { ptr: ptr.cast(), len })
}

/// Writable metadata or local-producer window of the authenticated BAR.
#[cfg(target_os = "linux")]
pub(crate) fn map_file_window_mut(file: &File, offset: u64, len: usize) -> io::Result<MappedView> {
    let end = offset.checked_add(len as u64)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "mapping window overflows"))?;
    if len == 0 || end > file.metadata()?.len() || offset > i64::MAX as u64 {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "mapping window exceeds backend"));
    }
    // SAFETY: checked aligned retained-file window; callers restrict this to
    // shared atomic metadata or bytes owned by this process's producer role.
    let ptr = unsafe { libc::mmap(std::ptr::null_mut(), len, libc::PROT_READ | libc::PROT_WRITE,
        libc::MAP_SHARED, file.as_raw_fd(), offset as libc::off_t) };
    if ptr == libc::MAP_FAILED { return Err(io::Error::last_os_error()); }
    Ok(MappedView { ptr: ptr.cast(), len })
}
