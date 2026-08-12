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
}

impl Drop for OwnedObject {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
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
        return format!("{}/wc-weak-{}", std::env::temp_dir().display(), hex(&bytes));
    }
    format!("{}/wc-{}", std::env::temp_dir().display(), hex(&bytes))
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
    Ok(OwnedObject { file, path })
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

/// Write into an object this process owns.
///
/// `pwrite` rather than a writable mapping: it needs no `unsafe`, and the
/// offsets involved are small and infrequent — this path exists to seed a
/// keymap or a test pattern, not to blit frames.
pub fn write_section(object: &OwnedObject, offset: usize, bytes: &[u8]) -> io::Result<()> {
    object.file.write_all_at(bytes, offset as u64)
}
