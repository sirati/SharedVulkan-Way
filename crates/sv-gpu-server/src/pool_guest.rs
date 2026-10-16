//! The guest's GPU process serving every session from one pool on the GPU
//! ivshmem device (waycompose-design.md §97).
//!
//! §87's `--ivshmem-slot` path cut the device into fixed slots; this one
//! leases it. The cold path is the same — one TCP connection per session from
//! the host's `sv-hostd ivshmem-serve`, a `Hello`, the host's `Accept`, a
//! `Ping` once the control region is published — and so is everything after
//! the rings are claimed. What differs:
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
//! It is written once for both systems. On Windows the pool is the BAR of the
//! device at `--ivshmem-slot`; on Linux, for the development loop, it is the
//! file `--pool-file`, which is exactly what the host side maps, so one
//! machine runs the whole protocol.
//!
//! [`Locator::IvshmemPool`]: sv_transport::cold::Locator::IvshmemPool

use crate::pool::{Importer, PoolServer};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use sv_arena::pool::{PoolConfig, Purpose};
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
    /// The TCP port the host's `sv-hostd` connects to.
    pub port: u16,
    pub device: usize,
    pub idle: Duration,
    /// Concurrent sessions at most.
    pub max_sessions: u32,
}

/// Serve sessions from a pool over `boot` until the listener fails.
pub fn serve_pool(boot: Arc<SharedRegion>, cfg: PoolServe, bell: Option<String>) -> Result<(), String> {
    let pool_nonce = nonce();
    PoolHeader::new(&boot).map_err(|e| format!("the pool cannot hold its header: {e}"))?.publish(pool_nonce);
    // The reserve is exactly one session's start: its control region and its
    // blob reserve.
    let pcfg = PoolConfig {
        reserve: geometry().control_size + sv_arena::pool::BLOB_LEASE,
        ..PoolConfig::for_capacity(boot.len() as u64)
    };
    let server = PoolServer::new(pcfg, Arc::clone(&boot), Importer::HostPointer).map_err(|e| format!("{e}"))?;
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
    let _doorman = match bell {
        None => None,
        Some(name) => match sv_transport::vdoorbell::PortHandle::guest_port(&name)
            .and_then(|p| sv_transport::vdoorbell::attach_set(p, Arc::clone(&set)))
        {
            Ok(d) => {
                eprintln!("[gpu] doorbell attached on {name}, shared by every session");
                Some(d)
            }
            Err(e) => {
                eprintln!("[gpu] no doorbell on {name} ({e}); waits will run out their slice");
                None
            }
        },
    };

    let listener = std::net::TcpListener::bind(("127.0.0.1", cfg.port)).map_err(|e| format!("{e}"))?;
    // The launcher waits for these words; they are the same as `win_guest`'s.
    eprintln!("[gpu] publishing the extent on 127.0.0.1:{} (ivshmem pool, one session per connection)", cfg.port);
    let live = Arc::new(std::sync::atomic::AtomicU32::new(0));
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
            let now = live.fetch_add(1, std::sync::atomic::Ordering::AcqRel) + 1;
            if now > cfg.max_sessions {
                live.fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
                // The words sv-icd waits on (§95.10): it retries a full house.
                let reason = format!("all {} GPU session slots are taken (the pool's session limit)", cfg.max_sessions);
                eprintln!("[gpu] session {session}: refused: {reason}");
                let _ = sv_transport::stream::write_message(
                    &mut stream,
                    &sv_transport::cold::ColdMessage::Refuse { code: 4, reason },
                );
                return;
            }
            let outcome = pool_session(session, &server, pool_nonce, cfg, &set, stream);
            live.fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
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

/// One pooled session: lease and stamp a control region, say hello, open a
/// device, serve, give everything back.
fn pool_session(
    session: u64,
    server: &Arc<PoolServer>,
    pool_nonce: u64,
    cfg: PoolServe,
    set: &Arc<sv_transport::vdoorbell::ControlSet>,
    mut stream: std::net::TcpStream,
) -> Result<u64, String> {
    use crate::driver::Driver;
    use crate::{arena, server::Shared, session as sess, table::Objects};
    use sv_transport::cold::{ColdMessage, ExtentDesc, Granularity, Hello, Locator};
    use sv_transport::control::{Control, ENDIAN_MARKER};
    use sv_transport::stream::{read_message, write_message};

    let geometry = geometry();
    let binding = server.open_session("?");
    // Everything leased from here is given back by `close_session`, on every
    // path out of this function except the quarantine.
    let refuse = |stream: &mut std::net::TcpStream, code: u32, reason: String| {
        let _ = write_message(stream, &ColdMessage::Refuse { code, reason: reason.clone() });
        server.close_session(binding.session);
        reason
    };
    let control = match server.lease_raw(binding.session, geometry.control_size, Purpose::Control) {
        Ok(l) => l,
        Err(e) => {
            // The words sv-icd waits on, so that it retries rather than
            // giving up: a control region comes back when a session ends.
            return Err(refuse(&mut stream, 4, format!("all GPU session slots are taken: no room for a control region ({e})")));
        }
    };
    let (region, offset) = sv_arena::pool::split(control.offset);
    let boot = server.region(region).ok_or("region 0 is not mapped")?;
    debug_assert_eq!(region, 0);
    let pages = (control.len / PAGE as u64) as usize;
    let run_id = nonce();
    for i in 0..pages {
        let page = arena_record::make_guest_page(run_id, i as u32, arena_record::MODE_IVSHMEM, 0);
        boot.write_bytes(offset + (i * PAGE) as u64, &page).map_err(|e| format!("{e}"))?;
    }
    std::sync::atomic::fence(std::sync::atomic::Ordering::Release);
    eprintln!(
        "[gpu] session {session} (pool account {}): control region at +{} KiB of region 0, run 0x{run_id:016x}",
        binding.session,
        offset >> 10
    );

    write_message(
        &mut stream,
        &ColdMessage::Hello(Box::new(Hello {
            ids: crate::protocol_ids(),
            generation: 1,
            extent: ExtentDesc {
                run_id,
                granularity: Granularity::Small4K,
                control_pages: pages as u64,
                arena_pages: 0,
                locator: Locator::IvshmemPool { control_offset: offset, pool_nonce },
            },
            geometry,
            endianness: ENDIAN_MARKER,
            pointer_width: 64,
        })),
    )
    .map_err(|e| {
        server.close_session(binding.session);
        format!("Hello: {e}")
    })?;
    let mut scratch = Vec::new();
    match read_message(&mut stream, &mut scratch) {
        Ok(ColdMessage::Accept { generation: 1 }) => {}
        Ok(ColdMessage::Refuse { code, reason }) => {
            server.close_session(binding.session);
            return Err(format!("the host refused {code}: {reason}"));
        }
        Ok(other) => {
            server.close_session(binding.session);
            return Err(format!("unexpected cold-path reply {other:?}"));
        }
        Err(e) => {
            server.close_session(binding.session);
            return Err(format!("no reply to Hello: {e}"));
        }
    }

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
        Err(e) => return Err(refuse(&mut stream, 5, e)),
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
    if let Err(e) = write_message(&mut stream, &ColdMessage::Ping) {
        eprintln!("[gpu] session {session}: could not say it is published: {e}");
    }

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
    drop(stream);

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
