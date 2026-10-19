//! The GPU pool's cold path in the pool's own header granule: no socket, no
//! TCP, no ssh forward between the guest's GPU process and the VM host's
//! `sv-hostd ivshmem-serve`.
//!
//! Until this module, a pooled session started with a TCP connection to the
//! guest's GPU process, reached from the VM host through an ssh reverse
//! forward (`$D/cold.sock`), carrying `Hello`, `Accept` and `Ping`. Both sides
//! already share the pool, and its first 64 KiB ([`poolhdr::HEADER_LEN`]) are
//! never leased. The pool header uses bytes 0..32 of page 0; pages 1..=15 are
//! [`COUNT`] mailboxes, each one page laid out exactly as
//! [`crate::coldpage`]'s (§103.6.1: magic, state, offer, heartbeat, claim,
//! reply), so a `Hello` cannot mean one thing over one cold path and another
//! here.
//!
//! ```text
//!  pool offset   what
//!  0x0000        pool header (poolhdr): magic, version, nonce, beat
//!  0x1000        mailbox 0  (a coldpage page)
//!  ...
//!  0xF000        mailbox 14
//!  0x10000       first lease
//! ```
//!
//! # The exchange
//!
//! ```text
//!   guest (pool owner, one process)              host (one sv-hostd per session)
//!   -------------------------------              -------------------------------
//!   reset every mailbox, publish the header
//!   lease + stamp a control region,
//!   offer(Hello) in an EMPTY mailbox:  OFFERED
//!   ring COLD_TOKEN ...................... an ICD's AttachRequest: scan the mailboxes;
//!                                          an OFFERED one whose Hello names this
//!                                          header's nonce and whose owner is live:
//!                                          CAS OFFERED -> CLAIMED, sign it
//!                                          check_slot (the stamps, the nonce)
//!                                          Accept (or Refuse) -> REPLIED
//!   <..................................... ring COLD_TOKEN
//!   REPLIED/Accept: open the device, make the
//!   control region (Control::create), then
//!   REPLIED -> PUBLISHED (or FAILED + reason)
//!   ring COLD_TOKEN ...................... PUBLISHED and the control magic: CAS -> EMPTY,
//!   offer the next lease in another        hand the ICD its Attach
//!   EMPTY mailbox (at once, after the Accept)
//! ```
//!
//! States: [`coldpage::STATE_EMPTY`], `OFFERED`, `CLAIMED`, `REPLIED`, and two
//! of this module's, [`STATE_PUBLISHED`] and [`STATE_FAILED`]. The guest moves
//! EMPTY -> OFFERED, REPLIED -> PUBLISHED/FAILED, and, for a Refuse, REPLIED ->
//! EMPTY. The host moves OFFERED -> CLAIMED -> REPLIED and PUBLISHED/FAILED ->
//! EMPTY. Every move but the guest's offer into its own EMPTY page is a
//! compare-and-swap, because several `sv-hostd` (one per session) sweep the
//! same mailboxes.
//!
//! **One offer at a time.** Each offer holds a leased control region (the
//! pool's `reserve` keeps room for exactly one session's start), so the guest
//! keeps a single OFFERED mailbox and makes the next one as soon as the
//! previous one is answered. Two ICDs asking at once: one claims the offer,
//! the other waits for the next, which the guest rings for. `max_sessions` is
//! honoured by not offering beyond it, so a full house is "no offer appears",
//! and the host then refuses with the words `sv-icd` retries on
//! ("GPU session slots are taken", §95.10).
//!
//! # Stale offers
//!
//! The pool is ivshmem: a dead owner's bytes survive it, offers included. An
//! offer is taken only if its `Hello`'s `pool_nonce` is the header's (a new
//! owner republishes the header with a new nonce, so an old owner's offers no
//! longer match) **and** its owner is live: the pool header's beat (once a
//! second) moved recently ([`Liveness`]), or the offer's own heartbeat moves
//! within [`HostTiming::live_within`] (the guest beats it on every scan). A
//! dead owner whose header is still valid has offers that match and freeze;
//! they are left alone, and the host says the guest is not running.
//!
//! # Wakes, and the rule about reopened ports
//!
//! Nobody spins. The doorbell port (`org.sirati.svbell.0`, shared through the
//! host's bell hub, §99) carries [`crate::park::COLD_TOKEN`] both ways: the
//! host rings after its reply, the guest after an offer and after a session
//! is published or failed. A receiver turns it into a wake of a [`ColdEvent`],
//! and every wake means "rescan every mailbox"; nothing names the mailbox.
//!
//! **A virtio-serial port drops bytes that arrive while nobody on that side
//! has it open**, so a wake sent across a reopen (a hub restart, a process
//! restart) is lost without a trace. So every (re)open of a bell port is
//! itself a wake ([`ColdEvent::reopened`]), and every waiter also rescans when
//! its slice runs out. With a bell the guest sleeps [`GuestTiming::bell_slice`]
//! (50 ms); without one it polls every [`GuestTiming::claimed_poll`] (1 ms)
//! only while a mailbox it offered is CLAIMED, and every
//! [`GuestTiming::idle_poll`] (20 ms) otherwise.

use crate::cold::{ColdMessage, Hello, Locator};
use crate::coldpage::{self, PAGE_LEN};
use crate::park::COLD_TOKEN;
use crate::poolhdr::{self, PoolHeader, Snapshot};
use crate::region::SharedRegion;
use crate::vdoorbell::BellTarget;
use crate::Result;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

/// Page index (in the pool) of mailbox 0. Page 0 is the pool header.
pub const FIRST_PAGE: u64 = 1;
/// Mailboxes: the rest of the never-leased 64 KiB granule.
pub const COUNT: usize = 15;
/// Guest: the accepted session's control region is published.
pub const STATE_PUBLISHED: u32 = 4;
/// Guest: the accepted session could not be opened; the reason is in the
/// offer's bytes.
pub const STATE_FAILED: u32 = 5;
/// The label on every pool offer.
pub const LABEL: &str = "sv-gpu-pool";

const _: () = assert!((FIRST_PAGE + COUNT as u64) * PAGE_LEN as u64 <= poolhdr::HEADER_LEN);

/// Pool offset of mailbox `i`.
pub const fn offset(i: usize) -> u64 {
    (FIRST_PAGE + i as u64) * PAGE_LEN as u64
}

/// The fifteen mailboxes of one mapping of the pool's header granule (the
/// guest's whole BAR, or the host's 64 KiB map of the region file).
pub struct Mailboxes {
    // Declared before `owner`, and borrowed from it: dropped first.
    pages: Vec<SharedRegion>,
    owner: Arc<SharedRegion>,
}

impl Mailboxes {
    /// `region`'s offset 0 is the pool's first byte.
    pub fn new(region: Arc<SharedRegion>) -> Result<Mailboxes> {
        region.check(0, poolhdr::HEADER_LEN)?;
        let pages = (0..COUNT)
            // SAFETY: inside the checked header granule, and `owner` keeps the
            // mapping alive for as long as these views exist.
            .map(|i| unsafe { SharedRegion::from_raw(region.base_ptr().add(offset(i) as usize), PAGE_LEN) })
            .collect();
        Ok(Mailboxes { pages, owner: region })
    }

    pub fn page(&self, i: usize) -> &SharedRegion {
        &self.pages[i]
    }

    pub fn header(&self) -> Snapshot {
        PoolHeader::new(&self.owner).map(|h| h.snapshot()).unwrap_or_default()
    }

    /// The whole mapping, offset 0 the pool's first byte.
    pub fn region(&self) -> &Arc<SharedRegion> {
        &self.owner
    }

    /// Guest, at start, before publishing the header: forget every offer a
    /// previous owner left.
    pub fn reset(&self) {
        for p in &self.pages {
            let _ = p.fill(0, PAGE_LEN as u64, 0);
        }
        std::sync::atomic::fence(Ordering::Release);
    }

    /// Every mailbox's state word, for a log line.
    pub fn states(&self) -> Vec<u32> {
        self.pages.iter().map(|p| coldpage::state(p).unwrap_or(u32::MAX)).collect()
    }
}

/// A wake for whoever waits on the mailboxes: rung by an arriving
/// [`COLD_TOKEN`], by a reopened bell port, and locally by anything that
/// changes what the waiter should do (a session ending frees a place).
///
/// A generation counter and a condition variable: portable, and a waiter that
/// was busy when it was rung sees the generation moved and does not sleep.
#[derive(Default)]
pub struct ColdEvent {
    generation: Mutex<u64>,
    cv: Condvar,
    bell: AtomicBool,
    opens: AtomicU64,
}

impl ColdEvent {
    pub fn new() -> Arc<ColdEvent> {
        Arc::new(ColdEvent::default())
    }

    pub fn ring(&self) {
        let mut g = self.generation.lock().unwrap_or_else(|e| e.into_inner());
        *g = g.wrapping_add(1);
        self.cv.notify_all();
    }

    pub fn generation(&self) -> u64 {
        *self.generation.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Sleep until rung after `seen` or until `timeout`; `seen` becomes the
    /// current generation. `true` if it was rung.
    pub fn wait(&self, seen: &mut u64, timeout: Duration) -> bool {
        let g = self.generation.lock().unwrap_or_else(|e| e.into_inner());
        let (g, _) = self
            .cv
            .wait_timeout_while(g, timeout, |g| *g == *seen)
            .unwrap_or_else(|e| e.into_inner());
        let woken = *g != *seen;
        *seen = *g;
        woken
    }

    /// Whether a bell port is open, which decides how long a waiter sleeps.
    pub fn has_bell(&self) -> bool {
        self.bell.load(Ordering::Acquire)
    }

    /// A bell port was (re)opened. Whatever was rung while it was closed is
    /// gone, so this is a wake: every waiter rescans every mailbox.
    pub fn reopened(&self) {
        self.opens.fetch_add(1, Ordering::Relaxed);
        self.bell.store(true, Ordering::Release);
        self.ring();
    }

    /// The bell port closed: waiters poll until it is back.
    pub fn closed(&self) {
        self.bell.store(false, Ordering::Release);
        self.ring();
    }

    /// How many times a bell port was opened, for the log.
    pub fn opens(&self) -> u64 {
        self.opens.load(Ordering::Relaxed)
    }
}

impl BellTarget for ColdEvent {
    fn ring_token(&self, token: u8, woken: &AtomicU64) -> bool {
        if token != COLD_TOKEN {
            return false;
        }
        woken.fetch_add(1, Ordering::Relaxed);
        self.ring();
        true
    }
}

/// The guest's doorman target: [`COLD_TOKEN`] wakes the mailbox waiter, every
/// other byte goes to the sessions' lanes as before.
pub struct WithCold<T: ?Sized> {
    pub cold: Arc<ColdEvent>,
    pub lanes: Arc<T>,
}

impl<T: ?Sized> WithCold<T> {
    /// The port was (re)opened: the mailbox waiter rescans.
    pub fn reopened(&self) {
        self.cold.reopened();
    }
}

impl<T: BellTarget + ?Sized> BellTarget for WithCold<T> {
    fn ring_token(&self, token: u8, woken: &AtomicU64) -> bool {
        if token == COLD_TOKEN {
            return self.cold.ring_token(token, woken);
        }
        self.lanes.ring_token(token, woken)
    }
}

/// Guest: open the doorbell on `port` for every session's lanes **and** the
/// mailboxes: the writer becomes the process's cross wake (as
/// `vdoorbell::attach_set` does) and the reader rings `lanes` or `cold`. The
/// open is a wake of `cold` (the reopen rule).
pub fn attach_guest_bell<T: BellTarget + ?Sized + 'static>(
    port: crate::vdoorbell::PortHandle,
    lanes: Arc<T>,
    cold: Arc<ColdEvent>,
) -> std::io::Result<crate::vdoorbell::Doorman> {
    let (reader, writer) = port.split()?;
    crate::park::install_cross_wake(Some(
        Arc::new(crate::vdoorbell::PortWake::new(writer)) as Arc<dyn crate::park::CrossWake>
    ));
    Ok(guest_doorman(reader, lanes, cold))
}

/// [`attach_guest_bell`]'s reader half alone: the doorman, and the wake its
/// open is.
pub fn guest_doorman<R: std::io::Read + Send + 'static, T: BellTarget + ?Sized + 'static>(
    reader: R,
    lanes: Arc<T>,
    cold: Arc<ColdEvent>,
) -> crate::vdoorbell::Doorman {
    let target = Arc::new(WithCold { cold, lanes });
    let d = crate::vdoorbell::Doorman::spawn_target(reader, Arc::clone(&target));
    target.reopened();
    d
}

/// Guest: ring [`COLD_TOKEN`] on the process's cross wake, if it has one.
pub fn poke_cross() {
    if let Some(w) = crate::park::cross_wake() {
        w.poke(COLD_TOKEN);
    }
}

/// A poke: how one side rings the other's [`ColdEvent`].
pub type Poke = Arc<dyn Fn() + Send + Sync>;

// ------------------------------------------------------------------ guest ---

/// How long the guest sleeps between scans.
#[derive(Debug, Clone, Copy)]
pub struct GuestTiming {
    /// With a bell: a wake is expected, so this only bounds a lost one.
    pub bell_slice: Duration,
    /// No bell, nothing claimed.
    pub idle_poll: Duration,
    /// No bell, an offer of ours claimed: the reply is due within
    /// milliseconds.
    pub claimed_poll: Duration,
    /// A claim never answered is taken back after this.
    pub claim_timeout: Duration,
    /// A mailbox left PUBLISHED or FAILED (its host died before reading it) is
    /// emptied after this.
    pub stale_after: Duration,
}

impl Default for GuestTiming {
    fn default() -> Self {
        GuestTiming {
            bell_slice: Duration::from_millis(50),
            idle_poll: Duration::from_millis(20),
            claimed_poll: Duration::from_millis(1),
            claim_timeout: Duration::from_secs(30),
            stale_after: Duration::from_secs(60),
        }
    }
}

/// What makes the guest's offers and runs its accepted sessions.
pub trait OfferSource {
    type Ticket;
    /// A `Hello` for a control region already leased and stamped, or `None`
    /// when no session may start now (the session limit, a full pool). Asked
    /// again at the next wake or slice.
    fn offer(&mut self) -> Option<(Hello, Self::Ticket)>;
    /// The host accepted. The session must end with `outcome.published()` or
    /// `outcome.failed(..)` (dropping it says failed).
    fn accepted(&mut self, ticket: Self::Ticket, outcome: Outcome);
    /// The offer ended unaccepted: refused, never answered, or withdrawn.
    /// Give its lease back.
    fn refused(&mut self, ticket: Self::Ticket, why: String);
}

/// An accepted session's way of telling its host it is ready, or why not.
pub struct Outcome {
    mailboxes: Arc<Mailboxes>,
    index: usize,
    poke: Poke,
    done: bool,
}

impl Outcome {
    pub fn mailbox(&self) -> usize {
        self.index
    }

    /// The control region is made: the host may hand it to the ICD.
    pub fn published(mut self) {
        self.done = true;
        let _ = coldpage::transition(self.mailboxes.page(self.index), coldpage::STATE_REPLIED, STATE_PUBLISHED);
        (self.poke)();
    }

    /// The session could not be opened.
    pub fn failed(mut self, reason: &str) {
        self.done = true;
        self.fail(reason);
    }

    fn fail(&self, reason: &str) {
        let page = self.mailboxes.page(self.index);
        let b = reason.as_bytes();
        let b = &b[..b.len().min(coldpage::MAX_OFFER)];
        if page.write_bytes(coldpage::OFF_OFFER, b).is_ok()
            && let Ok(len) = page.atomic_u32(coldpage::OFF_OFFER_LEN)
        {
            len.store(b.len() as u32, Ordering::Release);
        }
        let _ = coldpage::transition(page, coldpage::STATE_REPLIED, STATE_FAILED);
        (self.poke)();
    }
}

impl Drop for Outcome {
    fn drop(&mut self) {
        if !self.done {
            self.fail("the guest's session ended before its control region was published");
        }
    }
}

/// Guest: keep one offer in the mailboxes and answer the host's replies,
/// until `stop`. `event` is what the doorman (and the sessions, as they end)
/// ring; `poke` rings the host.
pub fn serve_offers<S: OfferSource>(
    mailboxes: &Arc<Mailboxes>,
    event: &ColdEvent,
    source: &mut S,
    poke: &Poke,
    timing: GuestTiming,
    stop: &AtomicBool,
) {
    struct Pending<T> {
        ticket: T,
        claimed_since: Option<Instant>,
    }
    let mut pending: Vec<Option<Pending<S::Ticket>>> = (0..COUNT).map(|_| None).collect();
    let mut leftover_since: Vec<Option<Instant>> = vec![None; COUNT];
    let mut seen = event.generation();
    loop {
        if stop.load(Ordering::Acquire) {
            for (i, p) in pending.iter_mut().enumerate() {
                if let Some(p) = p.take() {
                    let page = mailboxes.page(i);
                    if coldpage::transition(page, coldpage::STATE_OFFERED, coldpage::STATE_EMPTY).unwrap_or(false) {
                        source.refused(p.ticket, "withdrawn: the guest's GPU process is stopping".into());
                    } else {
                        source.refused(p.ticket, "the guest's GPU process stopped during the exchange".into());
                    }
                }
            }
            return;
        }
        let now = Instant::now();
        let mut offered = 0;
        let mut claimed = false;
        for i in 0..COUNT {
            let page = mailboxes.page(i);
            let st = coldpage::state(page).unwrap_or(u32::MAX);
            let Some(mut p) = pending[i].take() else {
                // Not ours, or ours and accepted (REPLIED until the session
                // publishes). Anything else not EMPTY is a host that died
                // before it read our PUBLISHED or FAILED.
                if st == STATE_PUBLISHED || st == STATE_FAILED {
                    let since = *leftover_since[i].get_or_insert(now);
                    if now.duration_since(since) >= timing.stale_after
                        && coldpage::transition(page, st, coldpage::STATE_EMPTY).unwrap_or(false)
                    {
                        eprintln!("[gpu] mailbox {i}: nobody read the session's outcome; emptied");
                        leftover_since[i] = None;
                    }
                } else {
                    leftover_since[i] = None;
                }
                continue;
            };
            match st {
                coldpage::STATE_OFFERED => {
                    let _ = coldpage::beat(page);
                    offered += 1;
                    pending[i] = Some(p);
                }
                coldpage::STATE_CLAIMED => {
                    let since = *p.claimed_since.get_or_insert(now);
                    if now.duration_since(since) >= timing.claim_timeout
                        && coldpage::transition(page, coldpage::STATE_CLAIMED, coldpage::STATE_EMPTY).unwrap_or(false)
                    {
                        source.refused(p.ticket, format!("mailbox {i}: claimed and never answered"));
                        continue;
                    }
                    claimed = true;
                    pending[i] = Some(p);
                }
                coldpage::STATE_REPLIED => match coldpage::poll_reply(page) {
                    Ok(Some(ColdMessage::Accept { .. })) => {
                        let outcome =
                            Outcome { mailboxes: Arc::clone(mailboxes), index: i, poke: Arc::clone(poke), done: false };
                        source.accepted(p.ticket, outcome);
                    }
                    Ok(Some(ColdMessage::Refuse { code, reason })) => {
                        let _ = coldpage::transition(page, coldpage::STATE_REPLIED, coldpage::STATE_EMPTY);
                        source.refused(p.ticket, format!("the host refused ({code}): {reason}"));
                    }
                    other => {
                        let _ = coldpage::transition(page, coldpage::STATE_REPLIED, coldpage::STATE_EMPTY);
                        source.refused(p.ticket, format!("mailbox {i}: an unreadable reply ({other:?})"));
                    }
                },
                other => source.refused(p.ticket, format!("mailbox {i}: the offer vanished (state {other})")),
            }
        }
        if offered == 0 {
            // A free mailbox: EMPTY and not one of ours.
            let free = (0..COUNT).find(|&i| {
                pending[i].is_none() && coldpage::state(mailboxes.page(i)).unwrap_or(u32::MAX) == coldpage::STATE_EMPTY
            });
            if let Some(i) = free
                && let Some((hello, ticket)) = source.offer()
            {
                match coldpage::offer(mailboxes.page(i), LABEL, &hello) {
                    Ok(()) => {
                        pending[i] = Some(Pending { ticket, claimed_since: None });
                        poke();
                    }
                    Err(e) => source.refused(ticket, format!("mailbox {i}: {e}")),
                }
            }
        }
        let slice = if event.has_bell() {
            timing.bell_slice
        } else if claimed {
            timing.claimed_poll
        } else {
            timing.idle_poll
        };
        event.wait(&mut seen, slice);
    }
}

// ------------------------------------------------------------------- host ---

/// How long the host waits, and for what.
#[derive(Debug, Clone, Copy)]
pub struct HostTiming {
    /// For an offer to appear; then a full house is refused.
    pub offer_wait: Duration,
    /// Between scans with a bell.
    pub bell_slice: Duration,
    /// Between scans without one (only while an ICD waits).
    pub poll: Duration,
    /// For an offer's heartbeat to move, when the pool header's has not been
    /// seen moving.
    pub live_within: Duration,
    /// For the guest to publish an accepted session.
    pub control_wait: Duration,
    /// The pool header's beat frozen this long: the owner is gone.
    pub owner_gone: Duration,
}

impl Default for HostTiming {
    fn default() -> Self {
        HostTiming {
            offer_wait: Duration::from_secs(2),
            bell_slice: Duration::from_millis(25),
            poll: Duration::from_millis(2),
            live_within: Duration::from_millis(250),
            control_wait: Duration::from_secs(30),
            owner_gone: Duration::from_secs(5),
        }
    }
}

/// The host's memory of the pool header's beat: when it last saw it move.
#[derive(Default)]
pub struct Liveness {
    /// The nonce and beat last seen, and when the beat was last seen to move.
    last: Mutex<Option<(u64, u64, Option<Instant>)>>,
}

impl Liveness {
    pub fn new() -> Arc<Liveness> {
        Arc::new(Liveness::default())
    }

    pub fn observe(&self, s: Snapshot) {
        if !s.valid {
            return;
        }
        let mut g = self.last.lock().unwrap_or_else(|e| e.into_inner());
        match *g {
            Some((n, b, _)) if n == s.nonce && b == s.beat => {}
            // The first sight proves nothing; a change of beat (or of owner,
            // which publishes a fresh header) does.
            None => *g = Some((s.nonce, s.beat, None)),
            Some(_) => *g = Some((s.nonce, s.beat, Some(Instant::now()))),
        }
    }

    /// An offer of pool `nonce` was seen beating: as good as a header beat,
    /// so the next attach does not wait for one.
    pub fn alive(&self, nonce: u64) {
        let mut g = self.last.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((n, _, at)) = g.as_mut()
            && *n == nonce
        {
            *at = Some(Instant::now());
        }
    }

    /// Has the beat moved within `d`?
    pub fn moved_within(&self, d: Duration) -> bool {
        self.last.lock().unwrap_or_else(|e| e.into_inner()).is_some_and(|(_, _, at)| at.is_some_and(|at| at.elapsed() <= d))
    }
}

/// Host: watch the pool header's beat every 250 ms, for as long as the
/// process lives.
pub fn spawn_liveness(mailboxes: Arc<Mailboxes>, live: Arc<Liveness>) -> std::io::Result<()> {
    std::thread::Builder::new().name("sv-pool-live".into()).spawn(move || {
        loop {
            live.observe(mailboxes.header());
            std::thread::sleep(Duration::from_millis(250));
        }
    })?;
    Ok(())
}

/// Host: does this offer's heartbeat move within `within`? `None` if the
/// mailbox stopped being OFFERED meanwhile (another host claimed it, and a
/// claimed offer is not beaten, so waiting on it would only run out the
/// time). `Some(true)` early if another attach of this process proved the
/// owner alive meanwhile.
fn offer_beats(page: &SharedRegion, seen: u64, live: &Liveness, within: Duration) -> Option<bool> {
    let deadline = Instant::now() + within;
    loop {
        if coldpage::state(page).ok()? != coldpage::STATE_OFFERED {
            return None;
        }
        if coldpage::heartbeat(page).ok()? != seen || live.moved_within(BEAT_WINDOW) {
            return Some(true);
        }
        if Instant::now() >= deadline {
            return Some(false);
        }
        std::thread::sleep(Duration::from_millis(1));
    }
}

/// The window within which a header beat (once a second) counts as live.
const BEAT_WINDOW: Duration = Duration::from_secs(3);

/// A session the host attached.
#[derive(Debug)]
pub struct Attached<T> {
    pub value: T,
    pub hello: Hello,
    pub mailbox: usize,
    /// From the request to the claim.
    pub claimed_after: Duration,
    /// From the Accept to the guest's PUBLISHED.
    pub published_after: Duration,
}

/// The words a full house is refused with, which `sv-icd` retries on
/// (waycompose-design.md §95.10).
pub const FULL_HOUSE: &str = "GPU session slots are taken";

/// Host: one session out of the mailboxes, for one ICD's `AttachRequest`.
///
/// Scans for a live offer of this pool, claims it, runs `check` on its
/// `Hello` (the stamps and the nonce, `sv_hostd::ivshmem::check_slot`),
/// replies `Accept` or `Refuse`, rings, and waits for the guest to publish
/// the control region (`control_ready` reads its magic). `event` is woken by
/// the guest's [`COLD_TOKEN`] through this host's bell, if it has one; it is
/// waited on in slices either way.
#[allow(clippy::too_many_arguments)]
pub fn host_attach<T>(
    mailboxes: &Mailboxes,
    live: &Liveness,
    event: &ColdEvent,
    poke: &dyn Fn(),
    claim_nonce: u64,
    timing: HostTiming,
    check: impl FnOnce(&Hello) -> std::result::Result<T, String>,
    control_ready: impl Fn(&Hello) -> bool,
) -> std::result::Result<Attached<T>, String> {
    let started = Instant::now();
    let slice = || if event.has_bell() { timing.bell_slice } else { timing.poll };
    let mut seen = event.generation();
    let deadline = started + timing.offer_wait;
    let (index, hello) = 'found: loop {
        let snap = mailboxes.header();
        live.observe(snap);
        let mut foreign = 0;
        let mut frozen = 0;
        if snap.valid {
            for i in 0..COUNT {
                let page = mailboxes.page(i);
                let Ok(Some(o)) = coldpage::read_offer(page) else { continue };
                match o.hello.extent.locator {
                    Locator::IvshmemPool { pool_nonce, .. } if pool_nonce == snap.nonce => {}
                    _ => {
                        foreign += 1;
                        continue;
                    }
                }
                let alive = live.moved_within(BEAT_WINDOW) || {
                    match offer_beats(page, o.heartbeat, live, timing.live_within) {
                        Some(true) => {
                            live.alive(snap.nonce);
                            true
                        }
                        // Taken meanwhile: look again at once.
                        None => continue,
                        Some(false) => false,
                    }
                };
                if !alive {
                    frozen += 1;
                    continue;
                }
                if coldpage::claim_signed(page, claim_nonce).unwrap_or(false) {
                    break 'found (i, o.hello);
                }
            }
        }
        if Instant::now() >= deadline {
            let stale = if foreign + frozen > 0 {
                format!("; {foreign} offer(s) of another pool and {frozen} with a frozen heartbeat left alone")
            } else {
                String::new()
            };
            return Err(if !snap.valid {
                "the pool has no header: the guest's GPU process is not serving a pool on this device".to_string()
            } else if live.moved_within(BEAT_WINDOW) {
                format!(
                    "all {FULL_HOUSE}: no offer in the pool's mailboxes for {:.1?} (the guest's session limit, \
                     or no room in the pool for a control region){stale}",
                    timing.offer_wait
                )
            } else {
                format!(
                    "the guest's GPU process is not running: pool {:#018x}'s beat has not moved{stale}",
                    snap.nonce
                )
            });
        }
        event.wait(&mut seen, slice());
    };
    let claimed_after = started.elapsed();
    let page = mailboxes.page(index);

    let value = match check(&hello) {
        Ok(v) => v,
        Err(e) => {
            let _ = coldpage::reply_claimed(page, claim_nonce, &ColdMessage::Refuse { code: 3, reason: e.clone() });
            poke();
            return Err(e);
        }
    };
    match coldpage::reply_claimed(page, claim_nonce, &ColdMessage::Accept { generation: hello.generation }) {
        Ok(true) => {}
        Ok(false) => return Err(format!("mailbox {index}: the guest took the offer back before the Accept")),
        Err(e) => return Err(format!("mailbox {index}: Accept: {e}")),
    }
    poke();

    let accepted = Instant::now();
    let deadline = accepted + timing.control_wait;
    loop {
        let st = coldpage::state(page).map_err(|e| format!("{e}"))?;
        match st {
            STATE_PUBLISHED if control_ready(&hello) => {
                let _ = coldpage::transition(page, STATE_PUBLISHED, coldpage::STATE_EMPTY);
                break;
            }
            STATE_PUBLISHED | coldpage::STATE_REPLIED => {}
            STATE_FAILED => {
                let len = page
                    .atomic_u32(coldpage::OFF_OFFER_LEN)
                    .map(|a| a.load(Ordering::Acquire) as usize)
                    .unwrap_or(0)
                    .min(coldpage::MAX_OFFER);
                let mut b = vec![0u8; len];
                let _ = page.read_bytes(coldpage::OFF_OFFER, &mut b);
                let _ = coldpage::transition(page, STATE_FAILED, coldpage::STATE_EMPTY);
                return Err(format!("the guest could not open the session: {}", String::from_utf8_lossy(&b)));
            }
            other => {
                return Err(format!("mailbox {index}: the guest dropped the accepted session (state {other})"));
            }
        }
        let now = Instant::now();
        if now >= deadline {
            return Err(format!("the guest accepted and never published its control region in {:.1?}", timing.control_wait));
        }
        if now.duration_since(accepted) >= timing.owner_gone && !live.moved_within(timing.owner_gone) {
            live.observe(mailboxes.header());
            if !live.moved_within(timing.owner_gone) {
                return Err("the guest's GPU process stopped beating after the Accept".into());
            }
        }
        event.wait(&mut seen, slice());
    }
    Ok(Attached { value, hello, mailbox: index, claimed_after, published_after: accepted.elapsed() })
}

/// Host: a connection to the bell hub for one attach (Linux): sends
/// [`COLD_TOKEN`] and turns the guest's into wakes of `event`. Its connect is
/// a wake (the reopen rule); its end marks `event` bell-less. Dropping it
/// shuts the socket, which ends its reader.
#[cfg(target_os = "linux")]
pub struct HubBell {
    stream: std::os::unix::net::UnixStream,
    writer: Mutex<crate::vdoorbell::DontWaitWriter>,
}

#[cfg(target_os = "linux")]
impl HubBell {
    pub fn connect(path: &str, event: Arc<ColdEvent>) -> std::io::Result<HubBell> {
        use std::io::Read;
        let stream = std::os::unix::net::UnixStream::connect(path)?;
        let mut reader = stream.try_clone()?;
        let writer = Mutex::new(crate::vdoorbell::DontWaitWriter(stream.try_clone()?));
        event.reopened();
        std::thread::Builder::new().name("sv-cold-bell".into()).spawn(move || {
            let mut buf = [0u8; 256];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if buf[..n].contains(&COLD_TOKEN) {
                            event.ring();
                        }
                    }
                }
            }
            event.closed();
        })?;
        Ok(HubBell { stream, writer })
    }

    pub fn poke(&self) {
        use std::io::Write;
        if let Ok(mut w) = self.writer.lock() {
            let _ = w.write(&[COLD_TOKEN]);
        }
    }
}

#[cfg(target_os = "linux")]
impl Drop for HubBell {
    fn drop(&mut self) {
        let _ = self.stream.shutdown(std::net::Shutdown::Both);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cold::{ExtentDesc, Granularity};
    use crate::control::{ENDIAN_MARKER, Geometry, ProtocolIds};
    use std::sync::atomic::AtomicU32;

    fn hello(pool_nonce: u64, control_offset: u64, run_id: u64) -> Hello {
        Hello {
            ids: ProtocolIds::new(1, 0x1234),
            generation: 1,
            extent: ExtentDesc {
                run_id,
                granularity: Granularity::Small4K,
                control_pages: 4,
                arena_pages: 0,
                locator: Locator::IvshmemPool { control_offset, pool_nonce },
            },
            geometry: Geometry::default(),
            endianness: ENDIAN_MARKER,
            pointer_width: 64,
        }
    }

    /// A pool owner without a GPU: offers numbered control regions, counts
    /// live sessions against a limit, and publishes each accepted session
    /// after `setup`.
    struct Fake {
        nonce: u64,
        next: u64,
        max: u32,
        live: Arc<AtomicU32>,
        event: Arc<ColdEvent>,
        refused: Arc<Mutex<Vec<String>>>,
        hold: Duration,
        setup: Duration,
    }

    impl OfferSource for Fake {
        type Ticket = u64;
        fn offer(&mut self) -> Option<(Hello, u64)> {
            if self.live.load(Ordering::Acquire) >= self.max {
                return None;
            }
            self.next += 1;
            Some((hello(self.nonce, poolhdr::HEADER_LEN * self.next, self.next), self.next))
        }
        fn accepted(&mut self, _t: u64, outcome: Outcome) {
            self.live.fetch_add(1, Ordering::AcqRel);
            let (live, event, hold, setup) = (Arc::clone(&self.live), Arc::clone(&self.event), self.hold, self.setup);
            std::thread::spawn(move || {
                std::thread::sleep(setup);
                outcome.published();
                std::thread::sleep(hold);
                live.fetch_sub(1, Ordering::AcqRel);
                event.ring();
            });
        }
        fn refused(&mut self, t: u64, why: String) {
            self.refused.lock().unwrap().push(format!("{t}: {why}"));
        }
    }

    struct Guest {
        stop: Arc<AtomicBool>,
        thread: Option<std::thread::JoinHandle<()>>,
        refused: Arc<Mutex<Vec<String>>>,
        event: Arc<ColdEvent>,
    }

    impl Drop for Guest {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Release);
            self.event.ring();
            if let Some(t) = self.thread.take() {
                let _ = t.join();
            }
        }
    }

    fn owner(region: Arc<SharedRegion>, nonce: u64, max: u32, hold: Duration, timing: GuestTiming, poke: Poke, event: Arc<ColdEvent>) -> Guest {
        let mb = Arc::new(Mailboxes::new(Arc::clone(&region)).unwrap());
        mb.reset();
        PoolHeader::new(&region).unwrap().publish(nonce);
        let stop = Arc::new(AtomicBool::new(false));
        let refused = Arc::new(Mutex::new(Vec::new()));
        let mut src = Fake {
            nonce,
            next: 0,
            max,
            live: Arc::new(AtomicU32::new(0)),
            event: Arc::clone(&event),
            refused: Arc::clone(&refused),
            hold,
            setup: Duration::from_millis(5),
        };
        let thread = {
            let (stop, event) = (Arc::clone(&stop), Arc::clone(&event));
            std::thread::spawn(move || serve_offers(&mb, &event, &mut src, &poke, timing, &stop))
        };
        Guest { stop, thread: Some(thread), refused, event }
    }

    fn quick() -> HostTiming {
        HostTiming { offer_wait: Duration::from_millis(400), ..HostTiming::default() }
    }

    fn attach(mb: &Mailboxes, live: &Liveness, ev: &ColdEvent, poke: &dyn Fn(), n: u64, t: HostTiming) -> std::result::Result<Attached<()>, String> {
        host_attach(mb, live, ev, poke, n, t, |_| Ok(()), |_| true)
    }

    fn file_pair(len: usize) -> (Arc<SharedRegion>, Arc<SharedRegion>) {
        use std::os::fd::AsFd;
        static N: AtomicU32 = AtomicU32::new(0);
        let path = std::env::temp_dir().join(format!("sv-poolcold-{}-{}", std::process::id(), N.fetch_add(1, Ordering::Relaxed)));
        let f = std::fs::OpenOptions::new().read(true).write(true).create(true).truncate(true).open(&path).unwrap();
        f.set_len(len as u64).unwrap();
        let guest = SharedRegion::map_fd(f.as_fd(), 0, len).unwrap();
        let host = SharedRegion::map_fd(f.as_fd(), 0, poolhdr::HEADER_LEN as usize).unwrap();
        let _ = std::fs::remove_file(&path);
        (Arc::new(guest), Arc::new(host))
    }

    #[test]
    fn the_mailboxes_fill_the_header_granule_after_the_header() {
        assert_eq!(offset(0), 4096);
        assert_eq!(offset(COUNT - 1) + PAGE_LEN as u64, poolhdr::HEADER_LEN);
        assert_ne!(COLD_TOKEN, crate::park::HELLO_TOKEN);
        const { assert!(COLD_TOKEN & 0x80 != 0, "never a lane token") };
    }

    /// The whole exchange over two mappings of one file, as the guest's BAR
    /// and the host's map of the region file are: the guest's process owns
    /// the pool, the host attaches twice in a row, and each session is
    /// published before the host returns.
    #[cfg(target_os = "linux")]
    #[test]
    fn the_exchange_runs_over_two_mappings_of_one_file() {
        let (g, h) = file_pair(1 << 20);
        let gev = ColdEvent::new();
        let _guest = owner(g, 0xfeed, 4, Duration::from_secs(5), GuestTiming::default(), Arc::new(|| {}), Arc::clone(&gev));
        let mb = Mailboxes::new(h).unwrap();
        let (live, ev) = (Liveness::new(), ColdEvent::new());
        let poke = || gev.ring();
        let a = host_attach(&mb, &live, &ev, &poke, 11, quick(), |h| Ok(h.extent.run_id), |_| true).unwrap();
        let b = host_attach(&mb, &live, &ev, &poke, 12, quick(), |h| Ok(h.extent.run_id), |_| true).unwrap();
        assert_ne!(a.value, b.value);
        assert_ne!(a.mailbox, b.mailbox, "the next offer is in another mailbox");
        assert!(matches!(a.hello.extent.locator, Locator::IvshmemPool { pool_nonce: 0xfeed, .. }));
        // Both read and emptied; one fresh offer waits.
        let st = mb.states();
        assert_eq!(st.iter().filter(|&&s| s == coldpage::STATE_OFFERED).count(), 1, "{st:?}");
        assert!(!st.contains(&STATE_PUBLISHED), "{st:?}");
    }

    /// Offers the host must not take: another pool's (a previous owner's
    /// nonce), and this pool's from an owner whose heartbeat is frozen. Both
    /// are left exactly as they were, and the refusal says why.
    #[test]
    fn stale_offers_are_left_alone() {
        let r = Arc::new(SharedRegion::heap(poolhdr::HEADER_LEN as usize).unwrap());
        let mb = Mailboxes::new(Arc::clone(&r)).unwrap();
        PoolHeader::new(&r).unwrap().publish(0xaaaa);
        coldpage::offer(mb.page(0), LABEL, &hello(0xbbbb, poolhdr::HEADER_LEN, 1)).unwrap();
        coldpage::offer(mb.page(3), LABEL, &hello(0xaaaa, poolhdr::HEADER_LEN, 2)).unwrap();
        // The foreign one even beats: the nonce alone keeps it out.
        let stop = Arc::new(AtomicBool::new(false));
        let beater = {
            let (r, stop) = (Arc::clone(&r), Arc::clone(&stop));
            std::thread::spawn(move || {
                let mb = Mailboxes::new(r).unwrap();
                while !stop.load(Ordering::Acquire) {
                    coldpage::beat(mb.page(0)).unwrap();
                    std::thread::sleep(Duration::from_millis(1));
                }
            })
        };
        let t = HostTiming { offer_wait: Duration::from_millis(300), live_within: Duration::from_millis(30), ..HostTiming::default() };
        let e = attach(&mb, &Liveness::new(), &ColdEvent::new(), &|| {}, 1, t).unwrap_err();
        stop.store(true, Ordering::Release);
        beater.join().unwrap();
        assert!(e.contains("not running") && e.contains("1 offer(s) of another pool and 1 with a frozen"), "{e}");
        assert_eq!(coldpage::state(mb.page(0)).unwrap(), coldpage::STATE_OFFERED);
        assert_eq!(coldpage::state(mb.page(3)).unwrap(), coldpage::STATE_OFFERED);
        // No header at all.
        let r2 = Arc::new(SharedRegion::heap(poolhdr::HEADER_LEN as usize).unwrap());
        let e = attach(&Mailboxes::new(r2).unwrap(), &Liveness::new(), &ColdEvent::new(), &|| {}, 1, t).unwrap_err();
        assert!(e.contains("no header"), "{e}");
    }

    /// Two ICDs at once, from two hosts' mappings: each gets its own session,
    /// the second from the offer the guest made after the first was answered.
    #[cfg(target_os = "linux")]
    #[test]
    fn two_icds_at_once_get_two_sessions() {
        let (g, h1) = file_pair(1 << 20);
        let gev = ColdEvent::new();
        let _guest = owner(g, 0x77, 8, Duration::from_secs(5), GuestTiming::default(), Arc::new(|| {}), Arc::clone(&gev));
        let live = Liveness::new();
        let results: Vec<_> = std::thread::scope(|s| {
            let hs: Vec<_> = (0..2u64)
                .map(|k| {
                    let (h1, live, gev) = (Arc::clone(&h1), Arc::clone(&live), Arc::clone(&gev));
                    s.spawn(move || {
                        let mb = Mailboxes::new(h1).unwrap();
                        let poke = || gev.ring();
                        host_attach(&mb, &live, &ColdEvent::new(), &poke, 100 + k, quick(), |h| Ok(h.extent.run_id), |_| true)
                    })
                })
                .collect();
            hs.into_iter().map(|h| h.join().unwrap()).collect()
        });
        let ids: Vec<u64> = results.into_iter().map(|r| r.unwrap().value).collect();
        assert_ne!(ids[0], ids[1], "{ids:?}");
    }

    /// A full house: with the guest's limit at one session and that session
    /// still running, no offer appears, and the host refuses with the words
    /// `sv-icd` retries on. Once the session ends the next attach succeeds.
    #[test]
    fn a_full_house_is_refused_with_the_words_sv_icd_retries_on() {
        let r = Arc::new(SharedRegion::heap(poolhdr::HEADER_LEN as usize).unwrap());
        let gev = ColdEvent::new();
        let _guest = owner(Arc::clone(&r), 0x55, 1, Duration::from_millis(1200), GuestTiming::default(), Arc::new(|| {}), Arc::clone(&gev));
        let mb = Mailboxes::new(r).unwrap();
        let (live, ev) = (Liveness::new(), ColdEvent::new());
        // The header's beat: what tells the host the owner is live although
        // it offers nothing.
        let beat = {
            let mb = Mailboxes::new(Arc::clone(mb.region())).unwrap();
            std::thread::spawn(move || {
                for _ in 0..40 {
                    PoolHeader::new(mb.region()).unwrap().beat();
                    std::thread::sleep(Duration::from_millis(50));
                }
            })
        };
        let poke = || gev.ring();
        attach(&mb, &live, &ev, &poke, 1, quick()).unwrap();
        let e = attach(&mb, &live, &ev, &poke, 2, quick()).unwrap_err();
        assert!(e.contains(FULL_HOUSE) && e.starts_with("all "), "{e}");
        let t = HostTiming { offer_wait: Duration::from_secs(3), ..HostTiming::default() };
        attach(&mb, &live, &ev, &poke, 3, t).expect("the place comes back when the session ends");
        beat.join().unwrap();
    }

    /// The guest's check refused: the host's Refuse gives the lease back, and
    /// a guest that fails to open the session says why.
    #[test]
    fn refusals_and_failures_travel_both_ways() {
        let r = Arc::new(SharedRegion::heap(poolhdr::HEADER_LEN as usize).unwrap());
        let gev = ColdEvent::new();
        let guest = owner(Arc::clone(&r), 0x99, 4, Duration::ZERO, GuestTiming::default(), Arc::new(|| {}), Arc::clone(&gev));
        let mb = Mailboxes::new(r).unwrap();
        let (live, ev) = (Liveness::new(), ColdEvent::new());
        let poke = || gev.ring();
        let e = host_attach(&mb, &live, &ev, &poke, 1, quick(), |_| Err::<(), _>("stamps disagree".into()), |_| true).unwrap_err();
        assert_eq!(e, "stamps disagree");
        let deadline = Instant::now() + Duration::from_secs(2);
        while guest.refused.lock().unwrap().is_empty() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        let got = guest.refused.lock().unwrap().clone();
        assert!(got.iter().any(|s| s.contains("stamps disagree")), "{got:?}");
        // An Outcome dropped unused is a failure the host reads.
        let p = mb.page(14);
        coldpage::offer(p, LABEL, &hello(0x99, 0, 0)).unwrap();
        assert!(coldpage::claim_signed(p, 5).unwrap());
        assert!(coldpage::reply_claimed(p, 5, &ColdMessage::Accept { generation: 1 }).unwrap());
        drop(Outcome { mailboxes: Arc::new(Mailboxes::new(Arc::clone(mb.region())).unwrap()), index: 14, poke: Arc::new(|| {}), done: false });
        assert_eq!(coldpage::state(p).unwrap(), STATE_FAILED);
        // A reply under another host's nonce is not written.
        coldpage::offer(p, LABEL, &hello(0x99, 0, 0)).unwrap();
        assert!(coldpage::claim_signed(p, 6).unwrap());
        assert!(!coldpage::reply_claimed(p, 7, &ColdMessage::Accept { generation: 1 }).unwrap());
        assert_eq!(coldpage::state(p).unwrap(), coldpage::STATE_CLAIMED);
    }

    /// The reopen rule. The guest has a bell and sleeps a slice far longer
    /// than the test: the host's reply is made and its ring is lost (the port
    /// was closed when it was sent). Reopening the port is a wake, the guest
    /// rescans, and finds the reply.
    #[cfg(unix)]
    #[test]
    fn a_ring_lost_across_a_reopen_is_recovered_by_the_rescan() {
        use std::os::unix::net::UnixStream;
        let r = Arc::new(SharedRegion::heap(poolhdr::HEADER_LEN as usize).unwrap());
        let event = ColdEvent::new();
        let lanes = crate::vdoorbell::ControlSet::new();
        let (g1, h1) = UnixStream::pair().unwrap();
        let _d1 = guest_doorman(g1, Arc::clone(&lanes), Arc::clone(&event));
        let slow = GuestTiming { bell_slice: Duration::from_secs(60), ..GuestTiming::default() };
        let guest = owner(Arc::clone(&r), 0x31, 4, Duration::from_secs(5), slow, Arc::new(|| {}), Arc::clone(&event));
        let mb = Mailboxes::new(Arc::clone(&r)).unwrap();
        // Wait for the offer, then claim and answer without ringing: the ring
        // went into a port nobody had open.
        let deadline = Instant::now() + Duration::from_secs(2);
        let i = loop {
            if let Some(i) = (0..COUNT).find(|&i| coldpage::state(mb.page(i)).unwrap() == coldpage::STATE_OFFERED) {
                break i;
            }
            assert!(Instant::now() < deadline, "no offer");
            std::thread::sleep(Duration::from_millis(1));
        };
        drop(h1);
        assert!(coldpage::claim_signed(mb.page(i), 9).unwrap());
        assert!(coldpage::reply_claimed(mb.page(i), 9, &ColdMessage::Accept { generation: 1 }).unwrap());
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(coldpage::state(mb.page(i)).unwrap(), coldpage::STATE_REPLIED, "nothing woke the guest yet");
        // The port is opened again.
        let (g2, _h2) = UnixStream::pair().unwrap();
        let t = Instant::now();
        let _d2 = guest_doorman(g2, lanes, Arc::clone(&event));
        while coldpage::state(mb.page(i)).unwrap() != STATE_PUBLISHED {
            assert!(t.elapsed() < Duration::from_secs(5), "the reopen did not make the guest rescan");
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(t.elapsed() < Duration::from_secs(2), "{:?}", t.elapsed());
        assert!(event.opens() >= 2);
        drop(guest);
    }

    /// The bell's byte reaches the mailbox waiter and no lane; any other byte
    /// reaches the lanes and not the waiter.
    #[test]
    fn the_cold_token_wakes_the_waiter_and_nothing_else() {
        struct Lanes(AtomicU32);
        impl BellTarget for Lanes {
            fn ring_token(&self, _t: u8, _w: &AtomicU64) -> bool {
                self.0.fetch_add(1, Ordering::Relaxed);
                true
            }
        }
        let ev = ColdEvent::new();
        let t = WithCold { cold: Arc::clone(&ev), lanes: Arc::new(Lanes(AtomicU32::new(0))) };
        let w = AtomicU64::new(0);
        let mut seen = ev.generation();
        assert!(t.ring_token(COLD_TOKEN, &w));
        assert!(ev.wait(&mut seen, Duration::from_millis(1)));
        assert_eq!(t.lanes.0.load(Ordering::Relaxed), 0);
        t.ring_token(crate::park::lane_token(2, crate::ring::Lane::Reply), &w);
        assert!(!ev.wait(&mut seen, Duration::from_millis(1)));
        assert_eq!(t.lanes.0.load(Ordering::Relaxed), 1);
    }
}
