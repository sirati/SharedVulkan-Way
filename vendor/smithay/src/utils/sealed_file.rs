use std::ffi::CStr;

#[cfg(unix)]
use std::fs::File;
#[cfg(any(target_os = "linux", target_os = "freebsd", target_os = "android"))]
use std::io::Write;

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
    /// §80: the **named** twin, minted only if an in-band peer ever asks for one.
    ///
    /// A `memfd` is a descriptor and nothing else; a peer reached through
    /// `wc-proxy` is handed a *name* and opens it itself. Those are different
    /// objects, so serving both means having both — and the second one is worth
    /// a file on disk only for the peers that need it, which is why this is
    /// lazy rather than made alongside the `memfd`. A compositor serving only
    /// stock clients never creates one.
    ///
    /// `Mutex` rather than `OnceLock` because minting can fail and a failure
    /// must be retryable: `publish::mint()` depends on `--shared-dir`, and a
    /// compositor started without one should not have the first keymap poison
    /// every later attempt.
    named: std::sync::Mutex<Option<crate::utils::os::Handle>>,
    /// The contents, kept where the file cannot be read back: a POSIX shm
    /// object on macOS supports neither `read(2)` nor `write(2)` (waycompose
    /// §103.11). `None` where the file is a memfd.
    bytes: Option<Vec<u8>>,
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
            named: std::sync::Mutex::new(None),
            bytes: None,
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

        // waycompose §103.11: macOS shm objects take no `write(2)`; they are
        // sized once with `ftruncate` and filled through a mapping. That is
        // also correct on the BSDs, so it is not conditional.
        let _ = &mut file;
        file.set_len(data.len().max(1) as u64)?;
        if !data.is_empty() {
            // SAFETY: a fresh shared mapping of an object this function just
            // sized to `data.len()`, written once and unmapped before return.
            unsafe {
                let ptr = rustix::mm::mmap(
                    std::ptr::null_mut(),
                    data.len(),
                    rustix::mm::ProtFlags::READ | rustix::mm::ProtFlags::WRITE,
                    rustix::mm::MapFlags::SHARED,
                    &file,
                    0,
                )?;
                std::ptr::copy_nonoverlapping(data.as_ptr(), ptr.cast::<u8>(), data.len());
                let _ = rustix::mm::munmap(ptr, data.len());
            }
        }

        Ok(Self {
            file: file_rdonly,
            size: data.len(),
            named: std::sync::Mutex::new(None),
            bytes: Some(data.to_vec()),
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
    ///
    /// # `create_shared`, not `create_section` — STAGE 6
    ///
    /// This used to call `create_section`, and that was wrong the moment
    /// anything actually sent one. A section is `Local\wc-…`, a name only the
    /// NT object manager understands; the peer that opens a keymap is a Linux
    /// process on the far side of the boundary, which can neither open nor
    /// spell one. `create_shared` asks `wc_handle::publish` where objects made
    /// for a peer should go, and lands the keymap in a directory both sides
    /// reach when the compositor was told about one — falling back to a section
    /// when it was not, which is right for two Windows processes.
    ///
    /// Nothing else about this function changes, including the honest note
    /// above: the object is still writable by anyone who learns its name, and a
    /// file in a shared directory is *more* reachable than a session-local
    /// section, not less.
    #[cfg(windows)]
    pub fn with_data(_name: &CStr, data: &[u8]) -> Result<Self, std::io::Error> {
        let handle = crate::utils::os::OwnedFd::create_shared(data.len() as u64)?;
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

impl SealedFile {
    /// Run `f` with the `h` argument a `wl_keyboard.keymap` should carry for this
    /// file, in the dialect `encoding` names.
    ///
    /// §80 replaced a `proto_fd()` that could only ever answer with the open form
    /// on Unix, and it is a closure rather than a getter for one reason: the
    /// named arm borrows a handle that has to stay alive for the call, and
    /// returning `BorrowedWireFd::Named(&handle)` out of a function would return
    /// a reference into a local.
    ///
    /// **The named object is minted here, not at construction.** See the `named`
    /// field: a compositor serving only stock clients never creates one.
    pub fn with_proto_fd<F, T>(
        &self,
        encoding: crate::utils::os::FdEncoding,
        f: F,
    ) -> Result<T, std::io::Error>
    where
        F: FnOnce(crate::utils::os::BorrowedProtoFd<'_>) -> T,
    {
        match encoding {
            crate::utils::os::FdEncoding::InBandLease(arena) => {
                let handle = crate::utils::os::Handle::create_bar_outbound(arena, self.size)?;
                #[cfg(unix)]
                {
                    use std::os::unix::fs::FileExt;
                    if let Some(bytes) = &self.bytes { handle.write_at(0, bytes)?; }
                    else {
                        let mut bytes = vec![0; self.size];
                        self.file.read_exact_at(&mut bytes, 0)?;
                        handle.write_at(0, &bytes)?;
                    }
                }
                #[cfg(windows)]
                handle.write_at(0, self.handle.map()?.as_slice())?;
                Ok(f(crate::utils::os::BorrowedProtoFd::Named(&handle)))
            }
            crate::utils::os::FdEncoding::InBandArena(_) => Err(std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                "this connection only permits its authenticated BAR arena",
            )),
            crate::utils::os::FdEncoding::InBand => {
                let handle = self.named()?;
                Ok(f(crate::utils::os::BorrowedProtoFd::Named(&handle)))
            }
            #[cfg(unix)]
            crate::utils::os::FdEncoding::OutOfBand => {
                use crate::utils::os::AsFd as _;
                Ok(f(crate::utils::os::BorrowedProtoFd::Open(self.file.as_fd())))
            }
            #[cfg(windows)]
            crate::utils::os::FdEncoding::OutOfBand => Err(std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                "there is no out-of-band descriptor on this platform",
            )),
        }
    }

    /// The named form of this file, minted on first use.
    ///
    /// On Windows the object **is** the named form and there is nothing to mint.
    /// On Unix the sealed file is a `memfd`, which is a descriptor and has no
    /// name a peer could open, so a second object is created holding the same
    /// bytes — `wc_handle::Handle::create_shared`, which is the same call the
    /// Windows arm has always made and lands in `--shared-dir` where the peer
    /// can reach it.
    ///
    /// The two objects are **not** kept in sync and do not need to be: a
    /// `SealedFile` is immutable by construction, which is what "sealed" means.
    fn named(&self) -> Result<crate::utils::os::Handle, std::io::Error> {
        #[cfg(windows)]
        {
            Ok(self.handle.clone())
        }
        #[cfg(unix)]
        {
            use std::io::{Read, Seek};
            let mut slot = self.named.lock().unwrap();
            if let Some(handle) = slot.as_ref() {
                return Ok(handle.clone());
            }
            // Read the bytes back out of the sealed file rather than keeping a
            // second copy alive for the lifetime of every keymap: this runs once
            // per keymap per compositor, and a keymap is tens of kilobytes.
            let bytes = if let Some(b) = &self.bytes {
                b.clone()
            } else {
                let mut file = &self.file;
                let mut bytes = Vec::with_capacity(self.size);
                file.seek(std::io::SeekFrom::Start(0))?;
                file.read_to_end(&mut bytes)?;
                file.seek(std::io::SeekFrom::Start(0))?;
                bytes
            };
            let handle = crate::utils::os::Handle::create_shared(bytes.len() as u64)?;
            handle.write_at(0, &bytes)?;
            *slot = Some(handle.clone());
            Ok(handle)
        }
    }
}
