//! The fd that stands in for a dma-buf (docs/waycompose-design.md §104).
//!
//! There is no kernel dma-buf on this path: the pixels KWin renders live in a
//! guest-side shared section that the GPU process created and the Windows
//! compositor opens by name. What Mesa, KWin and `wc-proxy` pass around as a
//! "dma-buf" is a sealed memfd that `sv-icd` makes, carrying that name. Readers
//! `pread` the header and check the seals, so the creator cannot change what a
//! token says after it was handed on.
//!
//! The file is sized to the allocation (sparse), because some importers take a
//! dma-buf's size from `lseek(fd, 0, SEEK_END)`.
//!
//! # Version 2: the sync page (§104.11)
//!
//! A version-2 token also names the GPU session's **fence record** (a small
//! section the GPU side writes "completed up to n" into), and its second page,
//! at [`SYNC_OFFSET`], is the **sync page**: the sequence number of the last
//! GPU work that wrote the allocation, stored by the creating `sv-icd` when the
//! work is submitted. A consumer that reads the sync page and then waits until
//! the record says `completed >= seq` never reads a frame the GPU has not
//! finished, and nobody had to wait when the frame was handed on.
//!
//! The header stays immutable: the file is sealed with `F_SEAL_FUTURE_WRITE`
//! rather than `F_SEAL_WRITE`, which forbids every *new* writable mapping and
//! every `write`, but keeps the one writable mapping of the sync page the
//! creator made before sealing. Nothing else can write either page.

use std::ffi::CString;
use std::io;
use std::os::fd::{AsRawFd, BorrowedFd, FromRawFd, OwnedFd};
use std::sync::atomic::{AtomicU64, Ordering};

pub use crate::token_wire::*;
use crate::token_wire::{FIXED, SYNC_LEN};

/// The seals every reader insists on, besides one of the two write seals.
const SEALS: libc::c_int = libc::F_SEAL_SHRINK | libc::F_SEAL_GROW | libc::F_SEAL_SEAL;
/// Either makes the header immutable for everyone but a mapping made before
/// sealing: version 1 tokens have `F_SEAL_WRITE`, version 2 the other.
const WRITE_SEALS: libc::c_int = libc::F_SEAL_WRITE | libc::F_SEAL_FUTURE_WRITE;

fn cvt(r: libc::c_int) -> io::Result<libc::c_int> {
    if r < 0 { Err(io::Error::last_os_error()) } else { Ok(r) }
}

/// The sync page of a version-2 token, mapped: writable in the process that
/// made the token ([`create`]), read-only everywhere else ([`SyncPage::open`]).
pub struct SyncPage {
    base: *mut u8,
}

// SAFETY: the mapping is process-wide, lives until `drop`, and its sequence
// number is only ever accessed through an atomic.
unsafe impl Send for SyncPage {}
// SAFETY: as above.
unsafe impl Sync for SyncPage {}

impl SyncPage {
    fn seq_cell(&self) -> &AtomicU64 {
        // SAFETY: 8-byte aligned (page base + 8), inside the mapping, which
        // lives as long as `self`; every access is atomic.
        unsafe { &*(self.base.add(8) as *const AtomicU64) }
    }

    /// The sequence number of the last GPU work the creator said wrote the
    /// allocation; 0 when there was none.
    pub fn load(&self) -> u64 {
        self.seq_cell().load(Ordering::Acquire)
    }

    /// Only the creator's mapping is writable; on a reader's this would fault,
    /// so it is reached only through [`Created::mark`].
    fn store(&self, seq: u64) {
        self.seq_cell().fetch_max(seq, Ordering::AcqRel);
    }

    /// Map the sync page of the token behind `fd`, read-only. `Ok(None)` for a
    /// version-1 token, which has none.
    pub fn open(fd: BorrowedFd<'_>) -> Result<Option<SyncPage>, TokenError> {
        let t = read(fd)?;
        if t.record.is_empty() {
            return Ok(None);
        }
        // SAFETY: plain syscall into a zeroed struct.
        let mut st: libc::stat = unsafe { std::mem::zeroed() };
        cvt(unsafe { libc::fstat(fd.as_raw_fd(), &mut st) })?;
        if (st.st_size as u64) < SYNC_OFFSET + SYNC_LEN as u64 {
            return Err(TokenError::BadLength);
        }
        // SAFETY: a read-only shared mapping of a sealed memfd that cannot
        // shrink (`read` checked F_SEAL_SHRINK) and is long enough (above).
        let p = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                SYNC_LEN,
                libc::PROT_READ,
                libc::MAP_SHARED,
                fd.as_raw_fd(),
                SYNC_OFFSET as libc::off_t,
            )
        };
        if p == libc::MAP_FAILED {
            return Err(io::Error::last_os_error().into());
        }
        let page = SyncPage { base: p.cast() };
        // SAFETY: inside the mapping.
        let magic = unsafe { std::slice::from_raw_parts(page.base, 8) };
        if magic != SYNC_MAGIC {
            return Err(TokenError::BadMagic);
        }
        Ok(Some(page))
    }
}

impl Drop for SyncPage {
    fn drop(&mut self) {
        // SAFETY: the mapping made in `create` or `open`, unmapped once.
        unsafe { libc::munmap(self.base.cast(), SYNC_LEN) };
    }
}

/// The creator's handle on a token: its fd (`dup`ed for every export, which is
/// what a real dma-buf export returns too: another fd for the same buffer)
/// and, for a version-2 token, the writable sync page.
pub struct Created {
    pub fd: OwnedFd,
    sync: Option<SyncPage>,
}

impl Created {
    /// A new descriptor for the same token, for `vkGetMemoryFdKHR`.
    pub fn dup(&self) -> io::Result<OwnedFd> {
        self.fd.try_clone()
    }

    /// Record that GPU work `seq` wrote the allocation (nothing for a version-1
    /// token). Monotonic: a smaller number never replaces a larger one.
    pub fn mark(&self, seq: u64) {
        if let Some(s) = &self.sync {
            s.store(seq);
        }
    }

    /// The last number [`Created::mark`] stored, or 0.
    pub fn marked(&self) -> u64 {
        self.sync.as_ref().map_or(0, SyncPage::load)
    }
}

/// Make a sealed token for `t`: version 2 with a writable sync page if
/// `t.record` is set, else version 1.
pub fn create(t: &Token) -> Result<Created, TokenError> {
    if !name_ok(&t.name) || t.size == 0 || (!t.record.is_empty() && !record_name_ok(&t.record)) {
        return Err(TokenError::BadName);
    }
    let v2 = !t.record.is_empty();
    let name = CString::new("sv-dmabuf").unwrap();
    // SAFETY: a valid C string and flags; the result is checked.
    let raw = cvt(unsafe { libc::memfd_create(name.as_ptr(), libc::MFD_CLOEXEC | libc::MFD_ALLOW_SEALING) })?;
    // SAFETY: memfd_create returned a fresh descriptor we own.
    let fd = unsafe { OwnedFd::from_raw_fd(raw) };
    let hdr = t.encode();
    let mut off = 0usize;
    while off < hdr.len() {
        // SAFETY: the buffer is valid for the length given.
        let n = unsafe {
            libc::pwrite(fd.as_raw_fd(), hdr[off..].as_ptr().cast(), hdr.len() - off, off as libc::off_t)
        };
        if n <= 0 {
            return Err(io::Error::last_os_error().into());
        }
        off += n as usize;
    }
    let min = if v2 { SYNC_OFFSET + SYNC_LEN as u64 } else { hdr.len() as u64 };
    let len = t.size.max(min);
    // SAFETY: plain syscall on a descriptor we own.
    cvt(unsafe { libc::ftruncate(fd.as_raw_fd(), len as libc::off_t) })?;
    let sync = if v2 {
        // SAFETY: a shared writable mapping of the page just sized; kept in
        // `SyncPage` and unmapped on drop.
        let p = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                SYNC_LEN,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                fd.as_raw_fd(),
                SYNC_OFFSET as libc::off_t,
            )
        };
        if p == libc::MAP_FAILED {
            return Err(io::Error::last_os_error().into());
        }
        let page = SyncPage { base: p.cast() };
        // SAFETY: inside the writable mapping; nobody else has the fd yet.
        unsafe { std::ptr::copy_nonoverlapping(SYNC_MAGIC.as_ptr(), page.base, 8) };
        Some(page)
    } else {
        None
    };
    let seals = SEALS | if v2 { libc::F_SEAL_FUTURE_WRITE } else { libc::F_SEAL_WRITE };
    // SAFETY: plain syscall on a descriptor we own.
    cvt(unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_ADD_SEALS, seals) })?;
    Ok(Created { fd, sync })
}

/// Read and check the token behind `fd`. Never maps it.
pub fn read(fd: BorrowedFd<'_>) -> Result<Token, TokenError> {
    // SAFETY: plain syscall on a borrowed descriptor.
    let seals = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GET_SEALS) };
    if seals < 0 {
        // Not a memfd at all (EINVAL): certainly not ours.
        return Err(TokenError::BadMagic);
    }
    if seals & SEALS != SEALS || seals & WRITE_SEALS == 0 {
        return Err(TokenError::Unsealed);
    }
    let mut buf = vec![0u8; FIXED + MAX_NAME + 4 + MAX_NAME];
    // SAFETY: the buffer is valid for its length.
    let n = unsafe { libc::pread(fd.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len(), 0) };
    if n < 0 {
        return Err(io::Error::last_os_error().into());
    }
    buf.truncate(n as usize);
    Token::decode(&buf)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::fd::AsFd;

    fn t(name: &str) -> Token {
        Token { size: 1280 * 800 * 4, nonce: [7; 16], name: name.into(), record: String::new() }
    }

    fn t2(name: &str) -> Token {
        Token { record: "/dev/shm/sv-fence-1-0-00112233".into(), ..t(name) }
    }

    #[test]
    fn a_token_round_trips_through_a_sealed_memfd() {
        for name in ["/dev/shm/sv-shared-1-2-0011223344556677", r"Local\sv-shared-412-3-a1b2c3d4e5f60718"] {
            for tok in [t(name), t2(name)] {
                let c = create(&tok).unwrap();
                assert_eq!(read(c.fd.as_fd()).unwrap(), tok);
                let d = c.dup().unwrap();
                assert_eq!(read(d.as_fd()).unwrap(), tok, "a dup is the same token");
                // SAFETY: lseek on our fd.
                let end = unsafe { libc::lseek(c.fd.as_raw_fd(), 0, libc::SEEK_END) };
                assert_eq!(end as u64, 1280 * 800 * 4, "importers take the size from SEEK_END");
            }
        }
    }

    #[test]
    fn a_sealed_token_cannot_be_rewritten_nor_mapped_writable() {
        for tok in [t("/dev/shm/x"), t2("/dev/shm/x")] {
            let c = create(&tok).unwrap();
            // SAFETY: pwrite on our fd, expected to fail.
            let n = unsafe { libc::pwrite(c.fd.as_raw_fd(), b"XX".as_ptr().cast(), 2, 0) };
            assert!(n < 0);
            // SAFETY: an mmap expected to fail.
            let p = unsafe {
                libc::mmap(std::ptr::null_mut(), 4096, libc::PROT_WRITE, libc::MAP_SHARED, c.fd.as_raw_fd(), 0)
            };
            assert_eq!(p, libc::MAP_FAILED, "nobody gets a new writable mapping");
        }
    }

    #[test]
    fn the_sync_page_carries_the_creators_marks_to_readers() {
        let c = create(&t2("/dev/shm/sv-shared-9-9-00")).unwrap();
        let other = c.dup().unwrap();
        let page = SyncPage::open(other.as_fd()).unwrap().expect("version 2");
        assert_eq!(page.load(), 0);
        c.mark(5);
        assert_eq!(page.load(), 5);
        c.mark(3);
        assert_eq!(page.load(), 5, "monotonic");
        assert_eq!(c.marked(), 5);
        // A version-1 token has no page, and marking it does nothing.
        let c1 = create(&t("/dev/shm/x")).unwrap();
        assert!(SyncPage::open(c1.fd.as_fd()).unwrap().is_none());
        c1.mark(9);
        assert_eq!(c1.marked(), 0);
    }

    #[test]
    fn an_unsealed_memfd_or_a_plain_file_is_refused() {
        let name = CString::new("x").unwrap();
        // SAFETY: test-only syscalls.
        let raw = unsafe { libc::memfd_create(name.as_ptr(), libc::MFD_ALLOW_SEALING) };
        let fd = unsafe { OwnedFd::from_raw_fd(raw) };
        let hdr = t("/dev/shm/x").encode();
        unsafe { libc::pwrite(fd.as_raw_fd(), hdr.as_ptr().cast(), hdr.len(), 0) };
        assert!(matches!(read(fd.as_fd()), Err(TokenError::Unsealed)));
        // Shrink, grow and seal but no write seal: refused too.
        unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_ADD_SEALS, SEALS) };
        assert!(matches!(read(fd.as_fd()), Err(TokenError::Unsealed)));
        let f = std::fs::File::open("/proc/self/exe").unwrap();
        assert!(matches!(read(f.as_fd()), Err(TokenError::BadMagic)));
    }

    #[test]
    fn names_that_could_escape_are_refused() {
        for bad in ["", "/dev/shm/../etc/passwd", "a b", "x;y", &"a".repeat(201), "Local\\x\0y"] {
            assert!(!name_ok(bad), "{bad:?}");
            assert!(create(&t(bad)).is_err());
        }
        let mut h = t("/ok").encode();
        let n = h.len();
        h[n - 1] = b' ';
        assert!(matches!(Token::decode(&h), Err(TokenError::BadName)));
        // A record must be a fence record.
        for bad in ["/dev/shm/sv-shared-1", "/etc/passwd", "Local\\other"] {
            assert!(!record_name_ok(bad), "{bad:?}");
            assert!(create(&Token { record: bad.into(), ..t("/ok") }).is_err());
        }
        assert!(record_name_ok("Local\\sv-fence-412-0-ab"));
    }

    #[test]
    fn truncated_or_lying_headers_are_refused() {
        for h in [t("/dev/shm/abc").encode(), t2("/dev/shm/abc").encode()] {
            for cut in 0..h.len() {
                assert!(Token::decode(&h[..cut]).is_err(), "cut {cut}");
            }
            let mut h2 = h.clone();
            h2[40] = 250; // name_len beyond MAX_NAME
            assert!(Token::decode(&h2).is_err());
        }
        // A version-2 header that claims version 1 has a length that lies.
        let mut h = t2("/dev/shm/abc").encode();
        h[8] = 1;
        assert!(Token::decode(&h).is_err());
    }
}
