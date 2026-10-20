//! The bounded immutable identity shared by native and Linux PRIME adapters.
use std::io;

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
pub(crate) const FIXED: usize = 8 + 4 + 4 + 8 + 16 + 4;
/// Where the sync page starts. The header fits in the first page.
pub const SYNC_OFFSET: u64 = 4096;
/// The sync page's first eight bytes; the sequence number follows.
pub const SYNC_MAGIC: [u8; 8] = *b"SVSYNC01";
#[cfg(target_os = "linux")]
pub(crate) const SYNC_LEN: usize = 4096;
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

