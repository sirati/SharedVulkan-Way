//! Protected directory-fd storage. The directory must be provisioned by the
//! privileged canonical owner; opening never creates it or its state file.
use super::{DurableStore, MAX, invalid};
use std::ffi::CString;
use std::fs::File;
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::Path;

pub struct DirectoryStore {
    directory: File,
    _lock: File,
    known_state: bool,
}
impl DirectoryStore {
    /// Retain the actual protected directory object, not a path reopened for
    /// each write. All children are opened relative to that retained fd.
    ///
    /// # Safety
    /// The caller must be the privileged original backing owner and choose its
    /// single canonical state directory through trusted ancestor directories.
    /// A dedicated UID is acceptable only when it is inaccessible to desktop
    /// processes. An alias/new path cannot reset outstanding physical holds.
    pub unsafe fn open(path: &Path) -> io::Result<Self> {
        let directory = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(path)?;
        check(&directory, true)?;
        let lock = child(&directory, "lock", libc::O_RDWR | libc::O_CREAT, 0o600)?;
        check(&lock, false)?;
        // SAFETY: a retained owned file; nonblocking lifetime exclusion.
        if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self {
            directory,
            _lock: lock,
            known_state: false,
        })
    }
}
fn check(file: &File, directory: bool) -> io::Result<()> {
    let m = file.metadata()?;
    // SAFETY: no arguments.
    if m.uid() != unsafe { libc::geteuid() }
        || m.mode() & 0o077 != 0
        || (directory && !m.is_dir())
        || (!directory && (!m.is_file() || m.nlink() != 1))
    {
        return Err(invalid("untrusted BAR registry ownership/type/permissions"));
    }
    Ok(())
}
fn child(directory: &File, name: &str, flags: i32, mode: u32) -> io::Result<File> {
    let name = CString::new(name).unwrap();
    // SAFETY: retained directory fd, fixed child name and valid mode.
    let fd = unsafe {
        libc::openat(
            directory.as_raw_fd(),
            name.as_ptr(),
            flags | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            mode,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: newly owned fd, closed exactly once.
    Ok(unsafe { File::from_raw_fd(fd) })
}
// SAFETY: exclusive lock is retained, all state is bound to the protected
// directory fd, and replacement flushes the file then the directory entry.
unsafe impl DurableStore for DirectoryStore {
    fn load(&mut self) -> io::Result<Vec<u8>> {
        let file = child(&self.directory, "state", libc::O_RDONLY, 0)?;
        check(&file, false)?;
        if file.metadata()?.len() > MAX as u64 {
            return Err(invalid("BAR registry too large"));
        }
        let mut bytes = Vec::new();
        file.take(MAX as u64 + 1).read_to_end(&mut bytes)?;
        if bytes.len() > MAX {
            return Err(invalid("BAR registry too large"));
        }
        self.known_state = true;
        Ok(bytes)
    }
    fn replace_synced(&mut self, bytes: &[u8]) -> io::Result<()> {
        // An interrupted write may leave pending. Never follow/truncate an
        // attacker-selected object or mistake pending for committed history.
        match child(&self.directory, "pending", libc::O_RDONLY, 0) {
            Ok(file) => {
                check(&file, false)?;
                unlink(&self.directory, "pending")?;
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
        // Refuse replacement of a preexisting untrusted/symlink state object.
        match child(&self.directory, "state", libc::O_RDONLY, 0) {
            Ok(file) => check(&file, false)?,
            Err(e) if e.kind() == io::ErrorKind::NotFound && !self.known_state => {}
            Err(e) => return Err(e),
        }
        let mut file = child(
            &self.directory,
            "pending",
            libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL,
            0o600,
        )?;
        check(&file, false)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        let old = CString::new("pending").unwrap();
        let new = CString::new("state").unwrap();
        // SAFETY: both fixed names relative to the same retained directory.
        if unsafe {
            libc::renameat(
                self.directory.as_raw_fd(),
                old.as_ptr(),
                self.directory.as_raw_fd(),
                new.as_ptr(),
            )
        } != 0
        {
            return Err(io::Error::last_os_error());
        }
        self.directory.sync_all()?;
        self.known_state = true;
        Ok(())
    }
}
fn unlink(directory: &File, name: &str) -> io::Result<()> {
    let name = CString::new(name).unwrap();
    // SAFETY: retained directory and fixed child name.
    if unsafe { libc::unlinkat(directory.as_raw_fd(), name.as_ptr(), 0) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}
