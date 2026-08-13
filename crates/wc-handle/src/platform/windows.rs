//! The Windows side of the handle shim — the reason this crate exists.
//!
//! Everything here is `CreateFileMappingW` with a name and `OpenFileMappingW`
//! with the same name. There is deliberately **no `DuplicateHandle` anywhere in
//! this file**, and that absence is the design: a named section is reachable by
//! any process that knows the name, so the sender never needs a handle to the
//! receiver's process and the compositor never needs `PROCESS_DUP_HANDLE` over
//! its clients.
//!
//! The names go in `Local\`, the per-logon-session object namespace, rather
//! than `Global\`. `Local\` is the correct scope for a desktop session's
//! display server: another user logged into the same machine cannot open the
//! object even if they learn the name, and no privilege is needed to create it
//! (`Global\` needs `SeCreateGlobalPrivilege`).

use std::io;

use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_ALREADY_EXISTS, GENERIC_READ, GENERIC_WRITE, HANDLE, INVALID_HANDLE_VALUE,
};
use windows_sys::Win32::Storage::FileSystem::{
    CREATE_NEW, CreateFileW, DeleteFileW, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_DELETE,
    FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows_sys::Win32::Security::Cryptography::{
    BCryptGenRandom, BCRYPT_USE_SYSTEM_PREFERRED_RNG,
};
use windows_sys::Win32::System::Memory::{
    CreateFileMappingW, MapViewOfFile, OpenFileMappingW, UnmapViewOfFile, VirtualAlloc, VirtualFree,
    VirtualLock, FILE_MAP_READ, FILE_MAP_WRITE, MEM_COMMIT, MEM_RELEASE, MEM_RESERVE,
    MEMORY_MAPPED_VIEW_ADDRESS, PAGE_READONLY, PAGE_READWRITE,
};
// STAGE 7: the working-set quota that `VirtualLock` is charged against.
use windows_sys::Win32::System::Threading::{
    GetCurrentProcess, GetProcessWorkingSetSize, SetProcessWorkingSetSize,
};

/// A section object held open by its creator.
///
/// Holding it open is not bookkeeping, it is the lifetime rule: a Windows named
/// section with no open handles is destroyed, and its name becomes free for
/// anyone to take. Dropping this before the peer has opened the section is
/// therefore both a lost buffer and a name-reuse hazard, which is why
/// `wc_handle::Handle` keeps it in an `Arc` for as long as any clone of the
/// handle survives.
#[derive(Debug)]
pub struct OwnedObject {
    section: HANDLE,
    /// Set only for a file-backed object (`create_file_object`), and deleted on
    /// drop so that a finished process leaves no shared-memory files behind.
    ///
    /// A pagefile-backed section needs nothing here: its name ceases to exist
    /// with its last handle, which is the behaviour this field reproduces for
    /// the one kind of object that would otherwise outlive its creator.
    path: Option<Vec<u16>>,
}

// SAFETY: an NT handle is a process-wide value, not thread-affine, and every
// use of it here goes through a Win32 call that is itself thread-safe.
unsafe impl Send for OwnedObject {}
unsafe impl Sync for OwnedObject {}

impl Drop for OwnedObject {
    fn drop(&mut self) {
        // SAFETY: `section` came from `CreateFileMappingW`, was checked
        // non-null, and is closed exactly once here.
        unsafe {
            CloseHandle(self.section);
        }
        if let Some(path) = &self.path {
            // After the section handle, never before: a file with a live
            // mapping cannot be deleted, and the order matters more here than
            // the failure does. Nothing can be done if it fails, and a leftover
            // file is a tidiness problem rather than a correctness one.
            // SAFETY: a live NUL-terminated UTF-16 buffer owned by `self`.
            unsafe {
                DeleteFileW(path.as_ptr());
            }
        }
    }
}

/// How a [`MappedView`]'s bytes were obtained, which is the only thing that
/// decides how they are given back.
///
/// STAGE 7. Until now every view was a view *of an object*: `MapViewOfFile`
/// gave it and `UnmapViewOfFile` takes it away. A guest-RAM arena is not — it is
/// this process's own committed memory, pinned, and `UnmapViewOfFile` on it
/// fails harmlessly while leaking 64 MiB of the guest's RAM for the life of the
/// process. Recording which one it is here rather than in the caller means the
/// wrong release call cannot be made at all.
enum Backing {
    /// `MapViewOfFile` — a window onto a section or a file.
    View,
    /// `VirtualAlloc(MEM_COMMIT)` — memory this process owns, released with
    /// `VirtualFree(MEM_RELEASE)`. See [`alloc_locked_region`].
    Region,
}

/// A read-only view of a section, or of a region this process allocated.
///
/// No `derive(Debug)`: `MEMORY_MAPPED_VIEW_ADDRESS` is a bare `*mut c_void`
/// newtype and does not implement it. Printing the address would be of no use
/// to anyone anyway — it differs between the two processes mapping the same
/// section, which is the whole point.
pub struct MappedView {
    address: MEMORY_MAPPED_VIEW_ADDRESS,
    len: usize,
    backing: Backing,
}

impl std::fmt::Debug for MappedView {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MappedView").field("len", &self.len).finish()
    }
}

// SAFETY: as for the Unix mapping — the view is owned solely by this struct and
// is read-only, so sharing it hands out `&[u8]`.
unsafe impl Send for MappedView {}
unsafe impl Sync for MappedView {}

impl MappedView {
    pub fn ptr(&self) -> *const u8 {
        self.address.Value.cast::<u8>().cast_const()
    }
}

impl Drop for MappedView {
    fn drop(&mut self) {
        if self.len == 0 {
            return;
        }
        match self.backing {
            // SAFETY: `address` is exactly what `MapViewOfFile` returned and
            // this runs once, in `Drop`.
            Backing::View => unsafe {
                UnmapViewOfFile(self.address);
            },
            // SAFETY: `address` is exactly what `VirtualAlloc` returned. The
            // size argument to `MEM_RELEASE` must be 0 and the address must be
            // the base of the reservation, which both hold here. Any
            // `VirtualLock` on the region is undone by the release.
            Backing::Region => unsafe {
                VirtualFree(self.address.Value, 0, MEM_RELEASE);
            },
        }
    }
}

/// Commit `len` bytes of this process's own memory, pin them into the working
/// set, and let the caller stamp every 4096-byte page before the region is
/// handed back **read-only**.
///
/// STAGE 7, and the whole substrate change. Returns `(view, locked)`: `locked`
/// is false when `VirtualLock` failed, which is a real and survivable state —
/// the pages are still guest RAM and still findable, they are merely allowed to
/// move — and the caller reports it rather than this function pretending.
///
/// # Why the working set is resized first
///
/// `VirtualLock` fails with `ERROR_WORKING_SET_QUOTA` (1453) unless the
/// process's minimum working set is large enough to hold the region as well as
/// everything else it has. The default minimum is on the order of a megabyte,
/// so a 64 MiB arena fails without this call and the failure reads as "locking
/// is not permitted" rather than "the quota is too small". Same recipe as
/// `shared_vulkan`'s `awe_probe::alloc::try_locked`, and it is deliberate that
/// the numbers match: this is the same allocation its step-0 experiment made.
///
/// # Why the caller stamps here rather than afterwards
///
/// The region is handed out as a [`MappedView`], which is read-only by
/// contract — every other view in this file is a `FILE_MAP_READ` mapping and the
/// compositor must not be able to write a client's pixels. The stamp is the one
/// write this process ever makes, it happens exactly once, and doing it inside
/// this function is what lets the contract stay unconditional.
pub fn alloc_locked_region(
    len: usize,
    stamp: &dyn Fn(u32, &mut [u8]),
) -> io::Result<(MappedView, bool)> {
    if len == 0 || !len.is_multiple_of(REGION_PAGE) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("a guest-RAM region must be a non-zero multiple of {REGION_PAGE} bytes"),
        ));
    }
    // SAFETY: a null base asks the kernel to choose the address; the size is
    // checked non-zero above. The result is checked for null before use.
    let base = unsafe { VirtualAlloc(std::ptr::null(), len, MEM_RESERVE | MEM_COMMIT, PAGE_READWRITE) };
    if base.is_null() {
        return Err(io::Error::last_os_error());
    }
    let view = MappedView {
        address: MEMORY_MAPPED_VIEW_ADDRESS { Value: base },
        len,
        backing: Backing::Region,
    };
    // From here on `view` owns the reservation, so every early return releases
    // it. That ordering is why the `MappedView` is built before the region is
    // touched rather than after.
    let (mut lo, mut hi) = (0usize, 0usize);
    // SAFETY: both out-parameters are live locals; the pseudo-handle from
    // `GetCurrentProcess` needs no closing.
    unsafe {
        if GetProcessWorkingSetSize(GetCurrentProcess(), &mut lo, &mut hi) != 0 {
            // Twice the region, as shared_vulkan does: the arena has to fit and
            // so does everything the process already had resident.
            SetProcessWorkingSetSize(GetCurrentProcess(), lo + len * 2, hi + len * 2);
        }
    }
    // Stamp before locking. Touching every page is what commits it in practice,
    // and locking pages that have never been touched is a slower way to reach
    // the same state.
    // SAFETY: `base` is a live `len`-byte read-write region owned by `view`, and
    // no other reference to it exists yet.
    let bytes = unsafe { std::slice::from_raw_parts_mut(base.cast::<u8>(), len) };
    for (i, page) in bytes.chunks_exact_mut(REGION_PAGE).enumerate() {
        stamp(i as u32, page);
    }
    // SAFETY: the region is live and `len` bytes long.
    let locked = unsafe { VirtualLock(base, len) } != 0;
    Ok((view, locked))
}

/// The granularity every stamp and every locate step works in. Not a Windows
/// constant: it is the *guest-physical* page size the host's scan sweeps in, and
/// it stays 4096 whatever the allocator did (shared_vulkan design.md §5.1).
pub const REGION_PAGE: usize = 4096;

/// NUL-terminated UTF-16, which is what every `*W` entry point wants.
fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// 128 bits from the system CSPRNG, hex encoded, in the session-local
/// namespace.
///
/// `BCryptGenRandom` with `BCRYPT_USE_SYSTEM_PREFERRED_RNG` rather than
/// `ProcessPrng`: it is the older and far more widely implemented of the two,
/// which matters because Wine is the only place this code can currently be run
/// at all.
pub fn random_section_name() -> String {
    format!("Local\\{}", random_token())
}

/// The unguessable part of a name, without deciding what namespace it goes in.
///
/// Split out of [`random_section_name`] for `crate::publish`, which mints file
/// names in a directory the caller chose rather than in the NT object
/// namespace. The randomness requirement is identical and so is the degraded
/// path; only the prefix differs, and having one source of the bytes means a
/// future change to how they are drawn cannot apply to one caller and not the
/// other.
pub fn random_token() -> String {
    let mut bytes = [0u8; 16];
    // SAFETY: a live 16-byte buffer whose length we pass truthfully. The null
    // algorithm handle is required by `BCRYPT_USE_SYSTEM_PREFERRED_RNG`.
    let status = unsafe {
        BCryptGenRandom(
            std::ptr::null_mut(),
            bytes.as_mut_ptr(),
            bytes.len() as u32,
            BCRYPT_USE_SYSTEM_PREFERRED_RNG,
        )
    };
    if status != 0 {
        // See the Unix counterpart: degrading is better than refusing to run,
        // but the caller must be able to tell it happened, so it is in the name.
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        bytes.copy_from_slice(&now.to_le_bytes());
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

/// Create a named, pagefile-backed section of `size` bytes.
pub fn create_section(name: &str, size: u64) -> io::Result<OwnedObject> {
    let name_w = wide(name);
    // SAFETY: `INVALID_HANDLE_VALUE` as the file handle is the documented way
    // to ask for pagefile backing; the name pointer is a live NUL-terminated
    // UTF-16 buffer for the duration of the call.
    let section = unsafe {
        CreateFileMappingW(
            INVALID_HANDLE_VALUE,
            std::ptr::null(),
            PAGE_READWRITE,
            (size >> 32) as u32,
            size as u32,
            name_w.as_ptr(),
        )
    };
    if section.is_null() {
        return Err(io::Error::last_os_error());
    }
    // `CreateFileMappingW` succeeds and returns a handle to the *existing*
    // object if the name is taken, reporting it only through
    // `ERROR_ALREADY_EXISTS`. Silently adopting somebody else's section would
    // be the worst possible failure mode — two clients sharing a buffer neither
    // asked to share — so this is checked rather than assumed away by the
    // randomness of the name.
    let existed = io::Error::last_os_error().raw_os_error() == Some(ERROR_ALREADY_EXISTS as i32);
    if existed {
        // SAFETY: closing the handle we were just given, exactly once.
        unsafe {
            CloseHandle(section);
        }
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!("section name {name} was already taken"),
        ));
    }
    Ok(OwnedObject { section, path: None })
}

/// Open a section by name and map it read-only.
pub fn map_section(name: &str, len: usize) -> io::Result<MappedView> {
    let name_w = wide(name);
    // SAFETY: live NUL-terminated name; `binherithandle = 0` (FALSE) because
    // nothing here is inherited by child processes.
    let section = unsafe { OpenFileMappingW(FILE_MAP_READ, 0, name_w.as_ptr()) };
    if section.is_null() {
        return Err(io::Error::last_os_error());
    }
    if len == 0 {
        // SAFETY: closing the handle we just opened.
        unsafe {
            CloseHandle(section);
        }
        return Ok(MappedView {
            address: MEMORY_MAPPED_VIEW_ADDRESS {
                Value: std::ptr::NonNull::<u8>::dangling().as_ptr().cast(),
            },
            len: 0,
            backing: Backing::View,
        });
    }
    // SAFETY: mapping the whole section we just opened, read-only.
    let address = unsafe { MapViewOfFile(section, FILE_MAP_READ, 0, 0, len) };
    // The view holds its own reference to the section object, so the handle can
    // go now; the mapping stays valid until `UnmapViewOfFile`.
    // SAFETY: closing the handle exactly once, after the view is established.
    unsafe {
        CloseHandle(section);
    }
    if address.Value.is_null() {
        return Err(io::Error::last_os_error());
    }
    Ok(MappedView { address, len, backing: Backing::View })
}

/// Create a shared-memory object named by a **filesystem path**.
///
/// ADDED FOR STAGE 4. The difference from `create_section` is the first
/// argument to `CreateFileMappingW`: a real file handle instead of
/// `INVALID_HANDLE_VALUE`, so the object's identity is a path rather than an
/// entry in the NT object namespace. That is what makes it reachable from a
/// process that is not running on this operating system at all — see
/// `crate::Descriptor::SharedFile`.
///
/// `CREATE_NEW` for the same reason `create_section` refuses
/// `ERROR_ALREADY_EXISTS`: silently adopting an existing object would mean two
/// clients sharing a buffer neither asked to share.
pub fn create_file_object(path: &str, size: u64) -> io::Result<OwnedObject> {
    let path_w = wide(path);
    // SAFETY: a live NUL-terminated UTF-16 path for the duration of the call.
    let file = unsafe {
        CreateFileW(
            path_w.as_ptr(),
            GENERIC_READ | GENERIC_WRITE,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            std::ptr::null(),
            CREATE_NEW,
            FILE_ATTRIBUTE_NORMAL,
            std::ptr::null_mut(),
        )
    };
    if file == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    // `CreateFileMappingW` extends the file to the size requested, so there is
    // no separate `SetEndOfFile`. A zero size would mean "the file's current
    // length", which is zero here, so it is refused rather than silently
    // producing an unmappable object.
    if size == 0 {
        // SAFETY: closing the handle we just opened, exactly once.
        unsafe {
            CloseHandle(file);
            DeleteFileW(path_w.as_ptr());
        }
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "a shared file must have a non-zero size",
        ));
    }
    // SAFETY: a live file handle and a null name, which is what asks for an
    // unnamed section backed by that file.
    let section = unsafe {
        CreateFileMappingW(
            file,
            std::ptr::null(),
            PAGE_READWRITE,
            (size >> 32) as u32,
            size as u32,
            std::ptr::null(),
        )
    };
    // The section holds its own reference to the file, so the file handle can
    // go now.
    // SAFETY: closing the file handle exactly once, after the section exists.
    unsafe {
        CloseHandle(file);
    }
    if section.is_null() {
        let err = io::Error::last_os_error();
        // SAFETY: a live NUL-terminated path; removing the file we just made.
        unsafe {
            DeleteFileW(path_w.as_ptr());
        }
        return Err(err);
    }
    Ok(OwnedObject { section, path: Some(path_w) })
}

/// Open a shared-memory object somebody else created, for writing, without
/// taking on the job of deleting it. See `crate::Handle::open_shared_file`.
pub fn open_file_object(path: &str, size: u64) -> io::Result<OwnedObject> {
    let path_w = wide(path);
    // SAFETY: a live NUL-terminated UTF-16 path for the duration of the call.
    let file = unsafe {
        CreateFileW(
            path_w.as_ptr(),
            GENERIC_READ | GENERIC_WRITE,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            std::ptr::null(),
            OPEN_EXISTING,
            FILE_ATTRIBUTE_NORMAL,
            std::ptr::null_mut(),
        )
    };
    if file == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    // Size 0: take the file's own length, so opening cannot silently extend an
    // arena somebody else sized.
    // SAFETY: a live file handle and a null name.
    let section = unsafe {
        CreateFileMappingW(file, std::ptr::null(), PAGE_READWRITE, 0, 0, std::ptr::null())
    };
    // SAFETY: closing the file handle once, after the section holds its own
    // reference.
    unsafe {
        CloseHandle(file);
    }
    if section.is_null() {
        return Err(io::Error::last_os_error());
    }
    let _ = size;
    Ok(OwnedObject { section, path: None })
}

/// Open a shared-memory object by **filesystem path** and map it read-only.
///
/// ADDED FOR STAGE 4, and the receiving half of the stage: this is what the
/// compositor calls when a `wl_shm` pool arrives from a Linux proxy that could
/// not have created an NT object name.
///
/// Measured with `scripts/probe-filemap.c` before anything was built on it: a
/// view obtained this way from a `Z:` path under Wine is coherent with a Linux
/// `MAP_SHARED` mmap of the same file, in both directions, with no flush. That
/// is not a property a network or paravirtual filesystem is obliged to have —
/// virtio-fs without DAX caches file pages in the guest — so it is checked on
/// each deployment rather than assumed from this one.
pub fn map_file(path: &str, len: usize) -> io::Result<MappedView> {
    let path_w = wide(path);
    // Read-only throughout: `GENERIC_READ`, `PAGE_READONLY`, `FILE_MAP_READ`.
    // A compositor's view of a client's buffer is read-only by design
    // (docs/waycompose-design.md §7.4), and asking for write access here would also make
    // the open fail against a genuinely read-only mount.
    // SAFETY: a live NUL-terminated UTF-16 path for the duration of the call.
    let file = unsafe {
        CreateFileW(
            path_w.as_ptr(),
            GENERIC_READ,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            std::ptr::null(),
            OPEN_EXISTING,
            FILE_ATTRIBUTE_NORMAL,
            std::ptr::null_mut(),
        )
    };
    if file == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    if len == 0 {
        // SAFETY: closing the handle we just opened.
        unsafe {
            CloseHandle(file);
        }
        return Ok(MappedView {
            address: MEMORY_MAPPED_VIEW_ADDRESS {
                Value: std::ptr::NonNull::<u8>::dangling().as_ptr().cast(),
            },
            len: 0,
            backing: Backing::View,
        });
    }
    // Size 0 means "the whole file", which is what a receiver wants: it was
    // told a length in the descriptor and `MapViewOfFile` below asks for
    // exactly that, so a file shorter than advertised fails there rather than
    // handing back a short view.
    // SAFETY: a live file handle and a null name.
    let section = unsafe {
        CreateFileMappingW(file, std::ptr::null(), PAGE_READONLY, 0, 0, std::ptr::null())
    };
    // SAFETY: closing the file handle once, after the section holds its own
    // reference to it.
    unsafe {
        CloseHandle(file);
    }
    if section.is_null() {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: mapping `len` bytes of the section we just made, read-only.
    let address = unsafe { MapViewOfFile(section, FILE_MAP_READ, 0, 0, len) };
    // SAFETY: closing the section handle once; the view keeps its own
    // reference, exactly as in `map_section`.
    unsafe {
        CloseHandle(section);
    }
    if address.Value.is_null() {
        return Err(io::Error::last_os_error());
    }
    Ok(MappedView { address, len, backing: Backing::View })
}

/// Open a shared-memory object by **filesystem path** and map it **writable**,
/// as a region.
///
/// STAGE 10, and it is the local twin of [`alloc_locked_region`] rather than a
/// second [`map_file`]. The type it returns is the same [`MappedView`] the
/// arena registry holds, and the reason a *writable* mapping can come back
/// through a type documented as read-only is the one `alloc_locked_region`
/// already relies on: the view is not what decides who may write. The registry
/// does — only a `provided` arena has a write path at all, and only its
/// declared outbound window can be reached through it (`crate::arena`).
///
/// This exists so that the compositor side of an arena can be a *file* on a
/// machine with no hypervisor. Under Wine the region cannot be guest RAM,
/// because nothing outside the VM can locate pages when there is no VM; a file
/// both sides open is the substrate that leg has, and everything above `attach`
/// is identical for the two.
pub fn map_file_mut(path: &str, len: usize) -> io::Result<MappedView> {
    let path_w = wide(path);
    // SAFETY: a live NUL-terminated UTF-16 path for the duration of the call.
    let file = unsafe {
        CreateFileW(
            path_w.as_ptr(),
            GENERIC_READ | GENERIC_WRITE,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            std::ptr::null(),
            OPEN_EXISTING,
            FILE_ATTRIBUTE_NORMAL,
            std::ptr::null_mut(),
        )
    };
    if file == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    if len == 0 {
        // SAFETY: closing the handle we just opened.
        unsafe {
            CloseHandle(file);
        }
        return Ok(MappedView {
            address: MEMORY_MAPPED_VIEW_ADDRESS {
                Value: std::ptr::NonNull::<u8>::dangling().as_ptr().cast(),
            },
            len: 0,
            backing: Backing::View,
        });
    }
    // SAFETY: a live file handle and a null name. Size 0 means "the whole
    // file", so a file shorter than advertised fails at `MapViewOfFile` below
    // rather than handing back a short view.
    let section = unsafe {
        CreateFileMappingW(file, std::ptr::null(), PAGE_READWRITE, 0, 0, std::ptr::null())
    };
    // SAFETY: closing the file handle once, after the section holds its own
    // reference to it.
    unsafe {
        CloseHandle(file);
    }
    if section.is_null() {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: mapping `len` bytes of the section we just made, read-write.
    let address = unsafe { MapViewOfFile(section, FILE_MAP_READ | FILE_MAP_WRITE, 0, 0, len) };
    // SAFETY: closing the section handle once; the view keeps its own
    // reference, exactly as in `map_section`.
    unsafe {
        CloseHandle(section);
    }
    if address.Value.is_null() {
        return Err(io::Error::last_os_error());
    }
    Ok(MappedView { address, len, backing: Backing::View })
}

/// Write into a section this process created.
pub fn write_section(object: &OwnedObject, offset: usize, bytes: &[u8]) -> io::Result<()> {
    if bytes.is_empty() {
        return Ok(());
    }
    let len = offset + bytes.len();
    // SAFETY: a writable view of the section this process owns, long enough to
    // cover the requested range; the caller has already bounds-checked against
    // the section size.
    let address = unsafe { MapViewOfFile(object.section, FILE_MAP_WRITE, 0, 0, len) };
    if address.Value.is_null() {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `address` covers `len` bytes and `offset + bytes.len() == len`,
    // so the destination is inside the view. The source and destination cannot
    // overlap: one is a fresh mapping, the other is the caller's slice.
    unsafe {
        std::ptr::copy_nonoverlapping(
            bytes.as_ptr(),
            address.Value.cast::<u8>().add(offset),
            bytes.len(),
        );
        UnmapViewOfFile(address);
    }
    Ok(())
}

/// Map a section this process created, **writable**, and keep the view.
///
/// # Why this exists alongside `write_section`
///
/// `write_section` maps and unmaps around every call, which is right for
/// writing a keymap once. It is wrong for a client drawing frames: a partial
/// update of one damaged row would map and unmap a view per row, and an
/// animating client would spend its time in the memory manager rather than
/// drawing.
///
/// Restricted to the creator by construction — it takes an [`OwnedObject`],
/// which only `create_section` produces. A `Handle` decoded from the wire has
/// no `OwnedObject` and therefore no way to reach this, which keeps the
/// compositor's view of a client buffer read-only as `map_section` intends.
pub fn map_section_mut(object: &OwnedObject, len: usize) -> io::Result<WritableView> {
    if len == 0 {
        return Ok(WritableView {
            address: MEMORY_MAPPED_VIEW_ADDRESS {
                Value: std::ptr::NonNull::<u8>::dangling().as_ptr().cast(),
            },
            len: 0,
        });
    }
    // SAFETY: mapping the whole of a section this process owns, for writing.
    let address = unsafe { MapViewOfFile(object.section, FILE_MAP_WRITE, 0, 0, len) };
    if address.Value.is_null() {
        return Err(io::Error::last_os_error());
    }
    Ok(WritableView { address, len })
}

/// A writable view of a section, held open.
pub struct WritableView {
    address: MEMORY_MAPPED_VIEW_ADDRESS,
    len: usize,
}

impl std::fmt::Debug for WritableView {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WritableView").field("len", &self.len).finish()
    }
}

// SAFETY: the view is owned solely by this struct, and handing out `&mut [u8]`
// requires `&mut self`, so Rust's own borrow rules serialise access from this
// process. The *peer* may read concurrently — that is what shared memory is
// for and is a `wl_shm` fact rather than a soundness claim.
unsafe impl Send for WritableView {}

impl WritableView {
    /// The same mapping, for reading. See `crate::MappingMut::as_slice`.
    pub fn ptr(&self) -> *const u8 {
        self.address.Value.cast::<u8>().cast_const()
    }

    pub fn ptr_mut(&mut self) -> *mut u8 {
        self.address.Value.cast::<u8>()
    }
}

impl Drop for WritableView {
    fn drop(&mut self) {
        if self.len > 0 {
            // SAFETY: exactly what `MapViewOfFile` returned, unmapped once.
            unsafe {
                UnmapViewOfFile(self.address);
            }
        }
    }
}
