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
