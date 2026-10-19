//! §103.15: the boot record, which is how a macOS session starts with no ssh.
//!
//! A guest with no shared-memory device (the macOS VM, role (a)) gives the
//! host two windows of its own RAM to locate: the arena and the ring region.
//! Until §103.15 the launcher told the host their run ids over three ssh
//! execs. Now page 0 of the ring window says it itself, in this record, beside
//! the stamp the host's sweep finds that page by:
//!
//! ```text
//!   page 0 of the ring window
//!   0     SVARENA1, run id, page index 0      the stamp (arena.rs)
//!   1024  WCMBOOT1 ...                        this record, written by the guest
//!   1152  state                               written by the host, last
//!   1160  host message (512 bytes)            written by the host, first
//! ```
//!
//! The host's session service (`wc-arena-host mac-sessiond`) sweeps guest RAM
//! for stamps with page index 0 and this magic, locates the two windows,
//! starts the session, and then writes its answer into the same page: the
//! message first, the state word last. The guest reads the state word of its
//! own page. Nothing else crosses, and the answer cannot reach the wrong
//! session because it is written into that session's own memory.
//!
//! Every field the guest writes is checked by the host as untrusted input:
//! the session id is the same alphabet `wc-session`'s `check_id` takes, and
//! the counts are bounded by the host's own sweep.

/// Where the record starts in page 0: past the stamp, clear of its fields.
pub const OFF: usize = 1024;
/// The record's magic.
pub const MAGIC: [u8; 8] = *b"WCMBOOT1";
/// The version this module writes and reads.
pub const VERSION: u32 = 1;
/// Longest session id.
pub const ID_MAX: usize = 64;
/// Offset of the host's state word, from the start of the page.
pub const OFF_STATE: usize = OFF + 128;
/// Offset of the host's message, from the start of the page.
pub const OFF_MESSAGE: usize = OFF + 136;
/// Bytes of host message.
pub const MESSAGE_MAX: usize = 512;

/// The host's answer, in the state word.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    /// Nothing yet.
    Waiting,
    /// Both windows located and the session started: the guest may write the
    /// control region over the stamps.
    Located,
    /// The host refused or failed; the message says why.
    Failed,
}

impl State {
    pub fn word(self) -> u64 {
        match self {
            State::Waiting => 0,
            // Words no stale page is likely to hold by chance.
            State::Located => 0x4C4F_4341_5445_4431,
            State::Failed => 0x4641_494C_4544_2121,
        }
    }

    pub fn from_word(w: u64) -> State {
        if w == State::Located.word() {
            State::Located
        } else if w == State::Failed.word() {
            State::Failed
        } else {
            State::Waiting
        }
    }
}

/// What the guest says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Record {
    pub session: String,
    /// The arena window's run id and page count.
    pub arena_run: u64,
    pub arena_pages: u32,
    /// The bytes at the arena's end the compositor keeps for its own objects.
    pub outbound: u64,
    /// The ring window's run id (also in the page's stamp) and page count.
    pub rings_run: u64,
    pub rings_pages: u32,
    /// The main process started the session's GPU server in the guest, whose
    /// cold page carries the label [`gpu_label`] gives: the host is to serve
    /// it (`wc-session gpu`).
    pub gpu: bool,
}

/// The label a session's GPU server offers its cold page under: the
/// session id, cut to the cold page's sixteen bytes.
pub fn gpu_label(session: &str) -> &str {
    let mut end = session.len().min(16);
    while !session.is_char_boundary(end) {
        end -= 1;
    }
    &session[..end]
}

/// Whether a session id is one `wc-session` will take (`check_id`).
pub fn valid_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= ID_MAX && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

impl Record {
    /// Write the record into page 0 (`page` is the whole page) and clear the
    /// host's fields.
    pub fn write(&self, page: &mut [u8]) {
        let r = &mut page[OFF..OFF_MESSAGE + MESSAGE_MAX];
        r.fill(0);
        r[0..8].copy_from_slice(&MAGIC);
        r[8..12].copy_from_slice(&VERSION.to_le_bytes());
        r[12..16].copy_from_slice(&u32::from(self.gpu).to_le_bytes());
        let id = self.session.as_bytes();
        let n = id.len().min(ID_MAX);
        r[16..16 + n].copy_from_slice(&id[..n]);
        r[80..88].copy_from_slice(&self.arena_run.to_le_bytes());
        r[88..92].copy_from_slice(&self.arena_pages.to_le_bytes());
        r[96..104].copy_from_slice(&self.outbound.to_le_bytes());
        r[104..112].copy_from_slice(&self.rings_run.to_le_bytes());
        r[112..116].copy_from_slice(&self.rings_pages.to_le_bytes());
    }

    /// Read a record from page 0, or `None` for a page without one or with
    /// one the host must not act on.
    pub fn read(page: &[u8]) -> Option<Record> {
        let r = page.get(OFF..OFF_MESSAGE + MESSAGE_MAX)?;
        if r[0..8] != MAGIC || u32::from_le_bytes(r[8..12].try_into().ok()?) != VERSION {
            return None;
        }
        let id = &r[16..16 + ID_MAX];
        let end = id.iter().position(|b| *b == 0).unwrap_or(ID_MAX);
        let session = std::str::from_utf8(&id[..end]).ok()?.to_string();
        if !valid_id(&session) {
            return None;
        }
        let u64_at = |o: usize| u64::from_le_bytes(r[o..o + 8].try_into().unwrap_or([0; 8]));
        let u32_at = |o: usize| u32::from_le_bytes(r[o..o + 4].try_into().unwrap_or([0; 4]));
        let rec = Record {
            session,
            arena_run: u64_at(80),
            arena_pages: u32_at(88),
            outbound: u64_at(96),
            rings_run: u64_at(104),
            rings_pages: u32_at(112),
            gpu: u32_at(12) & 1 != 0,
        };
        (rec.arena_run != 0 && rec.rings_run != 0 && rec.arena_pages > 0 && rec.rings_pages > 0).then_some(rec)
    }
}

/// The state word of page 0, read by the host's sweep.
pub fn state(page: &[u8]) -> State {
    page.get(OFF_STATE..OFF_STATE + 8)
        .and_then(|b| b.try_into().ok())
        .map_or(State::Waiting, |b| State::from_word(u64::from_le_bytes(b)))
}

/// The host's message, as the guest reads it.
pub fn message(page: &[u8]) -> String {
    let m = page.get(OFF_MESSAGE..OFF_MESSAGE + MESSAGE_MAX).unwrap_or(&[]);
    let end = m.iter().position(|b| *b == 0).unwrap_or(m.len());
    String::from_utf8_lossy(&m[..end]).into_owned()
}

/// The host's message as the bytes it writes at [`OFF_MESSAGE`]: truncated on
/// a character boundary and NUL-terminated.
pub fn message_bytes(text: &str) -> Vec<u8> {
    let mut end = text.len().min(MESSAGE_MAX - 1);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    let mut v = text.as_bytes()[..end].to_vec();
    v.push(0);
    v
}

/// The guest's view of its own page 0: the state word, read with an acquire
/// load, because the host writes it after the message.
///
/// # Safety
/// `page0` must point at the mapped page 0 of the ring window, 8-aligned at
/// [`OFF_STATE`].
pub unsafe fn load_state(page0: *const u8) -> State {
    // SAFETY: the caller's contract; the word is 8-aligned (page-aligned base
    // plus a multiple of 8).
    let w = unsafe { &*(page0.add(OFF_STATE) as *const std::sync::atomic::AtomicU64) };
    State::from_word(w.load(std::sync::atomic::Ordering::Acquire))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn page() -> Vec<u8> {
        vec![0xAB; 4096]
    }

    #[test]
    fn a_record_reads_back() {
        let r = Record {
            session: "wcm-1234-abcd".into(),
            arena_run: 0x1122_3344_5566_7788,
            arena_pages: 16384,
            outbound: 1 << 20,
            rings_run: 0x99,
            rings_pages: 1536,
            gpu: true,
        };
        let mut p = page();
        r.write(&mut p);
        assert_eq!(Record::read(&p), Some(r));
        assert_eq!(state(&p), State::Waiting);
        // The stamp's fields are untouched.
        assert!(p[..OFF].iter().all(|b| *b == 0xAB));
    }

    #[test]
    fn a_page_without_a_record_or_with_a_bad_id_is_ignored() {
        assert_eq!(Record::read(&page()), None);
        let mut p = page();
        Record { session: "a;rm -rf".into(), arena_run: 1, arena_pages: 1, outbound: 0, rings_run: 1, rings_pages: 1, gpu: false }
            .write(&mut p);
        assert_eq!(Record::read(&p), None);
        let mut p = page();
        Record { session: "ok".into(), arena_run: 0, arena_pages: 1, outbound: 0, rings_run: 1, rings_pages: 1, gpu: false }
            .write(&mut p);
        assert_eq!(Record::read(&p), None);
    }

    #[test]
    fn the_host_answer_reads_back() {
        let mut p = page();
        Record { session: "s".into(), arena_run: 1, arena_pages: 1, outbound: 0, rings_run: 2, rings_pages: 1, gpu: false }
            .write(&mut p);
        let m = message_bytes("no such app: é");
        p[OFF_MESSAGE..OFF_MESSAGE + m.len()].copy_from_slice(&m);
        p[OFF_STATE..OFF_STATE + 8].copy_from_slice(&State::Failed.word().to_le_bytes());
        assert_eq!(state(&p), State::Failed);
        assert_eq!(message(&p), "no such app: é");
        assert_eq!(unsafe { load_state(p.as_ptr()) }, State::Failed);
        assert!(message_bytes(&"x".repeat(2000)).len() == MESSAGE_MAX);
    }
}

// ------------------------------------------------ §105: the record's authenticator ---
//
// The boot record says which session to start; this block says who may ask.
// On ssh the guest proved itself with its key, and the forced command bounded
// what it could ask. Without ssh the shared-memory device is the boundary,
// and any process in the guest that can open it can write a record. So the
// record carries an HMAC-SHA-256 under a key that deploy wrote to two places
// only: the installation's directory on the host and the installation's
// directory in the guest user's profile. The host's service refuses a record
// for an installation that has a key unless the MAC verifies and the sequence
// number is above every one it accepted before, so a record read out of the
// device and written back later is refused as a replay. A torn read (the
// host's timer read the page while the guest was writing it) fails the MAC
// too, and is looked at again on the next pass, not answered.
//
// ```text
//   page 0, from AUTH_OFF
//   0    WCAUTH01
//   8    installation name, 32 bytes, NUL-padded ([A-Za-z0-9_-])
//   40   sequence number (u64): strictly increasing per installation
//   48   HMAC-SHA-256 (32 bytes) over auth_message()
//   80   end
// ```

/// Where the authenticator sits in page 0: past the host's message.
pub const AUTH_OFF: usize = 1792;
pub const AUTH_MAGIC: [u8; 8] = *b"WCAUTH01";
/// Longest installation name.
pub const INSTALL_MAX: usize = 32;
/// The key's length, in bytes. Deploy writes it as 64 hex digits.
pub const KEY_LEN: usize = 32;
const AUTH_LEN: usize = 80;

/// The authenticator, as written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Auth {
    pub installation: String,
    pub seq: u64,
    pub mac: [u8; 32],
}

/// Whether an installation name is one the host will look up.
pub fn valid_installation(name: &str) -> bool {
    !name.is_empty() && name.len() <= INSTALL_MAX && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// What the MAC covers: every field of the record the host acts on, the
/// installation and the sequence number, each length-delimited.
pub fn auth_message(rec: &Record, installation: &str, seq: u64) -> Vec<u8> {
    let mut m = Vec::with_capacity(160);
    m.extend_from_slice(b"wcboot-auth-v1\0");
    for s in [installation, rec.session.as_str()] {
        m.extend_from_slice(&(s.len() as u32).to_le_bytes());
        m.extend_from_slice(s.as_bytes());
    }
    for v in [rec.arena_run, u64::from(rec.arena_pages), rec.outbound, rec.rings_run, u64::from(rec.rings_pages), u64::from(rec.gpu), seq] {
        m.extend_from_slice(&v.to_le_bytes());
    }
    m
}

/// HMAC-SHA-256 (RFC 2104).
pub fn hmac_sha256(key: &[u8], msg: &[u8]) -> [u8; 32] {
    use sha2::{Digest, Sha256};
    let mut k = [0u8; 64];
    if key.len() > 64 {
        k[..32].copy_from_slice(&Sha256::digest(key));
    } else {
        k[..key.len()].copy_from_slice(key);
    }
    let mut inner = Sha256::new();
    inner.update(k.map(|b| b ^ 0x36));
    inner.update(msg);
    let ih = inner.finalize();
    let mut outer = Sha256::new();
    outer.update(k.map(|b| b ^ 0x5c));
    outer.update(ih);
    outer.finalize().into()
}

/// A key as deploy writes it: 64 hex digits, whitespace around them ignored.
pub fn parse_key(text: &str) -> Option<[u8; KEY_LEN]> {
    let t = text.trim();
    if t.len() != KEY_LEN * 2 {
        return None;
    }
    let mut k = [0u8; KEY_LEN];
    for (i, b) in k.iter_mut().enumerate() {
        *b = u8::from_str_radix(t.get(2 * i..2 * i + 2)?, 16).ok()?;
    }
    Some(k)
}

/// Equal in time independent of where the two differ.
pub fn mac_eq(a: &[u8; 32], b: &[u8; 32]) -> bool {
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

impl Auth {
    /// The guest's authenticator for `rec`.
    pub fn sign(rec: &Record, installation: &str, seq: u64, key: &[u8; KEY_LEN]) -> Auth {
        Auth { installation: installation.to_string(), seq, mac: hmac_sha256(key, &auth_message(rec, installation, seq)) }
    }

    /// Whether this authenticator is `rec`'s under `key`.
    pub fn verifies(&self, rec: &Record, key: &[u8; KEY_LEN]) -> bool {
        mac_eq(&self.mac, &hmac_sha256(key, &auth_message(rec, &self.installation, self.seq)))
    }

    pub fn write(&self, page: &mut [u8]) {
        let a = &mut page[AUTH_OFF..AUTH_OFF + AUTH_LEN];
        a.fill(0);
        a[0..8].copy_from_slice(&AUTH_MAGIC);
        let n = self.installation.len().min(INSTALL_MAX);
        a[8..8 + n].copy_from_slice(&self.installation.as_bytes()[..n]);
        a[40..48].copy_from_slice(&self.seq.to_le_bytes());
        a[48..80].copy_from_slice(&self.mac);
    }

    /// The authenticator in page 0, or `None` for none or a malformed one.
    pub fn read(page: &[u8]) -> Option<Auth> {
        let a = page.get(AUTH_OFF..AUTH_OFF + AUTH_LEN)?;
        if a[0..8] != AUTH_MAGIC {
            return None;
        }
        let name = &a[8..8 + INSTALL_MAX];
        let end = name.iter().position(|b| *b == 0).unwrap_or(INSTALL_MAX);
        let installation = std::str::from_utf8(&name[..end]).ok()?.to_string();
        if !valid_installation(&installation) {
            return None;
        }
        Some(Auth { installation, seq: u64::from_le_bytes(a[40..48].try_into().ok()?), mac: a[48..80].try_into().ok()? })
    }
}

/// Write a record and its authenticator into page 0 **of live shared memory**,
/// which a host may read at any moment: the record's magic is cleared first and
/// written last, after a release fence, so a reader sees the old record, no
/// record, or the whole new one. The host's fields (state and message) are
/// cleared with the record.
///
/// # Safety
/// `page0` must point at a mapped, writable page of at least 4096 bytes that
/// nothing else in this process writes concurrently.
pub unsafe fn publish(page0: *mut u8, rec: &Record, auth: Option<&Auth>) {
    use std::sync::atomic::{AtomicU64, Ordering, fence};
    let mut page = vec![0u8; 4096];
    rec.write(&mut page);
    if let Some(a) = auth {
        a.write(&mut page);
    }
    // SAFETY: the caller's contract. The magic word is 8-aligned (OFF is).
    unsafe {
        let magic = &*(page0.add(OFF) as *const AtomicU64);
        magic.store(0, Ordering::Relaxed);
        fence(Ordering::Release);
        std::ptr::copy_nonoverlapping(page.as_ptr().add(OFF + 8), page0.add(OFF + 8), OFF_MESSAGE + MESSAGE_MAX - OFF - 8);
        std::ptr::copy_nonoverlapping(page.as_ptr().add(AUTH_OFF), page0.add(AUTH_OFF), AUTH_LEN);
        fence(Ordering::Release);
        magic.store(u64::from_le_bytes(MAGIC), Ordering::Release);
    }
}

#[cfg(test)]
mod auth_tests {
    use super::*;

    fn rec() -> Record {
        Record { session: "wcns-77".into(), arena_run: 0xAB, arena_pages: 32768, outbound: 1 << 20, rings_run: 0xAB, rings_pages: 1536, gpu: true }
    }

    #[test]
    fn hmac_matches_rfc4231_case_2() {
        let mac = hmac_sha256(b"Jefe", b"what do ya want for nothing?");
        let want = "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843";
        let got: String = mac.iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(got, want);
    }

    #[test]
    fn a_signed_record_verifies_and_any_change_does_not() {
        let key = [7u8; KEY_LEN];
        let r = rec();
        let a = Auth::sign(&r, "wcns", 42, &key);
        let mut p = vec![0u8; 4096];
        r.write(&mut p);
        a.write(&mut p);
        let (r2, a2) = (Record::read(&p).unwrap(), Auth::read(&p).unwrap());
        assert!(a2.verifies(&r2, &key));
        assert!(!a2.verifies(&r2, &[8u8; KEY_LEN]), "another installation's key");
        let mut r3 = r2.clone();
        r3.gpu = false;
        assert!(!a2.verifies(&r3, &key), "a flipped field");
        let mut a3 = a2.clone();
        a3.seq += 1;
        assert!(!a3.verifies(&r2, &key), "a bumped sequence number");
        a3 = a2.clone();
        a3.installation = "wcother".into();
        assert!(!a3.verifies(&r2, &key), "another installation's name");
    }

    #[test]
    fn the_authenticator_does_not_overlap_the_hosts_fields() {
        assert!(AUTH_OFF >= OFF_MESSAGE + MESSAGE_MAX);
        assert!(AUTH_OFF + AUTH_LEN <= 4096);
    }

    #[test]
    fn keys_parse_from_hex_only() {
        let hex = "00112233445566778899aabbccddeeff00112233445566778899AABBCCDDEEFF\n";
        assert_eq!(parse_key(hex).unwrap()[15], 0xff);
        assert!(parse_key("abc").is_none());
        assert!(parse_key(&"zz".repeat(32)).is_none());
    }

    #[test]
    fn publish_writes_a_whole_record_with_the_magic_last() {
        let mut p = vec![0x55u8; 4096];
        let r = rec();
        let a = Auth::sign(&r, "wcns", 1, &[1; KEY_LEN]);
        // SAFETY: a 4096-byte buffer this test owns.
        unsafe { publish(p.as_mut_ptr(), &r, Some(&a)) };
        assert_eq!(Record::read(&p), Some(r));
        assert_eq!(Auth::read(&p), Some(a));
        assert_eq!(state(&p), State::Waiting);
        assert!(p[..OFF].iter().all(|b| *b == 0x55), "the header before the record is untouched");
    }
}
