//! The fd that stands in for a dma-buf (docs/waycompose-design.md §104).
//!
//! There is no kernel dma-buf on this path: the pixels KWin renders live in a
//! guest-side shared section that the GPU process created and the Windows
//! compositor opens by name. What Mesa, KWin and `wc-proxy` pass around as a
//! "dma-buf" is a sealed memfd that `sv-icd` makes, carrying that name. Nothing
//! maps it; readers `pread` the header and check the seals, so the creator
//! cannot change what a token says after it was handed on.
//!
//! The file is sized to the allocation (sparse), because some importers take a
//! dma-buf's size from `lseek(fd, 0, SEEK_END)`.

use std::ffi::CString;
use std::io;
use std::os::fd::{AsRawFd, BorrowedFd, FromRawFd, OwnedFd};

/// The header's first eight bytes.
pub const MAGIC: [u8; 8] = *b"SVDMABF1";
/// The one version there is.
pub const VERSION: u32 = 1;
/// The longest name a token may carry.
pub const MAX_NAME: usize = 200;
/// Fixed part of the header: magic, version, hdr_len, size, nonce, name_len.
const FIXED: usize = 8 + 4 + 4 + 8 + 16 + 4;
/// Every seal a reader insists on.
const SEALS: libc::c_int = libc::F_SEAL_SHRINK | libc::F_SEAL_GROW | libc::F_SEAL_WRITE | libc::F_SEAL_SEAL;

/// What a token says: a shared allocation of `size` bytes, named `name`, whose
/// GPU-side identity is `nonce`.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Token {
    pub size: u64,
    pub nonce: [u8; 16],
    pub name: String,
}

/// Why a fd is not a token.
#[derive(Debug)]
pub enum TokenError {
    Io(io::Error),
    /// Not sealed with all four seals: a writer could still change it.
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

impl Token {
    /// Whether the name is a unix path (`SharedFile`) rather than a section.
    pub fn is_path(&self) -> bool {
        self.name.starts_with('/')
    }

    /// The header bytes.
    pub fn encode(&self) -> Vec<u8> {
        let n = self.name.as_bytes();
        let hdr_len = (FIXED + n.len()) as u32;
        let mut v = Vec::with_capacity(hdr_len as usize);
        v.extend_from_slice(&MAGIC);
        v.extend_from_slice(&VERSION.to_le_bytes());
        v.extend_from_slice(&hdr_len.to_le_bytes());
        v.extend_from_slice(&self.size.to_le_bytes());
        v.extend_from_slice(&self.nonce);
        v.extend_from_slice(&(n.len() as u32).to_le_bytes());
        v.extend_from_slice(n);
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
        if version != VERSION {
            return Err(TokenError::BadVersion(version));
        }
        let hdr_len = u32_at(12) as usize;
        let size = u64::from_le_bytes(b[16..24].try_into().unwrap());
        let nonce: [u8; 16] = b[24..40].try_into().unwrap();
        let name_len = u32_at(40) as usize;
        if name_len > MAX_NAME || hdr_len != FIXED + name_len || b.len() < hdr_len {
            return Err(TokenError::BadLength);
        }
        let name = std::str::from_utf8(&b[FIXED..hdr_len]).map_err(|_| TokenError::BadName)?;
        if !name_ok(name) {
            return Err(TokenError::BadName);
        }
        if size == 0 {
            return Err(TokenError::BadLength);
        }
        Ok(Token { size, nonce, name: name.to_string() })
    }
}

fn cvt(r: libc::c_int) -> io::Result<libc::c_int> {
    if r < 0 { Err(io::Error::last_os_error()) } else { Ok(r) }
}

/// Make a sealed token fd for `t`. A new fd each call, as `vkGetMemoryFdKHR`
/// requires.
pub fn create(t: &Token) -> Result<OwnedFd, TokenError> {
    if !name_ok(&t.name) || t.size == 0 {
        return Err(TokenError::BadName);
    }
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
    let len = t.size.max(hdr.len() as u64);
    // SAFETY: plain syscalls on a descriptor we own.
    cvt(unsafe { libc::ftruncate(fd.as_raw_fd(), len as libc::off_t) })?;
    cvt(unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_ADD_SEALS, SEALS) })?;
    Ok(fd)
}

/// Read and check the token behind `fd`. Never maps it.
pub fn read(fd: BorrowedFd<'_>) -> Result<Token, TokenError> {
    // SAFETY: plain syscall on a borrowed descriptor.
    let seals = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GET_SEALS) };
    if seals < 0 {
        // Not a memfd at all (EINVAL): certainly not ours.
        return Err(TokenError::BadMagic);
    }
    if seals & SEALS != SEALS {
        return Err(TokenError::Unsealed);
    }
    let mut buf = vec![0u8; FIXED + MAX_NAME];
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
        Token { size: 1280 * 800 * 4, nonce: [7; 16], name: name.into() }
    }

    #[test]
    fn a_token_round_trips_through_a_sealed_memfd() {
        for name in ["/dev/shm/sv-shared-1-2-0011223344556677", r"Local\sv-shared-412-3-a1b2c3d4e5f60718"] {
            let fd = create(&t(name)).unwrap();
            assert_eq!(read(fd.as_fd()).unwrap(), t(name));
            // SAFETY: lseek on our fd.
            let end = unsafe { libc::lseek(fd.as_raw_fd(), 0, libc::SEEK_END) };
            assert_eq!(end as u64, 1280 * 800 * 4, "importers take the size from SEEK_END");
        }
    }

    #[test]
    fn a_sealed_token_cannot_be_rewritten() {
        let fd = create(&t("/dev/shm/x")).unwrap();
        // SAFETY: pwrite on our fd, expected to fail.
        let n = unsafe { libc::pwrite(fd.as_raw_fd(), b"XX".as_ptr().cast(), 2, 0) };
        assert!(n < 0);
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
    }

    #[test]
    fn truncated_or_lying_headers_are_refused() {
        let h = t("/dev/shm/abc").encode();
        for cut in 0..h.len() {
            assert!(Token::decode(&h[..cut]).is_err(), "cut {cut}");
        }
        let mut h2 = h.clone();
        h2[40] = 250; // name_len beyond MAX_NAME
        assert!(Token::decode(&h2).is_err());
    }
}
