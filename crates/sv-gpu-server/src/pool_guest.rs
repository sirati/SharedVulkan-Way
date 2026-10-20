//! The guest's GPU process serving every session from one pool on the GPU
//! ivshmem device (waycompose-design.md §97).
//!
//! §87's `--ivshmem-slot` path cut the device into fixed slots; this one
//! leases it. What differs from the slots:
//!
//! * the session's **control region** is a lease of the pool, stamped and
//!   named in the `Hello` by [`Locator::IvshmemPool`], which also carries the
//!   pool's nonce for the host to check against the pool header
//!   (`sv_transport::poolhdr`);
//! * the session has **no arena of its own**. Its `Objects` hold a
//!   `PoolBinding`, and the app side leases what it needs (`svLeaseArena`),
//!   each lease imported into the session's device;
//! * the session's end gives every lease back, its imports freed first, so a
//!   finished `glxtest` leaves the whole pool to the browser that follows it.
//!
//! # The cold path is in the pool
//!
//! A session starts in the pool's own header granule
//! (`sv_transport::poolcold`): this process keeps one `Hello` OFFERED in one of
//! the fifteen mailboxes after the pool header, for a control region it has
//! already leased and stamped; the host's `sv-hostd ivshmem-serve --cold pool`
//! claims it, checks it and answers in the same page; this process opens the
//! device and the control region and marks the mailbox PUBLISHED. Wakes both
//! ways are [`sv_transport::park::COLD_TOKEN`] on the doorbell port every
//! session already shares. Nothing crosses through a socket.
//!
//! `--port` keeps the TCP cold path (one connection per session, `Hello`,
//! `Accept`, `Ping`) as a **debug mode only**, and says so on every start.
//!
//! It is written once for both systems. On Windows the pool is the BAR of the
//! device at `--ivshmem-slot`; on Linux, for the development loop, it is the
//! file `--pool-file`, which is exactly what the host side maps, so one
//! machine runs the whole protocol.
//!
//! [`Locator::IvshmemPool`]: sv_transport::cold::Locator::IvshmemPool

use crate::pool::{Importer, PoolBinding, PoolServer};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use sv_arena::pool::{Lease, PoolConfig, Purpose};
use sv_transport::cold::{ColdMessage, Hello};
use sv_transport::poolcold::{self, ColdEvent, Mailboxes, OfferSource, Outcome};
use sv_transport::poolhdr::{self, PoolHeader};
use sv_transport::region::{PAGE, SharedRegion};

/// A nonce for a pool owner or a session's stamps: the time and the pid,
/// mixed. Odd, so it is never zero.
pub fn nonce() -> u64 {
    let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos() as u64).unwrap_or(0);
    (t.wrapping_mul(0x9E37_79B9_7F4A_7C15).rotate_left(17) ^ (u64::from(std::process::id()) << 32)) | 1
}

/// The settings of a pool-serving process.
#[derive(Debug, Clone, Copy)]
pub struct PoolServe {
    /// `Some`: the TCP cold path on this port, for debugging only. `None`
    /// (the default and what the launcher passes): the pool's mailboxes.
    pub port: Option<u16>,
    pub device: usize,
    pub idle: Duration,
    /// Concurrent sessions at most.
    pub max_sessions: u32,
    /// §107: end the process after this long with no session. The GPU
    /// process is the user's (every installation's sessions lease from it),
    /// so nothing else ends it; `None` serves for ever, as before.
    pub exit_idle: Option<Duration>,
}

/// §110: called once the pool exists, with the pool and its region 0, by a
/// process that also leases from it for something else: the shared
/// compositor, which makes it a region of its Wayland arena (`sv-guest
/// wayland … --with-gpu …`).
pub type PoolHook = Box<dyn Fn(&Arc<PoolServer>, &Arc<SharedRegion>) + Send + Sync>;

static POOL_HOOK: std::sync::OnceLock<PoolHook> = std::sync::OnceLock::new();

/// Install the [`PoolHook`]; the first call wins.
pub fn set_pool_hook(hook: PoolHook) {
    let _ = POOL_HOOK.set(hook);
}

/// The GPU process's doorbell port from `--bell`'s spec (§111.8): on a
/// Windows guest the broker opens the port and moves it here; without a
/// broker, or for any other spelling, [`PortHandle::open`] as before.
///
/// [`PortHandle::open`]: sv_transport::vdoorbell::PortHandle::open
pub fn open_bell(spec: &str) -> std::io::Result<sv_transport::vdoorbell::PortHandle> {
    #[cfg(windows)]
    if let Some(p) = wc_broker::client::lend::port(spec, "gpu") {
        return Ok(sv_transport::vdoorbell::PortHandle::Pipe(Arc::new(p)));
    }
    sv_transport::vdoorbell::PortHandle::open(spec)
}

/// The ivshmem device in PCI slot `slot`, mapped (§111.8): lent by the broker
/// when there is one, opened here otherwise; mapped by the same code either way.
#[cfg(windows)]
pub fn map_ivshmem_slot(slot: u8) -> std::io::Result<(wc_handle::MappedView, u64, String)> {
    use wc_broker::client::lend;
    if let Some(d) = lend::ivshmem_name(slot).and_then(|n| lend::ivshmem(&n, "gpu")) {
        return wc_handle::map_ivshmem_device(d);
    }
    wc_handle::map_ivshmem_slot(slot)
}

/// Serve sessions from a pool over `boot`, for the life of the process.
pub fn serve_pool(boot: Arc<SharedRegion>, cfg: PoolServe, bell: Option<String>) -> Result<(), String> {
    // SAFETY: only the address is taken, to name the pool's range for its
    // doorbell (§110); nothing is read or written through it here.
    let boot_base = unsafe { boot.base_ptr() } as usize;
    let pool_nonce = nonce();
    let mailboxes = Arc::new(Mailboxes::new(Arc::clone(&boot)).map_err(|e| format!("the pool cannot hold its header: {e}"))?);
    // A previous owner's offers are forgotten before the new nonce is out.
    mailboxes.reset();
    PoolHeader::new(&boot).map_err(|e| format!("the pool cannot hold its header: {e}"))?.publish(pool_nonce);
    // The reserve is exactly one session's start: its control region and its
    // blob reserve. The offer waiting in a mailbox holds the control region.
    let pcfg = PoolConfig {
        reserve: geometry().control_size + sv_arena::pool::BLOB_LEASE,
        ..PoolConfig::for_capacity(boot.len() as u64)
    };
    let server = PoolServer::new(pcfg, Arc::clone(&boot), Importer::HostPointer).map_err(|e| format!("{e}"))?;
    if let Some(hook) = POOL_HOOK.get() {
        hook(&server, &boot);
    }
    eprintln!(
        "[gpu] pool of {} MiB, nonce {pool_nonce:#018x}; a session may lease {} MiB besides its control \
         region, and {} MiB are kept for a session that has not started; {}",
        boot.len() >> 20,
        pcfg.session_cap >> 20,
        pcfg.reserve >> 20,
        server.report()
    );

    // The owner's beat, once a second: what tells the host the pool is served.
    {
        let boot = Arc::clone(&boot);
        std::thread::Builder::new()
            .name("sv-pool-beat".into())
            .spawn(move || {
                loop {
                    if let Ok(h) = PoolHeader::new(&boot) {
                        h.beat();
                    }
                    std::thread::sleep(Duration::from_secs(1));
                }
            })
            .map_err(|e| format!("{e}"))?;
    }

    let set = sv_transport::vdoorbell::ControlSet::new();
    let cold = ColdEvent::new();
    // `PortHandle::open`: a port name on the guest (Windows' `\\.\Global\…`),
    // or `socket:PATH` / a path, which is how the Linux development loop
    // stands a socket in for the port.
    let _doorman = match &bell {
        None => None,
        Some(name) => match open_bell(name)
            .and_then(|p| poolcold::attach_guest_bell_for(p, Arc::clone(&set), Arc::clone(&cold), boot_base, boot.len()))
        {
            Ok(d) => {
                eprintln!("[gpu] doorbell attached on {name}, shared by every session and the pool's mailboxes");
                Some(d)
            }
            Err(e) => {
                eprintln!("[gpu] no doorbell on {name} ({e}); waits will run out their slice, the mailboxes are polled");
                None
            }
        },
    };
    let live = Arc::new(AtomicU32::new(0));
    if let Some(after) = cfg.exit_idle {
        let live = Arc::clone(&live);
        std::thread::Builder::new()
            .name("sv-pool-idle".into())
            .spawn(move || {
                let mut since = Instant::now();
                loop {
                    std::thread::sleep(Duration::from_secs(1));
                    if live.load(Ordering::SeqCst) > 0 {
                        since = Instant::now();
                    } else if since.elapsed() >= after {
                        eprintln!("[gpu] no session for {after:?}: this user's GPU process ends (§107); the next session starts another");
                        std::process::exit(0);
                    }
                }
            })
            .map_err(|e| format!("{e}"))?;
    }

    if let Some(port) = cfg.port {
        return serve_tcp(port, server, pool_nonce, cfg, set, live);
    }

    // The launcher waits for these words; they are the same as `win_guest`'s.
    eprintln!(
        "[gpu] publishing the extent in the pool's mailboxes (ivshmem pool, {} mailboxes at +{} KiB, one session \
         per offer, at most {}; wakes {})",
        poolcold::COUNT,
        poolcold::offset(0) >> 10,
        cfg.max_sessions,
        if _doorman.is_some() { "by doorbell" } else { "polled: no doorbell" }
    );
    let mut offers = Offers { server, pool_nonce, cfg, set, live, cold: Arc::clone(&cold), n: 0, short: None };
    let poke: poolcold::Poke = Arc::new(move || poolcold::poke_cross_at(boot_base));
    poolcold::serve_offers(
        &mailboxes,
        &cold,
        &mut offers,
        &poke,
        poolcold::GuestTiming::default(),
        &AtomicBool::new(false),
    );
    Ok(())
}

/// The pool's offers: one prepared session start per offer, a thread per
/// accepted session.
struct Offers {
    server: Arc<PoolServer>,
    pool_nonce: u64,
    cfg: PoolServe,
    set: Arc<sv_transport::vdoorbell::ControlSet>,
    live: Arc<AtomicU32>,
    cold: Arc<ColdEvent>,
    n: u64,
    /// Why the last offer could not be made, so it is said once.
    short: Option<String>,
}

impl OfferSource for Offers {
    type Ticket = Prepared;

    fn offer(&mut self) -> Option<(Hello, Prepared)> {
        let live = self.live.load(Ordering::Acquire);
        if live >= self.cfg.max_sessions {
            let why = format!("{live} session(s) live, the limit");
            if self.short.as_deref() != Some(why.as_str()) {
                eprintln!("[gpu] no offer: {why}");
                self.short = Some(why);
            }
            return None;
        }
        self.n += 1;
        match prepare(self.n, &self.server) {
            Ok(p) => {
                self.short = None;
                let hello = p.hello(self.pool_nonce);
                Some((hello, p))
            }
            Err(e) => {
                if self.short.as_deref() != Some(e.as_str()) {
                    eprintln!("[gpu] no offer: {e}");
                    self.short = Some(e);
                }
                None
            }
        }
    }

    fn accepted(&mut self, p: Prepared, outcome: Outcome) {
        self.live.fetch_add(1, Ordering::AcqRel);
        let (server, cfg, set, live, cold) =
            (Arc::clone(&self.server), self.cfg, Arc::clone(&self.set), Arc::clone(&self.live), Arc::clone(&self.cold));
        let session = p.session;
        let mailbox = outcome.mailbox();
        eprintln!("[gpu] session {session}: accepted in mailbox {mailbox}");
        let spawned = std::thread::Builder::new().name(format!("sv-pool-session-{session}")).spawn(move || {
            let outcome = run_prepared(p, &server, cfg, &set, move |r| match r {
                Ok(()) => outcome.published(),
                Err(e) => outcome.failed(&e),
            });
            live.fetch_sub(1, Ordering::AcqRel);
            // A place is free: the owner may offer again.
            cold.ring();
            match outcome {
                Ok(cmds) => eprintln!("[gpu] session {session} finished, {cmds} commands; {}", server.report()),
                Err(e) => eprintln!("[gpu] session {session} ended: {e}; {}", server.report()),
            }
        });
        if let Err(e) = spawned {
            // The ticket and the outcome went with the closure: the outcome's
            // drop tells the host, and the lease is kept (a leak, not a reuse).
            self.live.fetch_sub(1, Ordering::AcqRel);
            eprintln!("[gpu] session {session}: no thread for it: {e}");
        }
    }

    fn refused(&mut self, p: Prepared, why: String) {
        self.server.close_session(p.binding.session);
        eprintln!("[gpu] session {}: offer ended unaccepted: {why}", p.session);
    }
}

/// The TCP cold path, kept only to debug the mailboxes against it.
fn serve_tcp(
    port: u16,
    server: Arc<PoolServer>,
    pool_nonce: u64,
    cfg: PoolServe,
    set: Arc<sv_transport::vdoorbell::ControlSet>,
    live: Arc<AtomicU32>,
) -> Result<(), String> {
    eprintln!(
        "[gpu] GPU COLD PATH ON TCP (debug only): --port {port} was given; the deployed cold path is the pool's \
         mailboxes, and nothing but a debugging session should pass --port"
    );
    let listener = std::net::TcpListener::bind(("127.0.0.1", port)).map_err(|e| format!("{e}"))?;
    // The launcher waits for these words; they are the same as `win_guest`'s.
    eprintln!("[gpu] publishing the extent on 127.0.0.1:{port} (ivshmem pool, one session per connection)");
    let mut n = 0u64;
    for conn in listener.incoming() {
        let mut stream = match conn {
            Ok(s) => s,
            Err(e) => {
                eprintln!("[gpu] accept: {e}");
                continue;
            }
        };
        let _ = stream.set_nodelay(true);
        n += 1;
        let session = n;
        let server = Arc::clone(&server);
        let set = Arc::clone(&set);
        let live = Arc::clone(&live);
        std::thread::spawn(move || {
            let now = live.fetch_add(1, Ordering::AcqRel) + 1;
            if now > cfg.max_sessions {
                live.fetch_sub(1, Ordering::AcqRel);
                // The words sv-icd waits on (§95.10): it retries a full house.
                let reason = format!("all {} GPU session slots are taken (the pool's session limit)", cfg.max_sessions);
                eprintln!("[gpu] session {session}: refused: {reason}");
                let _ = sv_transport::stream::write_message(&mut stream, &ColdMessage::Refuse { code: 4, reason });
                return;
            }
            let outcome = tcp_session(session, &server, pool_nonce, cfg, &set, stream);
            live.fetch_sub(1, Ordering::AcqRel);
            match outcome {
                Ok(cmds) => eprintln!("[gpu] session {session} finished, {cmds} commands; {}", server.report()),
                Err(e) => eprintln!("[gpu] session {session} ended: {e}; {}", server.report()),
            }
        });
    }
    Ok(())
}

/// The control region's geometry: §87's rings, in a region exactly as large
/// as their layout (rounded to the pool's granule). §87's 16 MiB was twice what
/// the layout uses — sixteen rings of two 256 KiB lanes are 8 MiB — and on a
/// pool every byte a control region does not use is a byte no session can
/// lease. The arena fields only size the (unused) slab directory.
pub fn geometry() -> sv_transport::control::Geometry {
    let mut g = sv_transport::control::Geometry::for_arena(64 << 20);
    if let Ok(l) = sv_transport::control::Layout::compute(g) {
        g.control_size = l.used.next_multiple_of(64 << 10);
    }
    g
}

/// A session's start, made before anybody asked for it: an account, a leased
/// control region stamped page by page, and the run id the stamps carry.
/// Everything leased is given back by `close_session` on the account.
struct Prepared {
    session: u64,
    binding: PoolBinding,
    control: Lease,
    offset: u64,
    pages: usize,
    run_id: u64,
}

impl Prepared {
    fn hello(&self, pool_nonce: u64) -> Hello {
        use sv_transport::cold::{ExtentDesc, Granularity, Locator};
        Hello {
            ids: crate::protocol_ids(),
            generation: 1,
            extent: ExtentDesc {
                run_id: self.run_id,
                granularity: Granularity::Small4K,
                control_pages: self.pages as u64,
                arena_pages: 0,
                locator: Locator::IvshmemPool { control_offset: self.offset, pool_nonce },
            },
            geometry: geometry(),
            endianness: sv_transport::control::ENDIAN_MARKER,
            pointer_width: 64,
        }
    }
}

/// Lease and stamp a session's control region. The error carries the words
/// sv-icd retries on when it is the pool that is full.
fn prepare(session: u64, server: &Arc<PoolServer>) -> Result<Prepared, String> {
    let geometry = geometry();
    let binding = server.open_session("?");
    let control = match server.lease_raw(binding.session, geometry.control_size, Purpose::Control) {
        Ok(l) => l,
        Err(e) => {
            server.close_session(binding.session);
            // The words sv-icd waits on, so that it retries rather than
            // giving up: a control region comes back when a session ends.
            return Err(format!("all GPU session slots are taken: no room for a control region ({e})"));
        }
    };
    let (region, offset) = sv_arena::pool::split(control.offset);
    let Some(boot) = server.region(region) else {
        server.close_session(binding.session);
        return Err("region 0 is not mapped".into());
    };
    debug_assert_eq!(region, 0);
    let pages = (control.len / PAGE as u64) as usize;
    let run_id = nonce();
    for i in 0..pages {
        let page = arena_record::make_guest_page(run_id, i as u32, arena_record::MODE_IVSHMEM, 0);
        if let Err(e) = boot.write_bytes(offset + (i * PAGE) as u64, &page) {
            server.close_session(binding.session);
            return Err(format!("{e}"));
        }
    }
    std::sync::atomic::fence(Ordering::Release);
    eprintln!(
        "[gpu] session {session} (pool account {}): control region at +{} KiB of region 0, run 0x{run_id:016x}",
        binding.session,
        offset >> 10
    );
    Ok(Prepared { session, binding, control, offset, pages, run_id })
}

/// The TCP debug path's one session: prepare, `Hello`, the host's reply, and
/// a `Ping` once published.
fn tcp_session(
    session: u64,
    server: &Arc<PoolServer>,
    pool_nonce: u64,
    cfg: PoolServe,
    set: &Arc<sv_transport::vdoorbell::ControlSet>,
    mut stream: std::net::TcpStream,
) -> Result<u64, String> {
    use sv_transport::stream::{read_message, write_message};
    let p = match prepare(session, server) {
        Ok(p) => p,
        Err(e) => {
            let _ = write_message(&mut stream, &ColdMessage::Refuse { code: 4, reason: e.clone() });
            return Err(e);
        }
    };
    let account = p.binding.session;
    if let Err(e) = write_message(&mut stream, &ColdMessage::Hello(Box::new(p.hello(pool_nonce)))) {
        server.close_session(account);
        return Err(format!("Hello: {e}"));
    }
    let mut scratch = Vec::new();
    let reply = read_message(&mut stream, &mut scratch);
    let refused = match reply {
        Ok(ColdMessage::Accept { generation: 1 }) => None,
        Ok(ColdMessage::Refuse { code, reason }) => Some(format!("the host refused {code}: {reason}")),
        Ok(other) => Some(format!("unexpected cold-path reply {other:?}")),
        Err(e) => Some(format!("no reply to Hello: {e}")),
    };
    if let Some(e) = refused {
        server.close_session(account);
        return Err(e);
    }
    run_prepared(p, server, cfg, set, |r| {
        let m = match r {
            Ok(()) => ColdMessage::Ping,
            Err(reason) => ColdMessage::Refuse { code: 5, reason },
        };
        if let Err(e) = write_message(&mut stream, &m) {
            eprintln!("[gpu] session {session}: could not say whether it is published: {e}");
        }
    })
}

/// An accepted session: open a device, make the control region over the
/// stamps, say so (`published`), serve, give everything back.
fn run_prepared(
    p: Prepared,
    server: &Arc<PoolServer>,
    cfg: PoolServe,
    set: &Arc<sv_transport::vdoorbell::ControlSet>,
    published: impl FnOnce(Result<(), String>),
) -> Result<u64, String> {
    use crate::driver::Driver;
    use crate::{arena, server::Shared, session as sess, table::Objects};
    use sv_transport::control::Control;

    let Prepared { session, binding, control, offset, .. } = p;
    let geometry = geometry();
    let Some(boot) = server.region(0) else {
        let e = "region 0 is not mapped".to_string();
        published(Err(e.clone()));
        server.close_session(binding.session);
        return Err(e);
    };

    // SAFETY: the control lease is inside region 0's mapping, which the pool
    // server holds for the life of the process, and the lease is this
    // session's until `close_session`, which runs after `control_region` and
    // everything made from it are gone.
    let control_region = Arc::new(unsafe { SharedRegion::from_raw(boot.base_ptr().add(offset as usize), control.len as usize) });

    let t = Instant::now();
    let setup = (|| -> Result<_, String> {
        let driver = Driver::open(cfg.device).map_err(|e| format!("Driver::open: {e}"))?;
        let mut gpu_arena = arena::map_lazy(Arc::clone(&boot));
        gpu_arena.memory_type = crate::pool::host_pointer_memory_type(&driver, &boot, poolhdr::HEADER_LEN)?;
        let control = Control::create(Arc::clone(&control_region), geometry, crate::protocol_ids(), 1)
            .map_err(|e| format!("Control::create: {e}"))?;
        control.stamp_epoch().map_err(|e| format!("stamp_epoch: {e}"))?;
        Ok((driver, gpu_arena, control))
    })();
    let (driver, gpu_arena, control) = match setup {
        Ok(v) => v,
        Err(e) => {
            published(Err(e.clone()));
            drop(control_region);
            server.close_session(binding.session);
            return Err(e);
        }
    };
    eprintln!(
        "[gpu] session {session}: {} ({}), leased arena (memory type {}), device and control region in {:.2?}",
        driver.name,
        driver.driver_key(),
        gpu_arena.memory_type,
        t.elapsed()
    );
    let control = Arc::new(control);
    set.add(&control);
    published(Ok(()));

    let fns = driver.load_fns();
    crate::entry::report_missing_pub(&fns);
    let mut objects = Objects::new();
    objects.set_pooled(binding.clone());
    let shared = Arc::new(Shared {
        driver: Arc::new(driver),
        arena: gpu_arena,
        objects: Mutex::new(objects),
        fns: Arc::new(fns),
        queue_lock: Arc::new(Mutex::new(())),
        pool_lock: Arc::new(Mutex::new(())),
        video_stable: Default::default(),
        poisoned: Default::default(),
    });
    let served = sess::serve(Arc::clone(&control), Arc::clone(&shared), geometry.ring_slot_count, cfg.idle);

    // As §87's slot path: destroy what the application left if this side
    // knows how; otherwise keep the device and its leases for ever.
    let leftovers = shared.objects.lock().map(|o| o.leftovers()).unwrap_or_default();
    let mut clean = Arc::strong_count(&shared) == 1;
    // A session this side ended with an error may still have its application:
    // its ICD still maps the control region. Giving the region to the next
    // session would re-create a live session's rings under it (§97.10), so it
    // is kept, with its leases, until this process ends.
    if let Err(e) = &served {
        eprintln!("[gpu] session {session} ended by this side ({e}); its control region and leases are kept");
        clean = false;
    }
    if clean && !leftovers.is_empty() {
        let destroyed = shared.objects.lock().map_err(|_| vec!["poisoned".to_string()]).and_then(|mut o| unsafe {
            let _ = shared.driver.device.device_wait_idle();
            o.destroy_leftovers(&shared.driver.device, ash::vk::DeviceMemory::null())
        });
        match destroyed {
            Ok(n) => eprintln!("[gpu] session {session}: destroyed {n} object(s) it left behind ({leftovers:?})"),
            Err(kinds) => {
                eprintln!(
                    "[gpu] session {session}: left behind {leftovers:?}, including {kinds:?}, which this side \
                     does not destroy; keeping its device and its leases"
                );
                clean = false;
            }
        }
    }
    if clean {
        drop(control);
        match Arc::try_unwrap(shared) {
            Ok(s) => {
                let leases = s.objects.lock().map(|mut o| o.take_leases()).unwrap_or_default();
                let n = unsafe {
                    let _ = s.driver.device.device_wait_idle();
                    crate::pool::end_session(&binding, &s.driver.device, leases)
                };
                drop(s);
                eprintln!("[gpu] session {session}: {n} lease(s) and the control region given back");
            }
            Err(s) => {
                server.quarantine(binding.session);
                std::mem::forget(s);
                return Err("the session's state is still shared; its leases are kept".into());
            }
        }
    } else {
        server.quarantine(binding.session);
        std::mem::forget(shared);
        std::mem::forget(control);
    }
    drop(control_region);
    served
}


#[cfg(test)]
mod tests {
    /// The control region is the layout's size, not §87's 16 MiB, and still
    /// holds the layout.
    #[test]
    fn a_pooled_control_region_is_as_large_as_its_layout() {
        let g = super::geometry();
        let used = sv_transport::control::Layout::compute(g).unwrap().used;
        assert!(g.control_size >= used && g.control_size - used < 64 << 10);
        assert!(g.control_size <= 9 << 20, "{} bytes", g.control_size);
        assert!(g.control_size.is_multiple_of(64 << 10));
    }
}
