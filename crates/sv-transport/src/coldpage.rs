//! The cold path in the shared memory itself: a mailbox in the first page of
//! the extent (waycompose-design.md §103.6).
//!
//! Every other cold path in this crate is a byte stream ([`crate::stream`]):
//! a virtio-serial port, a Unix socket, and on the macOS guest until §103.6's
//! second half, a TCP listener reached through an ssh forward. A macOS guest
//! has no virtio-serial port and no shared-memory device, and the rule there is
//! that nothing of the GPU path crosses the boundary through a socket. What
//! both sides *do* share, before any session exists, is the extent: the guest
//! wrote it, and the host finds it by its stamps (`sv_hostd::locate`) or, over
//! a shared file, at offset 0. So the cold path's two messages go in the
//! extent's own first page:
//!
//! ```text
//!   guest (owner)                                  host (attacher)
//!   -------------                                  ---------------
//!   stamp every page (arena_record)
//!   offer(Hello), state = OFFERED
//!   beat() every tick ................. sweeps; reads the page; sees the
//!                                       heartbeat move; claim(): OFFERED -> CLAIMED
//!                                       locates, stitches, checks
//!   poll_reply() ...................... reply(Accept): REPLIED
//!   Control::create (zeroes the page)   waits for the control magic, serves
//! ```
//!
//! The mailbox's bytes are a [`ColdMessage`] encoded exactly as the stream
//! would carry it, minus the stream's frame: one serialiser for every cold
//! path, so a Hello cannot mean one thing over a socket and another in a page.
//!
//! # Where it sits, and why it cannot collide
//!
//! [`BASE`] is past every slot `arena_record`'s page uses (0..152), so page 0
//! keeps its magic, run id and index, and the host's locate and its
//! stitched-index check see an ordinary stamped page. The arena's pages are
//! not touched. `Control::create` zeroes the whole control region after the
//! Accept, which ends the mailbox with nothing to clean up.
//!
//! # A stale offer is not a live one
//!
//! Freed guest pages are not zeroed, so an offer from a process that died
//! before its host came can still be in guest RAM, magic and all. The guest
//! therefore bumps a heartbeat while it waits, and the host takes an offer
//! only after it has seen the heartbeat change ([`is_live`]). A dead offer's
//! heartbeat is frozen. The same rule makes a shared file (role (b), a RAM-disk
//! file through virtio-fs DAX) safe to reuse across runs.
//!
//! # Waking
//!
//! There is no doorbell in this exchange: the guest polls [`poll_reply`] every
//! tick until a deadline, and the host polls the heartbeat. It happens once per
//! session and its cost is bounded by the tick (1 ms by default) against a
//! locate of hundreds of milliseconds. The rings' doorbell, if the guest has a
//! port, is attached after the Accept as everywhere else
//! ([`crate::vdoorbell`]).
//!
//! # Independent of the memory
//!
//! Everything here takes a [`SharedRegion`] whose offset 0 is the extent's
//! first byte: guest RAM located by sweeping QEMU's backend, a shared file
//! mapped by both sides, or the ivshmem stand-in file. Nothing here knows
//! which.

use crate::cold::{ColdMessage, Hello};
use crate::region::SharedRegion;
use crate::{Error, Result};
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

/// The mailbox's offset in page 0, past `arena_record`'s slots.
pub const BASE: u64 = 512;
pub const OFF_MAGIC: u64 = BASE;
pub const OFF_STATE: u64 = BASE + 8;
pub const OFF_OFFER_LEN: u64 = BASE + 12;
pub const OFF_BEAT: u64 = BASE + 16;
pub const OFF_CLAIM: u64 = BASE + 24;
pub const OFF_REPLY_LEN: u64 = BASE + 32;
pub const OFF_LABEL: u64 = BASE + 40;
pub const LABEL_LEN: usize = 16;
/// The guest's message (a `Hello`), encoded.
pub const OFF_OFFER: u64 = 1024;
pub const MAX_OFFER: usize = 1536;
/// The host's message (`Accept` or `Refuse`), encoded.
pub const OFF_REPLY: u64 = OFF_OFFER + MAX_OFFER as u64;
pub const MAX_REPLY: usize = 1536;
/// One page: the mailbox never reaches past it.
pub const PAGE_LEN: usize = 4096;

pub const MAGIC: [u8; 8] = *b"SVCOLDP1";

pub const STATE_EMPTY: u32 = 0;
pub const STATE_OFFERED: u32 = 1;
pub const STATE_CLAIMED: u32 = 2;
pub const STATE_REPLIED: u32 = 3;

/// The label an offer carries when the caller does not choose one.
pub const DEFAULT_LABEL: &str = "sv-gpu";

/// What the guest's tick is when the caller does not choose one.
pub const DEFAULT_TICK: Duration = Duration::from_millis(1);

fn label_bytes(label: &str) -> [u8; LABEL_LEN] {
    let mut b = [0u8; LABEL_LEN];
    let n = label.len().min(LABEL_LEN);
    b[..n].copy_from_slice(&label.as_bytes()[..n]);
    b
}

fn label_str(b: &[u8]) -> String {
    let end = b.iter().position(|&c| c == 0).unwrap_or(b.len());
    String::from_utf8_lossy(&b[..end]).into_owned()
}

/// Guest: write `hello` into the mailbox and mark it offered.
///
/// The body first and the state last, with a release store, so a host that
/// reads `OFFERED` reads a whole Hello.
pub fn offer(region: &SharedRegion, label: &str, hello: &Hello) -> Result<()> {
    let mut body = Vec::new();
    ColdMessage::Hello(Box::new(hello.clone())).encode(&mut body)?;
    if body.len() > MAX_OFFER {
        return Err(Error::BadColdMessage("the Hello does not fit the mailbox"));
    }
    region.check(0, PAGE_LEN as u64)?;
    region.atomic_u32(OFF_STATE)?.store(STATE_EMPTY, Ordering::Release);
    region.atomic_u64(OFF_CLAIM)?.store(0, Ordering::Relaxed);
    region.atomic_u32(OFF_REPLY_LEN)?.store(0, Ordering::Relaxed);
    region.atomic_u64(OFF_BEAT)?.store(1, Ordering::Relaxed);
    region.write_bytes(OFF_LABEL, &label_bytes(label))?;
    region.write_bytes(OFF_OFFER, &body)?;
    region.atomic_u32(OFF_OFFER_LEN)?.store(body.len() as u32, Ordering::Relaxed);
    region.write_bytes(OFF_MAGIC, &MAGIC)?;
    region.atomic_u32(OFF_STATE)?.store(STATE_OFFERED, Ordering::Release);
    Ok(())
}

/// Guest: one heartbeat, so the host can tell this offer from a dead one.
pub fn beat(region: &SharedRegion) -> Result<()> {
    region.atomic_u64(OFF_BEAT)?.fetch_add(1, Ordering::Release);
    Ok(())
}

/// The mailbox's state word.
pub fn state(region: &SharedRegion) -> Result<u32> {
    Ok(region.atomic_u32(OFF_STATE)?.load(Ordering::Acquire))
}

/// The heartbeat, read by the host through its own mapping.
pub fn heartbeat(region: &SharedRegion) -> Result<u64> {
    Ok(region.atomic_u64(OFF_BEAT)?.load(Ordering::Acquire))
}

/// Guest: the host's reply, if it has written one.
pub fn poll_reply(region: &SharedRegion) -> Result<Option<ColdMessage>> {
    if state(region)? != STATE_REPLIED {
        return Ok(None);
    }
    let len = region.atomic_u32(OFF_REPLY_LEN)?.load(Ordering::Acquire) as usize;
    if len == 0 || len > MAX_REPLY {
        return Err(Error::BadColdMessage("the mailbox's reply length is out of range"));
    }
    let mut body = vec![0u8; len];
    region.read_bytes(OFF_REPLY, &mut body)?;
    ColdMessage::decode(&body).map(Some)
}

/// How a guest's wait for the host ended without a reply.
#[derive(Debug)]
pub enum WaitEnd {
    /// Nobody replied before the deadline; `claimed` says whether a host had
    /// at least taken the offer.
    Deadline { claimed: bool },
    Transport(Error),
}

impl std::fmt::Display for WaitEnd {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WaitEnd::Deadline { claimed: false } => write!(f, "no host took the offer before the deadline"),
            WaitEnd::Deadline { claimed: true } => {
                write!(f, "a host claimed the offer and did not reply before the deadline")
            }
            WaitEnd::Transport(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for WaitEnd {}

impl From<Error> for WaitEnd {
    fn from(e: Error) -> Self {
        WaitEnd::Transport(e)
    }
}

/// Guest: beat and poll every `tick` until the host replies or `wait` runs
/// out. `on_claim` is called once, when a host claims the offer.
pub fn await_reply(
    region: &SharedRegion,
    wait: Duration,
    tick: Duration,
    mut on_claim: impl FnMut(u64),
) -> std::result::Result<ColdMessage, WaitEnd> {
    let deadline = Instant::now() + wait;
    let mut claimed = false;
    loop {
        if let Some(m) = poll_reply(region)? {
            return Ok(m);
        }
        if !claimed && state(region)? == STATE_CLAIMED {
            claimed = true;
            on_claim(region.atomic_u64(OFF_CLAIM)?.load(Ordering::Acquire));
        }
        if Instant::now() >= deadline {
            return Err(WaitEnd::Deadline { claimed });
        }
        beat(region)?;
        std::thread::sleep(tick);
    }
}

/// An offer as the host reads it out of a copy of the page.
#[derive(Debug, Clone)]
pub struct Offer {
    pub label: String,
    pub heartbeat: u64,
    pub hello: Hello,
}

/// Host: parse a page snapshot (read by `pread` during a sweep, or out of a
/// mapping). `None` for anything that is not a whole, offered Hello: a page
/// with no mailbox, one already claimed or answered, or one caught half
/// written.
pub fn parse_offer(page: &[u8]) -> Option<Offer> {
    if page.len() < PAGE_LEN {
        return None;
    }
    let at = |off: u64, n: usize| &page[off as usize..off as usize + n];
    if at(OFF_MAGIC, 8) != MAGIC {
        return None;
    }
    let u32_at = |off: u64| u32::from_le_bytes(at(off, 4).try_into().unwrap());
    let u64_at = |off: u64| u64::from_le_bytes(at(off, 8).try_into().unwrap());
    if u32_at(OFF_STATE) != STATE_OFFERED {
        return None;
    }
    let len = u32_at(OFF_OFFER_LEN) as usize;
    if len == 0 || len > MAX_OFFER {
        return None;
    }
    match ColdMessage::decode(at(OFF_OFFER, len)) {
        Ok(ColdMessage::Hello(h)) => Some(Offer {
            label: label_str(at(OFF_LABEL, LABEL_LEN)),
            heartbeat: u64_at(OFF_BEAT),
            hello: *h,
        }),
        _ => None,
    }
}

/// Host: read the offer straight out of a mapping of the extent's first page.
pub fn read_offer(region: &SharedRegion) -> Result<Option<Offer>> {
    let mut page = vec![0u8; PAGE_LEN];
    region.read_bytes(0, &mut page)?;
    Ok(parse_offer(&page))
}

/// Host: does the label match what this host was asked to take? An empty
/// `want` takes any.
pub fn label_matches(offer: &Offer, want: &str) -> bool {
    want.is_empty() || offer.label == want
}

/// Host: has the heartbeat moved since `seen`? Waits at most `within`,
/// re-reading every millisecond; a live guest beats every tick.
pub fn is_live(region: &SharedRegion, seen: u64, within: Duration) -> Result<bool> {
    let deadline = Instant::now() + within;
    loop {
        if heartbeat(region)? != seen {
            return Ok(true);
        }
        if Instant::now() >= deadline {
            return Ok(false);
        }
        std::thread::sleep(Duration::from_millis(1));
    }
}

/// Host: take the offer, so a second host sweeping the same guest leaves it
/// alone. `false` if somebody else took it first (or it is no longer offered).
pub fn claim(region: &SharedRegion, nonce: u64) -> Result<bool> {
    region.atomic_u64(OFF_CLAIM)?.store(nonce, Ordering::Relaxed);
    Ok(region
        .atomic_u32(OFF_STATE)?
        .compare_exchange(STATE_OFFERED, STATE_CLAIMED, Ordering::AcqRel, Ordering::Acquire)
        .is_ok())
}

/// Host: write the reply and mark it, body first.
pub fn reply(region: &SharedRegion, msg: &ColdMessage) -> Result<()> {
    let mut body = Vec::new();
    msg.encode(&mut body)?;
    if body.len() > MAX_REPLY {
        // A Refuse whose reason is too long is cut, not lost.
        if let ColdMessage::Refuse { code, reason } = msg {
            let mut r = reason.clone();
            r.truncate(MAX_REPLY / 2);
            return reply(region, &ColdMessage::Refuse { code: *code, reason: r });
        }
        return Err(Error::BadColdMessage("the reply does not fit the mailbox"));
    }
    region.write_bytes(OFF_REPLY, &body)?;
    region.atomic_u32(OFF_REPLY_LEN)?.store(body.len() as u32, Ordering::Release);
    region.atomic_u32(OFF_STATE)?.store(STATE_REPLIED, Ordering::Release);
    Ok(())
}

/// Host: [`claim`] for a mailbox several hosts sweep at once (the GPU pool's,
/// `crate::poolcold`): the compare-and-swap first and the nonce after it, so
/// the nonce in the page is always the winner's and [`reply_claimed`] can
/// check it. ([`claim`] stores the nonce first, and a loser's store can land
/// after the winner's; its one reader is the guest's log.)
pub fn claim_signed(region: &SharedRegion, nonce: u64) -> Result<bool> {
    let won = region
        .atomic_u32(OFF_STATE)?
        .compare_exchange(STATE_OFFERED, STATE_CLAIMED, Ordering::AcqRel, Ordering::Acquire)
        .is_ok();
    if won {
        region.atomic_u64(OFF_CLAIM)?.store(nonce, Ordering::Release);
    }
    Ok(won)
}

/// Host: [`reply`], but only into a mailbox this host still holds: still
/// `CLAIMED`, under `nonce`. `false` if the owner took the offer back in the
/// meantime (it does when a claim is never answered), in which case nothing
/// the owner reads has changed.
pub fn reply_claimed(region: &SharedRegion, nonce: u64, msg: &ColdMessage) -> Result<bool> {
    if region.atomic_u64(OFF_CLAIM)?.load(Ordering::Acquire) != nonce {
        return Ok(false);
    }
    let mut body = Vec::new();
    msg.encode(&mut body)?;
    if body.len() > MAX_REPLY {
        if let ColdMessage::Refuse { code, reason } = msg {
            let mut r = reason.clone();
            r.truncate(MAX_REPLY / 2);
            return reply_claimed(region, nonce, &ColdMessage::Refuse { code: *code, reason: r });
        }
        return Err(Error::BadColdMessage("the reply does not fit the mailbox"));
    }
    region.write_bytes(OFF_REPLY, &body)?;
    region.atomic_u32(OFF_REPLY_LEN)?.store(body.len() as u32, Ordering::Release);
    Ok(region
        .atomic_u32(OFF_STATE)?
        .compare_exchange(STATE_CLAIMED, STATE_REPLIED, Ordering::AcqRel, Ordering::Acquire)
        .is_ok())
}

/// Move the state word from `from` to `to` if it is still `from`.
pub fn transition(region: &SharedRegion, from: u32, to: u32) -> Result<bool> {
    Ok(region.atomic_u32(OFF_STATE)?.compare_exchange(from, to, Ordering::AcqRel, Ordering::Acquire).is_ok())
}

/// Host: wait for the owner to have made the control region, after the
/// Accept: its magic at offset 0 is the last thing `Control::create` writes
/// before the header is usable. An ICD handed the region before then reads
/// the stamp (`SVARENA1`) and gives up, which was measured on the macOS guest
/// once the cold path stopped costing an ssh round trip.
pub fn await_control(region: &SharedRegion, wait: Duration) -> Result<bool> {
    let deadline = Instant::now() + wait;
    loop {
        if region.atomic_u64(crate::control::OFF_MAGIC)?.load(Ordering::Acquire) == crate::control::MAGIC {
            return Ok(true);
        }
        if Instant::now() >= deadline {
            return Ok(false);
        }
        std::thread::sleep(Duration::from_millis(1));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cold::{ExtentDesc, Granularity, Locator};
    use crate::control::{ENDIAN_MARKER, Geometry, ProtocolIds};

    fn hello(run_id: u64) -> Hello {
        Hello {
            ids: ProtocolIds::new(1, 0x1234),
            generation: 1,
            extent: ExtentDesc {
                run_id,
                granularity: Granularity::Small4K,
                control_pages: 4096,
                arena_pages: 16384,
                locator: Locator::Scan,
            },
            geometry: Geometry::default(),
            endianness: ENDIAN_MARKER,
            pointer_width: 64,
        }
    }

    fn snapshot(r: &SharedRegion) -> Vec<u8> {
        let mut p = vec![0u8; PAGE_LEN];
        r.read_bytes(0, &mut p).unwrap();
        p
    }

    #[test]
    fn the_mailbox_is_past_the_page_record_and_inside_the_page() {
        // arena_record's last slot is OFF_HOST2 (128) + 24.
        const { assert!(BASE >= 152) };
        const { assert!(OFF_LABEL + LABEL_LEN as u64 <= OFF_OFFER) };
        const { assert!(OFF_REPLY + MAX_REPLY as u64 <= PAGE_LEN as u64) };
    }

    #[test]
    fn a_hello_fits_and_round_trips() {
        let r = SharedRegion::heap(PAGE_LEN).unwrap();
        offer(&r, "gpu-a", &hello(0xabc)).unwrap();
        let o = parse_offer(&snapshot(&r)).expect("offered");
        assert_eq!(o.label, "gpu-a");
        assert_eq!(o.hello.extent.run_id, 0xabc);
        assert_eq!(o.hello.extent.arena_pages, 16384);
        assert!(label_matches(&o, "gpu-a") && label_matches(&o, "") && !label_matches(&o, "gpu-b"));
    }

    #[test]
    fn claimed_or_answered_offers_are_not_offered() {
        let r = SharedRegion::heap(PAGE_LEN).unwrap();
        offer(&r, DEFAULT_LABEL, &hello(7)).unwrap();
        assert!(claim(&r, 42).unwrap());
        assert!(!claim(&r, 43).unwrap(), "a second host must not take it");
        assert!(parse_offer(&snapshot(&r)).is_none());
        reply(&r, &ColdMessage::Accept { generation: 1 }).unwrap();
        assert!(parse_offer(&snapshot(&r)).is_none());
        assert!(matches!(poll_reply(&r).unwrap(), Some(ColdMessage::Accept { generation: 1 })));
    }

    #[test]
    fn a_frozen_heartbeat_is_dead_and_a_beating_one_is_live() {
        let r = SharedRegion::heap(PAGE_LEN).unwrap();
        offer(&r, DEFAULT_LABEL, &hello(9)).unwrap();
        let seen = heartbeat(&r).unwrap();
        assert!(!is_live(&r, seen, Duration::from_millis(5)).unwrap());
        beat(&r).unwrap();
        assert!(is_live(&r, seen, Duration::from_millis(5)).unwrap());
    }

    #[test]
    fn a_long_refusal_is_cut_to_fit() {
        let r = SharedRegion::heap(PAGE_LEN).unwrap();
        offer(&r, DEFAULT_LABEL, &hello(9)).unwrap();
        reply(&r, &ColdMessage::Refuse { code: 3, reason: "x".repeat(10_000) }).unwrap();
        match poll_reply(&r).unwrap() {
            Some(ColdMessage::Refuse { code: 3, reason }) => assert!(reason.len() <= MAX_REPLY),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn the_guest_waits_out_its_deadline_and_says_whether_it_was_claimed() {
        let r = SharedRegion::heap(PAGE_LEN).unwrap();
        offer(&r, DEFAULT_LABEL, &hello(9)).unwrap();
        let end = await_reply(&r, Duration::from_millis(10), Duration::from_millis(1), |_| {});
        assert!(matches!(end, Err(WaitEnd::Deadline { claimed: false })));
        claim(&r, 5).unwrap();
        let mut seen = 0;
        let end = await_reply(&r, Duration::from_millis(10), Duration::from_millis(1), |n| seen = n);
        assert!(matches!(end, Err(WaitEnd::Deadline { claimed: true })));
        assert_eq!(seen, 5);
    }

    /// The exchange over a shared file, two mappings and two threads: role
    /// (b)'s arrangement, where the owner and the attacher map one file.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn the_exchange_runs_over_two_mappings_of_one_file() {
        use std::os::fd::AsFd;
        let path = std::env::temp_dir().join(format!("sv-coldpage-{}", std::process::id()));
        let f = std::fs::OpenOptions::new().read(true).write(true).create(true).truncate(true).open(&path).unwrap();
        f.set_len(2 * PAGE_LEN as u64).unwrap();
        let guest = SharedRegion::map_fd(f.as_fd(), 0, 2 * PAGE_LEN).unwrap();
        let host = SharedRegion::map_fd(f.as_fd(), 0, PAGE_LEN).unwrap();
        let _ = std::fs::remove_file(&path);

        offer(&guest, "file", &hello(0x55)).unwrap();
        let t = std::thread::spawn(move || {
            await_reply(&guest, Duration::from_secs(10), Duration::from_millis(1), |_| {})
                .map(|m| matches!(m, ColdMessage::Accept { generation: 1 }))
        });
        let o = read_offer(&host).unwrap().expect("the host sees the offer");
        assert_eq!(o.hello.extent.run_id, 0x55);
        assert!(is_live(&host, o.heartbeat, Duration::from_secs(2)).unwrap());
        assert!(claim(&host, 1).unwrap());
        reply(&host, &ColdMessage::Accept { generation: 1 }).unwrap();
        assert!(t.join().unwrap().unwrap());
    }
}
