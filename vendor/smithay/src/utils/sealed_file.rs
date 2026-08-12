use std::ffi::CStr;

#[cfg(unix)]
use std::{fs::File, io::Write};

use crate::utils::os::{AsFd, BorrowedFd};

/// A file whose fd cannot be written by other processes
///
/// This mechanism is useful for giving clients access to large amounts of
/// information such as keymaps without them being able to write to the handle.
///
/// On Linux, Android, and FreeBSD, this uses a sealed memfd. On other Unix
/// platforms it creates a POSIX shared memory object with `shm_open`, opens a
/// read-only copy, and unlinks it. On Windows it is a named section object —
/// see the note on [`SealedFile::with_data`] there, because "sealed" is weaker
/// on that platform and it matters.
#[cfg(unix)]
#[derive(Debug)]
pub struct SealedFile {
    file: File,
    size: usize,
}

/// See the Unix definition above.
///
/// The Windows form holds a `wc_handle::Handle` rather than a `File`, because
/// what crosses the wire as an `h` argument here is a named section object and
/// not a descriptor (docs/design.md §2.2). The handle must be kept alive for as
/// long as the protocol object it backs, or the name refers to nothing — that
/// is design §2.5's lifetime rule, and holding it in this struct is how this
/// call site obeys it.
#[cfg(windows)]
#[derive(Debug)]
pub struct SealedFile {
    handle: crate::utils::os::OwnedFd,
    size: usize,
}

impl SealedFile {
    /// Create a `[SealedFile]` with the given nul-terminated C string.
    pub fn with_content(name: &CStr, contents: &CStr) -> Result<Self, std::io::Error> {
        Self::with_data(name, contents.to_bytes_with_nul())
    }

    /// Create a `[SealedFile]` with the given binary data.
    #[cfg(any(target_os = "linux", target_os = "freebsd", target_os = "android"))]
    pub fn with_data(name: &CStr, data: &[u8]) -> Result<Self, std::io::Error> {
        use rustix::fs::{MemfdFlags, SealFlags};
        use std::io::Seek;

        let fd = rustix::fs::memfd_create(name, MemfdFlags::CLOEXEC | MemfdFlags::ALLOW_SEALING)?;

        let mut file: File = fd.into();
        file.write_all(data)?;
        file.flush()?;

        file.seek(std::io::SeekFrom::Start(0))?;

        rustix::fs::fcntl_add_seals(
            &file,
            SealFlags::SEAL | SealFlags::SHRINK | SealFlags::GROW | SealFlags::WRITE,
        )?;

        Ok(Self {
            file,
            size: data.len(),
        })
    }

    /// Create a `[SealedFile]` with the given binary data.
    #[cfg(all(
        unix,
        not(any(target_os = "linux", target_os = "freebsd", target_os = "android"))
    ))]
    pub fn with_data(name: &CStr, data: &[u8]) -> Result<Self, std::io::Error> {
        use rand::{distr::Alphanumeric, Rng};
        use rustix::{
            io::Errno,
            shm::{self, Mode},
        };

        let mut rng = rand::rng();

        // `memfd_create` isn't available. Instead, try `shm_open` with a randomized name, and
        // loop a couple times if it exists.
        let mut n = 0;
        let (shm_name, mut file) = loop {
            let mut shm_name = name.to_bytes().to_owned();
            shm_name.push(b'-');
            shm_name.extend((0..7).map(|_| rng.sample(Alphanumeric)));
            let fd = shm::open(
                shm_name.as_slice(),
                shm::OFlags::RDWR | shm::OFlags::CREATE | shm::OFlags::EXCL,
                Mode::RWXU,
            );
            if !matches!(fd, Err(Errno::EXIST)) || n > 3 {
                break (shm_name, File::from(fd?));
            }
            n += 1;
        };

        // Sealing isn't available, so re-open read-only.
        let fd_rdonly = shm::open(shm_name.as_slice(), shm::OFlags::RDONLY, Mode::empty())?;
        let file_rdonly = File::from(fd_rdonly);

        // Unlink so another process can't open shm file.
        let _ = shm::unlink(shm_name.as_slice());

        file.write_all(data)?;
        file.flush()?;

        Ok(Self {
            file: file_rdonly,
            size: data.len(),
        })
    }

    /// Create a `[SealedFile]` with the given binary data.
    ///
    /// # "Sealed" is weaker here than on Linux, and this is the honest statement
    ///
    /// A sealed `memfd` cannot be written by anyone, including a client that
    /// holds the fd: `F_SEAL_WRITE` is enforced by the kernel against the
    /// object, not against a particular descriptor.
    ///
    /// A Windows named section has no equivalent. The compositor creates it
    /// `PAGE_READWRITE` (it has to write the keymap into it) and the client is
    /// handed the *name*. `wc_handle` only ever maps read-only, so nothing in
    /// this project writes through it — but a client that chose to call
    /// `OpenFileMappingW(FILE_MAP_WRITE, …)` on the name it was given could
    /// modify the keymap other clients then read.
    ///
    /// This is a real regression against the Unix model and is recorded rather
    /// than papered over. The fix is a security descriptor on the section
    /// granting `FILE_MAP_READ` only, which is a `wc_handle` change; it is not
    /// done here because `wc_handle::create_section` has no DACL parameter yet
    /// and inventing one at this call site would put the security decision in
    /// the wrong module. Until then, treat the keymap as integrity-protected
    /// only against accident, not against a hostile client.
    ///
    /// `name` is ignored: section names carry 128 bits from `BCryptGenRandom`
    /// (design §2.5) so that they cannot be guessed, and a caller-chosen name
    /// would defeat that.
    #[cfg(windows)]
    pub fn with_data(_name: &CStr, data: &[u8]) -> Result<Self, std::io::Error> {
        let handle = crate::utils::os::OwnedFd::create_section(data.len() as u64)?;
        handle.write_at(0, data)?;
        Ok(Self {
            handle,
            size: data.len(),
        })
    }

    /// Size of the data contained in the sealed file.
    pub fn size(&self) -> usize {
        self.size
    }
}

// `AsRawFd` is Unix-only: on Windows the raw and owned forms are the same
// refcounted handle, so the trait would carry no information. See
// `crate::utils::os`.
#[cfg(unix)]
impl std::os::unix::io::AsRawFd for SealedFile {
    fn as_raw_fd(&self) -> std::os::unix::io::RawFd {
        std::os::unix::io::AsRawFd::as_raw_fd(&self.file)
    }
}

#[cfg(unix)]
impl AsFd for SealedFile {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.file.as_fd()
    }
}

#[cfg(windows)]
impl AsFd for SealedFile {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.handle.as_fd()
    }
}
