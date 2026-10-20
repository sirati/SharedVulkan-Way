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

/// The header's first eight bytes.
pub const MAGIC: [u8; 8] = *b"SVDMABF1";
/// The first version: a name, no sync page.
pub const VERSION_1: u32 = 1;
/// The version [`create`] writes when there is a record: a name, the fence
/// record, a sync page.
pub const VERSION: u32 = 2;
/// The longest name a token may carry.
pub const MAX_NAME: usize = 200;
/// Fixed part of the header: magic, version, hdr_len, size, nonce, name_len.
const FIXED: usize = 8 + 4 + 4 + 8 + 16 + 4;
/// Where the sync page starts. The header fits in the first page.
pub const SYNC_OFFSET: u64 = 4096;
/// The sync page's first eight bytes; the sequence number follows.
pub const SYNC_MAGIC: [u8; 8] = *b"SVSYNC01";
const SYNC_LEN: usize = 4096;
/// The seals every reader insists on, besides one of the two write seals.
const SEALS: libc::c_int = libc::F_SEAL_SHRINK | libc::F_SEAL_GROW | libc::F_SEAL_SEAL;
/// Either makes the header immutable for everyone but a mapping made before
/// sealing: version 1 tokens have `F_SEAL_WRITE`, version 2 the other.
const WRITE_SEALS: libc::c_int = libc::F_SEAL_WRITE | libc::F_SEAL_FUTURE_WRITE;

/// What a token says: a shared allocation of `size` bytes, named `name`, whose
/// GPU-side identity is `nonce`; and, from version 2, the fence record of the
/// GPU session that made it (`record`, empty for version 1).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Token {
    pub size: u64,
    pub nonce: [u8; 16],
    pub name: String,
    pub record: String,
}

/// Why a fd is not a token.
#[derive(Debug)]
pub enum TokenError {
    Io(io::Error),
    /// Not sealed against shrinking, growing, writing and resealing: a writer
    /// could still change it.
    Unsealed,
    BadMagic,
    BadVersion(u32),
    BadLength,
    BadName,
}

impl std::fmt::Display for TokenError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TokenError::Io(e) => write!(f, "token: {e}"),
            TokenError::Unsealed => write!(f, "token: not sealed"),
            TokenError::BadMagic => write!(f, "token: not an sv dma-buf token"),
            TokenError::BadVersion(v) => write!(f, "token: version {v}"),
            TokenError::BadLength => write!(f, "token: bad length"),
            TokenError::BadName => write!(f, "token: bad name"),
        }
    }
}

impl std::error::Error for TokenError {}

impl From<io::Error> for TokenError {
    fn from(e: io::Error) -> Self {
        TokenError::Io(e)
    }
}

/// The name rule: an absolute unix path (local runs, the compositor under Wine
/// maps it through `Z:`), or a Windows section name. Nothing that could escape a
/// path or a namespace in a way the two openers do not already allow.
pub fn name_ok(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_NAME
        && name.bytes().all(|b| b.is_ascii_alphanumeric() || b"_-./\\".contains(&b))
        && !name.contains("..")
}

/// A fence record's name: the section rule, and a basename that starts with
/// `sv-fence-`, so that a token cannot make a consumer open any other object
/// as a record.
pub fn record_name_ok(name: &str) -> bool {
    name_ok(name) && name.rsplit(['/', '\\']).next().is_some_and(|b| b.starts_with("sv-fence-"))
}

impl Token {
    /// Whether the name is a unix path (`SharedFile`) rather than a section.
    pub fn is_path(&self) -> bool {
        self.name.starts_with('/')
    }

    /// The header bytes: version 2 when there is a record, else version 1.
    pub fn encode(&self) -> Vec<u8> {
        let n = self.name.as_bytes();
        let r = self.record.as_bytes();
        let v2 = !r.is_empty();
        let hdr_len = (FIXED + n.len() + if v2 { 4 + r.len() } else { 0 }) as u32;
        let mut v = Vec::with_capacity(hdr_len as usize);
        v.extend_from_slice(&MAGIC);
        v.extend_from_slice(&(if v2 { VERSION } else { VERSION_1 }).to_le_bytes());
        v.extend_from_slice(&hdr_len.to_le_bytes());
        v.extend_from_slice(&self.size.to_le_bytes());
        v.extend_from_slice(&self.nonce);
        v.extend_from_slice(&(n.len() as u32).to_le_bytes());
        v.extend_from_slice(n);
        if v2 {
            v.extend_from_slice(&(r.len() as u32).to_le_bytes());
            v.extend_from_slice(r);
        }
        v
    }

    /// Parse header bytes (at least the whole header).
    pub fn decode(b: &[u8]) -> Result<Token, TokenError> {
        if b.len() < FIXED {
            return Err(TokenError::BadLength);
        }
        if b[..8] != MAGIC {
            return Err(TokenError::BadMagic);
        }
        let u32_at = |o: usize| u32::from_le_bytes(b[o..o + 4].try_into().unwrap());
        let version = u32_at(8);
        if version != VERSION_1 && version != VERSION {
            return Err(TokenError::BadVersion(version));
        }
        let hdr_len = u32_at(12) as usize;
        let size = u64::from_le_bytes(b[16..24].try_into().unwrap());
        let nonce: [u8; 16] = b[24..40].try_into().unwrap();
        let name_len = u32_at(40) as usize;
        if name_len > MAX_NAME || b.len() < hdr_len || hdr_len < FIXED + name_len {
            return Err(TokenError::BadLength);
        }
        let name_end = FIXED + name_len;
        let name = std::str::from_utf8(&b[FIXED..name_end]).map_err(|_| TokenError::BadName)?;
        if !name_ok(name) {
            return Err(TokenError::BadName);
        }
        let record = if version == VERSION_1 {
            if hdr_len != name_end {
                return Err(TokenError::BadLength);
            }
            String::new()
        } else {
            if hdr_len < name_end + 4 {
                return Err(TokenError::BadLength);
            }
            let rec_len = u32_at(name_end) as usize;
            if rec_len > MAX_NAME || hdr_len != name_end + 4 + rec_len {
                return Err(TokenError::BadLength);
            }
            let rec = std::str::from_utf8(&b[name_end + 4..hdr_len]).map_err(|_| TokenError::BadName)?;
            if !record_name_ok(rec) {
                return Err(TokenError::BadName);
            }
            rec.to_string()
        };
        if size == 0 {
            return Err(TokenError::BadLength);
        }
        Ok(Token { size, nonce, name: name.to_string(), record })
    }
}

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
