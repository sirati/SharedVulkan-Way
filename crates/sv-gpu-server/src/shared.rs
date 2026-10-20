//! Shared sections: the GPU side of the dma-buf path (waycompose-design.md
//! §104).
//!
//! A `VkDeviceMemory` the application allocates for export, or imports from a
//! token, is a **named** shared section this process created: pagefile-backed
//! on Windows (`Local\sv-shared-<pid>-<n>-<16 hex>`), a file under `/dev/shm`
//! on Linux (the name is its absolute path). This process maps it and imports
//! the view into the session's device with `VK_EXT_external_memory_host`; the
//! compositor opens the same section by name and reads the pixels the GPU
//! wrote. Nothing but the name leaves the guest.
//!
//! The registry is per process: a name is importable only if this process
//! created it and the nonce matches, so a token cannot make the GPU side open
//! an arbitrary section or file. It is shared by **every session** of the
//! process, which is what lets a Plasma client's buffer (its own ICD session)
//! be imported into KWin's session: the importer maps a view of its own, the
//! entry counts the views of all sessions, and the section outlives its
//! creator's free for as long as any session holds a view. On Windows each
//! view holds its own section handle, so the kernel keeps the section exactly
//! that long; on Linux the file is unlinked with the last view.

use crate::driver::Driver;
use ash::vk;
use std::collections::HashMap;
use std::sync::Mutex;

/// `VK_EXTERNAL_MEMORY_HANDLE_TYPE_HOST_ALLOCATION_BIT_EXT`.
pub const HOST_ALLOCATION: u32 = 0x80;
/// `VK_EXTERNAL_MEMORY_FEATURE_IMPORTABLE_BIT`.
const IMPORTABLE: u32 = 0x4;
/// `VK_EXTERNAL_MEMORY_FEATURE_DEDICATED_ONLY_BIT`.
const DEDICATED_ONLY: u32 = 0x1;
/// The longest name; `sv_drm::token::MAX_NAME`.
const MAX_NAME: usize = 200;
const PAGE: u64 = 4096;

struct Entry {
    nonce: [u8; 16],
    size: u64,
    /// Views of it this process holds: the creator's and every import's, of
    /// every session. The name (and on Linux the file) goes with the last.
    refs: usize,
    /// §103.19: an IOSurface section's surface, for imports of its token.
    #[cfg(target_os = "macos")]
    surface: Option<wc_handle::iosection::Surface>,
    /// The last `svFenceMark` each session's fence record has seen on the
    /// section, by record name (§104.11): what a later user of the section
    /// from another session waits for.
    marks: HashMap<String, u64>,
}

static REGISTRY: Mutex<Option<HashMap<String, Entry>>> = Mutex::new(None);
static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn registry<R>(f: impl FnOnce(&mut HashMap<String, Entry>) -> R) -> R {
    let mut g = REGISTRY.lock().unwrap_or_else(|e| e.into_inner());
    f(g.get_or_insert_with(HashMap::new))
}

/// The name rule of `sv_drm::token::name_ok`, restated here because the GPU
/// side must build for Windows and `sv-drm` is Linux-only.
pub fn name_ok(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_NAME
        && name.bytes().all(|b| b.is_ascii_alphanumeric() || b"_-./\\".contains(&b))
        && !name.contains("..")
}

/// `n` bytes nobody can predict.
fn random_bytes<const N: usize>() -> [u8; N] {
    let mut out = [0u8; N];
    #[cfg(target_os = "linux")]
    {
        let mut got = 0usize;
        while got < N {
            // SAFETY: the buffer is valid for the remaining length.
            let r = unsafe { libc::getrandom(out[got..].as_mut_ptr().cast(), N - got, 0) };
            if r <= 0 {
                break;
            }
            got += r as usize;
        }
        if got == N {
            return out;
        }
    }
    // Windows, or a getrandom that failed: `RandomState` is seeded from the
    // operating system's generator once per process and every new one
    // differs, which is unpredictable enough for a name and a nonce.
    use std::hash::{BuildHasher, Hasher};
    for (i, chunk) in out.chunks_mut(8).enumerate() {
        let mut h = std::collections::hash_map::RandomState::new().build_hasher();
        h.write_usize(i);
        h.write_u128(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos()));
        let v = h.finish().to_le_bytes();
        chunk.copy_from_slice(&v[..chunk.len()]);
    }
    out
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// Note that GPU work `seq` of fence record `record` used the section `name`
/// (§104.11). Ignored for a name this process does not have.
pub fn note_mark(name: &str, record: &str, seq: u64) {
    registry(|r| {
        if let Some(e) = r.get_mut(name) {
            let m = e.marks.entry(record.to_string()).or_insert(0);
            *m = (*m).max(seq);
        }
    });
}

/// Every mark noted on section `name`, by record.
pub fn marks_of(name: &str) -> Vec<(String, u64)> {
    registry(|r| r.get(name).map(|e| e.marks.iter().map(|(k, v)| (k.clone(), *v)).collect()).unwrap_or_default())
}

/// A file name this process makes under `/dev/shm`, as `(kind, pid)`:
/// `sv-shared-<pid>-<n>-<16 hex>` or `sv-fence-<pid>-<n>-<16 hex>`, nothing
/// else.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn parse_ours(file: &str) -> Option<(&'static str, u32)> {
    let (kind, rest) = if let Some(r) = file.strip_prefix("sv-shared-") {
        ("shared", r)
    } else {
        ("fence", file.strip_prefix("sv-fence-")?)
    };
    let mut it = rest.split('-');
    let pid = it.next()?;
    let n = it.next()?;
    let tag = it.next()?;
    let digits = |s: &str| !s.is_empty() && s.len() <= 20 && s.bytes().all(|b| b.is_ascii_digit());
    if it.next().is_some() || !digits(pid) || !digits(n) || tag.len() != 16 || !tag.bytes().all(|b| b.is_ascii_hexdigit())
    {
        return None;
    }
    Some((kind, pid.parse().ok()?))
}

/// Remove what a GPU process that died without cleaning up left in
/// `/dev/shm` (§104.10 item 5): files named exactly as [`new_name`] makes them,
/// owned by this user, whose pid is no longer a process. Once per process,
/// before its first section. A pid that was reused by another process keeps
/// its files: deleting a live process's section would be worse than leaking a
/// dead one's.
///
/// On Windows there is nothing to do: a section is a kernel object that dies
/// with the last handle to it, and a crashed process's handles are closed by
/// the kernel.
pub fn sweep_stale_once() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        #[cfg(target_os = "linux")]
        sweep_stale(std::path::Path::new("/dev/shm"));
    });
}

#[cfg(target_os = "linux")]
fn sweep_stale(dir: &std::path::Path) -> usize {
    use std::os::unix::fs::MetadataExt;
    let Ok(rd) = std::fs::read_dir(dir) else { return 0 };
    // SAFETY: plain syscall.
    let uid = unsafe { libc::geteuid() };
    let me = std::process::id();
    let mut n = 0;
    for e in rd.flatten() {
        let name = e.file_name();
        let Some((_, pid)) = name.to_str().and_then(parse_ours) else { continue };
        if pid == me || pid == 0 || pid > i32::MAX as u32 {
            continue;
        }
        // Not followed: a symlink is not a file this code made.
        let Ok(m) = std::fs::symlink_metadata(e.path()) else { continue };
        if !m.file_type().is_file() || m.uid() != uid {
            continue;
        }
        // SAFETY: signal 0 only checks that the process exists.
        let alive = unsafe { libc::kill(pid as libc::pid_t, 0) } == 0
            || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM);
        if alive {
            continue;
        }
        if std::fs::remove_file(e.path()).is_ok() {
            n += 1;
        }
    }
    if n > 0 {
        eprintln!("[gpu] removed {n} shared section file(s) of GPU processes that are gone");
    }
    n
}

/// A mapped view of a section this process holds.
pub struct View {
    #[cfg(target_os = "linux")]
    region: sv_transport::region::SharedRegion,
    /// §103.19: a POSIX shared memory object's mapping, or, for an image, an
    /// IOSurface the GPU renders into (`iosurface.rs`).
    #[cfg(target_os = "macos")]
    region: Option<sv_transport::region::SharedRegion>,
    #[cfg(target_os = "macos")]
    _ios: Option<crate::iosurface::Section>,
    #[cfg(windows)]
    section: sv_transport::winshm::Section,
    len: u64,
}

// The pointer is handed to the driver as an import, never dereferenced here
// (I3 of `SharedRegion`).
impl View {
    #[cfg(target_os = "linux")]
    pub(crate) fn base(&self) -> *mut u8 {
        // SAFETY: as above.
        unsafe { self.region.base_ptr() }
    }

    #[cfg(target_os = "macos")]
    pub(crate) fn base(&self) -> *mut u8 {
        // SAFETY: as above.
        self.region.as_ref().map_or(std::ptr::null_mut(), |r| unsafe { r.base_ptr() })
    }

    #[cfg(windows)]
    pub(crate) fn base(&self) -> *mut u8 {
        // SAFETY: as above.
        unsafe { self.section.base_ptr() }
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
    pub(crate) fn base(&self) -> *mut u8 {
        std::ptr::null_mut()
    }
}

#[cfg(target_os = "linux")]
pub(crate) fn create_section(name: &str, len: u64) -> Result<View, String> {
    sweep_stale_once();
    use std::os::fd::AsFd;
    use std::os::unix::fs::OpenOptionsExt;
    let f = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(name)
        .map_err(|e| format!("open {name}: {e}"))?;
    let made = (|| {
        f.set_len(len).map_err(|e| format!("ftruncate {name}: {e}"))?;
        let region = sv_transport::region::SharedRegion::map_fd(f.as_fd(), 0, len as usize)
            .map_err(|e| format!("mmap {name}: {e:?}"))?;
        Ok(View { region, len })
    })();
    if made.is_err() {
        let _ = std::fs::remove_file(name);
    }
    made
}

#[cfg(target_os = "linux")]
fn open_section(name: &str, len: u64) -> Result<View, String> {
    use std::os::fd::AsFd;
    let f = std::fs::OpenOptions::new().read(true).write(true).open(name).map_err(|e| format!("open {name}: {e}"))?;
    let have = f.metadata().map_err(|e| format!("stat {name}: {e}"))?.len();
    if have < len {
        return Err(format!("{name} is {have} bytes, not {len}"));
    }
    let region = sv_transport::region::SharedRegion::map_fd(f.as_fd(), 0, len as usize)
        .map_err(|e| format!("mmap {name}: {e:?}"))?;
    Ok(View { region, len })
}

/// §103.19: on the Mac a section is a POSIX shared memory object, which the
/// compositor (another process of the same user) opens by its name.
#[cfg(target_os = "macos")]
fn shm_fd(name: &str, create: bool) -> Result<std::fs::File, String> {
    use std::os::fd::FromRawFd;
    let c = std::ffi::CString::new(name).map_err(|_| format!("{name:?} has a NUL"))?;
    let flags = if create { libc::O_RDWR | libc::O_CREAT | libc::O_EXCL } else { libc::O_RDWR };
    // SAFETY: a NUL-terminated name; the mode is read only with O_CREAT.
    let fd = unsafe { libc::shm_open(c.as_ptr(), flags, 0o600 as libc::c_uint) };
    if fd < 0 {
        return Err(format!("shm_open {name}: {}", std::io::Error::last_os_error()));
    }
    // SAFETY: a descriptor shm_open just returned, owned from here.
    Ok(unsafe { std::fs::File::from_raw_fd(fd) })
}

/// The name goes with the last view this process holds; mappings, the
/// compositor's included, keep the memory until they end.
fn remove_name(name: &str) {
    #[cfg(target_os = "linux")]
    let _ = std::fs::remove_file(name);
    #[cfg(target_os = "macos")]
    if name.starts_with(wc_handle::iosection::PREFIX) {
        crate::iosurface::retire(name);
    } else if let Ok(c) = std::ffi::CString::new(name) {
        // SAFETY: a NUL-terminated name.
        unsafe { libc::shm_unlink(c.as_ptr()) };
    }
    let _ = name;
}

#[cfg(target_os = "macos")]
pub(crate) fn create_section(name: &str, len: u64) -> Result<View, String> {
    use std::os::fd::AsFd;
    let f = shm_fd(name, true)?;
    let made = (|| {
        f.set_len(len).map_err(|e| format!("ftruncate {name}: {e}"))?;
        let region = sv_transport::region::SharedRegion::map_fd(f.as_fd(), 0, len as usize)
            .map_err(|e| format!("mmap {name}: {e:?}"))?;
        Ok(View { region: Some(region), _ios: None, len })
    })();
    if made.is_err() {
        remove_name(name);
    }
    made
}

#[cfg(target_os = "macos")]
fn open_section(name: &str, len: u64) -> Result<View, String> {
    use std::os::fd::AsFd;
    let f = shm_fd(name, false)?;
    let have = f.metadata().map_err(|e| format!("stat {name}: {e}"))?.len();
    if have < len {
        return Err(format!("{name} is {have} bytes, not {len}"));
    }
    let region = sv_transport::region::SharedRegion::map_fd(f.as_fd(), 0, len as usize)
        .map_err(|e| format!("mmap {name}: {e:?}"))?;
    Ok(View { region: Some(region), _ios: None, len })
}

#[cfg(windows)]
pub(crate) fn create_section(name: &str, len: u64) -> Result<View, String> {
    let section = sv_transport::winshm::Section::create(name, len).map_err(|e| format!("{name}: {e:?}"))?;
    Ok(View { section, len })
}

#[cfg(windows)]
fn open_section(name: &str, len: u64) -> Result<View, String> {
    let section = sv_transport::winshm::Section::open(name, len).map_err(|e| format!("{name}: {e:?}"))?;
    Ok(View { section, len })
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
pub(crate) fn create_section(_name: &str, _len: u64) -> Result<View, String> {
    Err("no shared sections on this system".into())
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
fn open_section(_name: &str, _len: u64) -> Result<View, String> {
    Err("no shared sections on this system".into())
}

fn new_name() -> String {
    named("shared")
}

/// `Local\sv-<kind>-<pid>-<n>-<16 hex>`, or the same under `/dev/shm` on Linux.
pub(crate) fn named(kind: &str) -> String {
    let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let tag = hex(&random_bytes::<8>());
    let pid = std::process::id();
    if cfg!(windows) {
        format!("Local\\sv-{kind}-{pid}-{n}-{tag}")
    } else if cfg!(target_os = "macos") {
        // A shared memory object's name: at most 31 bytes on macOS.
        format!("/sv{pid:x}.{n:x}.{}", &tag[..12])
    } else {
        format!("/dev/shm/sv-{kind}-{pid}-{n}-{tag}")
    }
}

/// One section this session holds a view of, imported into its device.
///
/// **The import must be freed before this is dropped**: dropping unmaps the
/// view. `server::free_memory` and `Objects::destroy_leftovers` free the
/// `VkDeviceMemory` first.
pub struct SharedImport {
    pub name: String,
    pub memory: vk::DeviceMemory,
    pub memory_type: u32,
    view: View,
}

impl Drop for SharedImport {
    fn drop(&mut self) {
        let _ = &self.view;
        let last = registry(|r| match r.get_mut(&self.name) {
            Some(e) => {
                e.refs = e.refs.saturating_sub(1);
                if e.refs == 0 {
                    r.remove(&self.name);
                    true
                } else {
                    false
                }
            }
            None => false,
        });
        // The name goes with the last view this process holds. The compositor's
        // own mapping survives it, as any unlinked file's does; on Windows the
        // section lives while any handle to it does.
        if last {
            remove_name(&self.name);
        }
    }
}

/// The external memory handle type a shared image is created with: host
/// memory (a section's view), or on the Mac an IOSurface's Metal texture
/// (§103.19).
pub fn shared_handle_type() -> u32 {
    #[cfg(target_os = "macos")]
    return crate::iosurface::MTLTEXTURE;
    #[cfg(not(target_os = "macos"))]
    HOST_ALLOCATION
}

/// The driver's `minImportedHostPointerAlignment`, at least a page.
fn import_alignment(driver: &Driver) -> u64 {
    let mut host = vk::PhysicalDeviceExternalMemoryHostPropertiesEXT::default();
    let mut p2 = vk::PhysicalDeviceProperties2::default().push_next(&mut host);
    // SAFETY: a query on the driver's own physical device.
    unsafe { driver.instance.get_physical_device_properties2(driver.pd, &mut p2) };
    host.min_imported_host_pointer_alignment.max(PAGE)
}

fn round_up(v: u64, a: u64) -> u64 {
    v.div_ceil(a) * a
}

/// The memory type a host-pointer import of `view` gets on `driver`, narrowed
/// to `want` (an image's requirement bits), preferring `DEVICE_LOCAL`, and the
/// import itself.
fn import(driver: &Driver, view: &View, want: u32) -> Result<(vk::DeviceMemory, u32), String> {
    let ext = driver.external_host.as_ref().ok_or("VK_EXT_external_memory_host is missing")?;
    let ptr = view.base() as *mut std::ffi::c_void;
    let mut hp = vk::MemoryHostPointerPropertiesEXT::default();
    // SAFETY: the base of a live mapping this process owns, queried only.
    let r = unsafe {
        (ext.fp().get_memory_host_pointer_properties_ext)(
            driver.device.handle(),
            vk::ExternalMemoryHandleTypeFlags::HOST_ALLOCATION_EXT,
            ptr,
            &mut hp,
        )
    };
    if r != vk::Result::SUCCESS {
        return Err(format!("vkGetMemoryHostPointerPropertiesEXT: {r:?}"));
    }
    // The lowest type both the image and a host pointer accept: measured on
    // the guest's Intel UHD (host-pointer bits 0x6, image bits 0x7 -> type 1,
    // docs/kwin-gpu/extimg-probe-guest.txt on branch kwin-gpu).
    let bits = hp.memory_type_bits & want & ((1u64 << driver.mem_props.memory_type_count) - 1) as u32;
    let ty = (bits != 0)
        .then(|| bits.trailing_zeros())
        .ok_or_else(|| {
            format!(
                "no memory type a host pointer can be imported as ({:#x}) suits the image ({want:#x})",
                hp.memory_type_bits
            )
        })?;
    let mut imp = vk::ImportMemoryHostPointerInfoEXT::default()
        .handle_type(vk::ExternalMemoryHandleTypeFlags::HOST_ALLOCATION_EXT)
        .host_pointer(ptr);
    let ai = vk::MemoryAllocateInfo::default().allocation_size(view.len).memory_type_index(ty).push_next(&mut imp);
    // SAFETY: the view outlives the import (`SharedImport` holds both, and the
    // memory is freed before the view is dropped).
    let memory = unsafe { driver.device.allocate_memory(&ai, None) }
        .map_err(|e| format!("vkAllocateMemory(import a shared section): {e:?}"))?;
    Ok((memory, ty))
}

/// What an image bound to a shared allocation needs: its driver requirements,
/// or "anything" with no image.
fn image_needs(driver: &Driver, image: Option<vk::Image>) -> (u64, u64, u32) {
    match image {
        // SAFETY: the driver's own image, from the session's table.
        Some(i) if i != vk::Image::null() => {
            let r = unsafe { driver.device.get_image_memory_requirements(i) };
            (r.size, r.alignment, r.memory_type_bits)
        }
        _ => (0, 1, u32::MAX),
    }
}

/// `svAllocShared`: a new section of at least `size` bytes, imported.
/// Returns the import and the nonce.
pub fn alloc(driver: &Driver, size: u64, image: Option<vk::Image>) -> Result<(SharedImport, [u8; 16]), String> {
    let (need, align, want) = image_needs(driver, image);
    // §103.19: on the Mac a shared image renders into an IOSurface.
    #[cfg(target_os = "macos")]
    if let Some(img) = image.filter(|&i| crate::iosurface::is_shared(i)) {
        return alloc_iosurface(driver, img, want);
    }
    let a = import_alignment(driver).max(align);
    let len = round_up(size.max(need).max(1), a);
    let name = new_name();
    let view = create_section(&name, len)?;
    if !(view.base() as u64).is_multiple_of(a) {
        drop(view);
        remove_name(&name);
        return Err(format!("the view of {name} is not {a}-byte aligned"));
    }
    let nonce = random_bytes::<16>();
    // Registered before the import so that the drop of a failed one finds it.
    registry(|r| r.insert(name.clone(), Entry { nonce, size: len, refs: 1, marks: HashMap::new(), #[cfg(target_os = "macos")] surface: None }));
    match import(driver, &view, want) {
        Ok((memory, memory_type)) => Ok((SharedImport { name, memory, memory_type, view }, nonce)),
        Err(e) => {
            drop(SharedImport { name, memory: vk::DeviceMemory::null(), memory_type: 0, view });
            Err(e)
        }
    }
}

/// `svImportShared`: a section this process created, by name and nonce.
pub fn open(
    driver: &Driver,
    name: &str,
    size: u64,
    nonce: &[u8; 16],
    image: Option<vk::Image>,
) -> Result<SharedImport, String> {
    if !name_ok(name) {
        return Err(format!("{name:?} is not a section name"));
    }
    #[cfg(target_os = "macos")]
    if name.starts_with(wc_handle::iosection::PREFIX) {
        return open_iosurface(driver, name, size, nonce, image);
    }
    let len = registry(|r| match r.get_mut(name) {
        None => Err(format!("{name} is not a section this GPU process made")),
        Some(e) if &e.nonce != nonce => Err(format!("{name}: the nonce does not match")),
        Some(e) if size > e.size => Err(format!("{name} is {} bytes, {size} asked for", e.size)),
        Some(e) => {
            e.refs += 1;
            Ok(e.size)
        }
    })?;
    let (need, align, want) = image_needs(driver, image);
    let made = (|| {
        if need > len {
            return Err(format!("{name} is {len} bytes and the image needs {need}"));
        }
        let view = open_section(name, len)?;
        if !(view.base() as u64).is_multiple_of(align.max(1)) {
            return Err(format!("the view of {name} is not {align}-byte aligned"));
        }
        Ok(view)
    })();
    let view = match made {
        Ok(v) => v,
        Err(e) => {
            release_ref(name);
            return Err(e);
        }
    };
    match import(driver, &view, want) {
        Ok((memory, memory_type)) => Ok(SharedImport { name: name.to_string(), memory, memory_type, view }),
        Err(e) => {
            drop(SharedImport { name: name.to_string(), memory: vk::DeviceMemory::null(), memory_type: 0, view });
            Err(e)
        }
    }
}

/// §103.19: `alloc` for a shared image on the Mac: an IOSurface section the
/// compositor is sent at once.
#[cfg(target_os = "macos")]
fn alloc_iosurface(driver: &Driver, image: vk::Image, want: u32) -> Result<(SharedImport, [u8; 16]), String> {
    let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let name = format!("{}{}.{n}.{}", wc_handle::iosection::PREFIX, std::process::id(), hex(&random_bytes::<8>()));
    let (section, memory, memory_type) = crate::iosurface::make(driver, image, want)?;
    let len = section.surface.len() as u64;
    let nonce = random_bytes::<16>();
    if let Err(e) = crate::iosurface::announce(&name, &section.surface) {
        // Memory must retire before its retained Metal texture and IOSurface.
        unsafe { driver.device.free_memory(memory, None) };
        return Err(e);
    }
    let kept = section.surface.retain();
    registry(|r| r.insert(name.clone(), Entry { nonce, size: len, refs: 1, marks: HashMap::new(), surface: Some(kept) }));
    Ok((SharedImport { name, memory, memory_type, view: View { region: None, _ios: Some(section), len } }, nonce))
}

/// §103.19: `open` of an IOSurface section this process made, for another
/// image (an app importing its own token).
#[cfg(target_os = "macos")]
fn open_iosurface(
    driver: &Driver,
    name: &str,
    size: u64,
    nonce: &[u8; 16],
    image: Option<vk::Image>,
) -> Result<SharedImport, String> {
    let image = image.filter(|&i| crate::iosurface::is_shared(i)).ok_or_else(|| {
        format!("{name} is an IOSurface and can only be imported by a shared image")
    })?;
    let (len, surface) = registry(|r| match r.get_mut(name) {
        None => Err(format!("{name} is not a section this GPU process made")),
        Some(e) if &e.nonce != nonce => Err(format!("{name}: the nonce does not match")),
        Some(e) if size > e.size => Err(format!("{name} is {} bytes, {size} asked for", e.size)),
        Some(e) => match &e.surface {
            Some(s) => {
                e.refs += 1;
                Ok((e.size, s.retain()))
            }
            None => Err(format!("{name} has no IOSurface")),
        },
    })?;
    let (_, _, want) = image_needs(driver, Some(image));
    match crate::iosurface::import(driver, image, want, surface) {
        Ok((section, memory, memory_type)) => Ok(SharedImport {
            name: name.to_string(),
            memory,
            memory_type,
            view: View { region: None, _ios: Some(section), len },
        }),
        Err(e) => {
            release_ref(name);
            Err(e)
        }
    }
}

fn release_ref(name: &str) {
    let last = registry(|r| match r.get_mut(name) {
        Some(e) => {
            e.refs = e.refs.saturating_sub(1);
            e.refs == 0 && r.remove(name).is_some()
        }
        None => false,
    });
    if last {
        remove_name(name);
    }
}

/// Whether the driver says an image of this description can be bound to
/// imported host memory. Asked before every such `vkCreateImage`: the real
/// target is Intel's Windows driver inside a VM, where a driver fault stops the
/// VM, so it is never handed a combination it said it does not support.
pub fn image_importable(driver: &Driver, ci: &sv_proto::VkImageCreateInfo) -> Result<(), String> {
    #[cfg(target_os = "macos")]
    if !crate::iosurface::format_ok(vk::Format::from_raw(ci.format.0)) {
        return Err(format!("format {} cannot be an IOSurface", ci.format.0));
    }
    let mut ext = vk::PhysicalDeviceExternalImageFormatInfo::default()
        .handle_type(vk::ExternalMemoryHandleTypeFlags::from_raw(shared_handle_type()));
    let info = vk::PhysicalDeviceImageFormatInfo2::default()
        .format(vk::Format::from_raw(ci.format.0))
        .ty(vk::ImageType::from_raw(ci.image_type.0))
        .tiling(vk::ImageTiling::LINEAR)
        .usage(vk::ImageUsageFlags::from_raw(ci.usage.0))
        .flags(vk::ImageCreateFlags::from_raw(ci.flags.0))
        .push_next(&mut ext);
    let mut eprops = vk::ExternalImageFormatProperties::default();
    let mut props = vk::ImageFormatProperties2::default().push_next(&mut eprops);
    // SAFETY: a query on the driver's own physical device with valid chains.
    let r = unsafe { driver.instance.get_physical_device_image_format_properties2(driver.pd, &info, &mut props) };
    let p = props.image_format_properties;
    if let Err(e) = r {
        return Err(format!("the driver answers {e:?} for a LINEAR host-allocation image of this format and usage"));
    }
    let feats = eprops.external_memory_properties.external_memory_features.as_raw();
    if feats & IMPORTABLE == 0 {
        return Err(format!("the driver cannot import host memory for it (features {feats:#x})"));
    }
    if eprops.external_memory_properties.compatible_handle_types.as_raw() & shared_handle_type() == 0 {
        return Err("the driver does not list the requested import handle as compatible".into());
    }
    // An IOSurface texture is always a dedicated import (§103.19).
    if feats & DEDICATED_ONLY != 0 && shared_handle_type() == HOST_ALLOCATION {
        return Err("the driver wants a dedicated import, which a host pointer is not".into());
    }
    let e = &ci.extent;
    if e.width > p.max_extent.width || e.height > p.max_extent.height || e.depth > p.max_extent.depth {
        return Err(format!("{}x{}x{} is beyond the driver's {:?}", e.width, e.height, e.depth, p.max_extent));
    }
    if ci.mip_levels > p.max_mip_levels || ci.array_layers > p.max_array_layers {
        return Err(format!("{} levels/{} layers beyond the driver's {}/{}", ci.mip_levels, ci.array_layers, p.max_mip_levels, p.max_array_layers));
    }
    if p.sample_counts.as_raw() & (ci.samples.0 as u32) == 0 {
        return Err(format!("{} samples are not supported", ci.samples.0));
    }
    Ok(())
}

/// `VK_ERROR_OUT_OF_DEVICE_MEMORY`.
const OUT_OF_DEVICE_MEMORY: i32 = -2;
/// `VK_ERROR_INVALID_EXTERNAL_HANDLE`.
const INVALID_EXTERNAL_HANDLE: i32 = -1_000_072_003;

/// The driver image an app-side image id names, for narrowing an import.
fn driver_image(shared: &crate::server::Shared, image: u64) -> Result<Option<vk::Image>, String> {
    if image == 0 {
        return Ok(None);
    }
    shared
        .objects()
        .get::<vk::Image>(sv_proto::HandleType::VkImage, image)
        .map(Some)
        .map_err(|e| e.to_string())
}

/// Register an import under the app-side id: the `VkDeviceMemory` resolves
/// like any other (`table::resolve_memory`), so `vkBindImageMemory` lands on
/// the section.
fn register(shared: &crate::server::Shared, id: u64, imp: SharedImport) {
    let len = imp.view.len;
    let mut o = shared.objects();
    o.insert_memory(id, crate::table::MemoryRange { memory: imp.memory, base: 0, len });
    o.shared_imports.insert(id, imp);
}

#[cfg(target_os = "macos")]
fn native_video_alloc(shared: &crate::server::Shared, d: &sv_proto::sv::AllocShared)
    -> Option<Result<sv_proto::sv::AllocSharedReply, String>> {
    let image = shared.objects().native_video.images.get(&d.image).cloned()?;
    Some((|| {
        {
            let objects = shared.objects();
            if d.memory == 0 || objects.memory(d.memory).is_some()
                || !objects.native_video.exportable.contains(&d.image) {
                return Err("native NV12 export lacks dedicated external image or fresh memory identity".into());
            }
        }
        let requirements = image.requirements();
        if d.size < requirements.size || requirements.memory_type_bits & 1 == 0 {
            return Err("native NV12 export allocation requirements mismatch".into());
        }
        let memory = image.allocate().map_err(|error| format!("native NV12 allocation: {error:?}"))?;
        let lease = crate::native_video_shared::register(std::sync::Arc::clone(&image))?;
        if lease.size() < requirements.size {
            return Err("original NV12 surface is smaller than native memory requirements".into());
        }
        let reply = sv_proto::sv::AllocSharedReply {
            result: 0, memory_type: 0, size: lease.size(), nonce: lease.nonce(), name: lease.name().to_owned(),
        };
        let mut objects = shared.objects();
        objects.insert_memory(d.memory, crate::table::MemoryRange { memory, base: 0, len: reply.size });
        objects.native_video.memories.insert(d.memory, image);
        objects.native_video.leases.insert(d.memory, lease);
        Ok(reply)
    })())
}

#[cfg(target_os = "macos")]
fn native_video_import(shared: &crate::server::Shared, d: &sv_proto::sv::ImportShared)
    -> Option<Result<sv_proto::sv::ImportSharedReply, String>> {
    let image = shared.objects().native_video.images.get(&d.image).cloned()?;
    Some((|| {
        if d.memory == 0 || shared.objects().memory(d.memory).is_some() {
            return Err("native NV12 import requires fresh memory identity".into());
        }
        let (lease, picture) = crate::native_video_shared::resolve(&d.name, d.size, &d.nonce)?;
        let requirements = image.requirements();
        if d.size < requirements.size || lease.size() < requirements.size || requirements.memory_type_bits & 1 == 0 {
            return Err("native NV12 import allocation requirements mismatch".into());
        }
        let memory = image.allocate_picture(picture)
            .map_err(|error| format!("original NV12 image import: {error:?}"))?;
        let size = lease.size();
        let mut objects = shared.objects();
        objects.insert_memory(d.memory, crate::table::MemoryRange { memory, base: 0, len: size });
        objects.native_video.memories.insert(d.memory, image);
        objects.native_video.leases.insert(d.memory, lease);
        Ok(sv_proto::sv::ImportSharedReply { result: 0, memory_type: 0, size })
    })())
}

/// The `svAllocShared` arm of `session::protocol_command`.
pub fn serve_alloc(shared: &crate::server::Shared, d: sv_proto::sv::AllocShared) -> sv_proto::sv::AllocSharedReply {
    #[cfg(target_os = "macos")]
    if let Some(result) = native_video_alloc(shared, &d) {
        return result.unwrap_or_else(|error| {
            eprintln!("[gpu] original NV12 export refused: {error}");
            sv_proto::sv::AllocSharedReply { result: OUT_OF_DEVICE_MEMORY, ..Default::default() }
        });
    }
    let made = driver_image(shared, d.image).and_then(|img| alloc(&shared.driver, d.size, img));
    match made {
        Ok((imp, nonce)) => {
            let r = sv_proto::sv::AllocSharedReply {
                result: 0,
                memory_type: imp.memory_type,
                size: imp.view.len,
                nonce,
                name: imp.name.clone(),
            };
            eprintln!("[gpu] shared section {} ({} KiB, type {}) for memory {}", imp.name, imp.view.len >> 10, imp.memory_type, d.memory);
            register(shared, d.memory, imp);
            r
        }
        Err(e) => {
            eprintln!("[gpu] svAllocShared of {} bytes refused: {e}", d.size);
            sv_proto::sv::AllocSharedReply { result: OUT_OF_DEVICE_MEMORY, ..Default::default() }
        }
    }
}

/// The `svImportShared` arm of `session::protocol_command`.
pub fn serve_import(shared: &crate::server::Shared, d: sv_proto::sv::ImportShared) -> sv_proto::sv::ImportSharedReply {
    #[cfg(target_os = "macos")]
    if let Some(result) = native_video_import(shared, &d) {
        return result.unwrap_or_else(|error| {
            eprintln!("[gpu] original NV12 import refused: {error}");
            sv_proto::sv::ImportSharedReply { result: INVALID_EXTERNAL_HANDLE, ..Default::default() }
        });
    }
    let made = driver_image(shared, d.image).and_then(|img| open(&shared.driver, &d.name, d.size, &d.nonce, img));
    match made {
        Ok(imp) => {
            let r = sv_proto::sv::ImportSharedReply { result: 0, memory_type: imp.memory_type, size: imp.view.len };
            eprintln!("[gpu] shared section {} imported (type {}) for memory {}", imp.name, imp.memory_type, d.memory);
            register(shared, d.memory, imp);
            r
        }
        Err(e) => {
            eprintln!("[gpu] svImportShared of {:?} refused: {e}", d.name);
            sv_proto::sv::ImportSharedReply { result: INVALID_EXTERNAL_HANDLE, ..Default::default() }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_follow_the_token_rule_and_are_unique() {
        let a = new_name();
        let b = new_name();
        assert_ne!(a, b);
        assert!(name_ok(&a), "{a}");
        assert!(a.len() <= MAX_NAME);
        for bad in ["", "/dev/shm/../x", "a b", "C:\\x;y"] {
            assert!(!name_ok(bad));
        }
    }

    #[test]
    fn only_exact_names_are_ours() {
        assert_eq!(parse_ours("sv-shared-12-3-0011223344556677"), Some(("shared", 12)));
        assert_eq!(parse_ours("sv-fence-7-0-aabbccddeeff0011"), Some(("fence", 7)));
        for bad in [
            "sv-shared-12-3-00112233445566",
            "sv-shared-12-3-0011223344556677-x",
            "sv-shared-x-3-0011223344556677",
            "sv-shared--3-0011223344556677",
            "sv-other-1-2-0011223344556677",
            "sv-shared-1-2-001122334455667g",
            "xsv-shared-1-2-0011223344556677",
        ] {
            assert_eq!(parse_ours(bad), None, "{bad}");
        }
        let a = named("fence");
        let base = a.rsplit(['/', '\\']).next().unwrap();
        assert_eq!(parse_ours(base), Some(("fence", std::process::id())));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn the_sweep_removes_only_a_dead_pids_files() {
        let dir = std::env::temp_dir().join(format!("sv-sweep-{}-{}", std::process::id(), hex(&random_bytes::<4>())));
        std::fs::create_dir_all(&dir).unwrap();
        // A pid that cannot be alive: above the kernel's pid_max limit (4M).
        let dead = 4_194_305u32;
        let me = std::process::id();
        let files = [
            (format!("sv-shared-{dead}-0-0011223344556677"), false),
            (format!("sv-fence-{dead}-1-0011223344556677"), false),
            (format!("sv-shared-{me}-0-0011223344556677"), true),
            (format!("sv-shared-{dead}-0-0011"), true),
            (format!("other-{dead}"), true),
        ];
        for (f, _) in &files {
            std::fs::write(dir.join(f), b"x").unwrap();
        }
        assert_eq!(sweep_stale(&dir), 2);
        for (f, stays) in &files {
            assert_eq!(dir.join(f).exists(), *stays, "{f}");
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn nonces_differ() {
        assert_ne!(random_bytes::<16>(), random_bytes::<16>());
    }

    #[test]
    fn an_unknown_name_or_a_wrong_nonce_is_refused_before_anything_is_opened() {
        registry(|r| r.insert("/dev/shm/sv-shared-test-x".into(), Entry { nonce: [1; 16], size: 8192, refs: 1, marks: HashMap::new(), #[cfg(target_os = "macos")] surface: None }));
        let check = |name: &str, nonce: [u8; 16], size: u64| {
            registry(|r| match r.get(name) {
                None => Err("unknown"),
                Some(e) if e.nonce != nonce => Err("nonce"),
                Some(e) if size > e.size => Err("size"),
                Some(_) => Ok(()),
            })
        };
        assert_eq!(check("/dev/shm/sv-shared-test-y", [1; 16], 1), Err("unknown"));
        assert_eq!(check("/dev/shm/sv-shared-test-x", [2; 16], 1), Err("nonce"));
        assert_eq!(check("/dev/shm/sv-shared-test-x", [1; 16], 8193), Err("size"));
        assert_eq!(check("/dev/shm/sv-shared-test-x", [1; 16], 8192), Ok(()));
        registry(|r| r.remove("/dev/shm/sv-shared-test-x"));
    }
}
