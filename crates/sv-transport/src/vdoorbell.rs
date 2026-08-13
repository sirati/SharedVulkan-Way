//! A doorbell the hypervisor actually carries, over a virtio-serial port.
//!
//! # The defect this exists to remove
//!
//! `vkQueuePresentKHR` costs 1.7-2.1 ms locally and **216.9 ms** across the VM
//! boundary (median of 480 frames, min 213.9, max 228.7, load ~1 — a 7% spread
//! is not queueing). The per-frame `XSync` was cleared by control and so was
//! guest contention. What is left is [`crate::park`]'s own admission: a
//! `FUTEX_WAKE` or a `WakeByAddressAll` reaches a waiter **on this kernel**, and
//! across a hypervisor it reaches nobody. Every wait therefore ran out its
//! slice, and one present makes about eleven waits.
//!
//! [`Futex::first_slice`](crate::park::Futex::first_slice) shortened the slice
//! and bought a measured 16.3x. **It is not the fix and must not be mistaken for
//! one.** A ladder is a poll at a better duty cycle: the waiter is still asking
//! repeatedly whether anything happened, and the floor on the guest is 1 ms
//! because `WaitOnAddress` takes whole milliseconds. This module is the fix, and
//! the difference is categorical — the waiter blocks indefinitely and the
//! *hypervisor* delivers an interrupt when there is something to see.
//!
//! # The mechanism, which is one thread and no new wait
//!
//! A virtio-serial port is a byte pipe between guest and host, and a write on
//! one side is a virtqueue notification: a vmexit, an interrupt injected into
//! the other side, and a blocked read returning. Tens of microseconds, carried
//! by the hypervisor because that is what the hypervisor is.
//!
//! ```text
//!   producer                                        consumer
//!   --------                                        --------
//!   publish (Release store to tail)
//!   parked != 0 ?                                   blocked in FUTEX_WAIT
//!   wake += 1; sys::wake()      -- reaches nobody -->
//!   CrossWake::poke(token)  ==[ virtio-serial ]==>   reader thread wakes
//!                                                    Bell::ring_local()
//!                                                    sys::wake()  -- same kernel
//!                                                                     --> returns
//! ```
//!
//! The load-bearing observation is that **the receiving side needs no new way to
//! wait.** `FUTEX_WAIT` already blocks correctly; the only thing missing is
//! something to end it early. So one thread, blocked on the port, converts an
//! arriving byte into an ordinary same-kernel [`Bell::ring_local`] — and every
//! existing waiter is woken by the mechanism it already uses, with **not one
//! call site changed**. `session::serve`'s drain threads, the ICD's
//! `Transport::call`, and §36's Wayland rings all benefit without knowing this
//! module exists.
//!
//! # Framing: there is none, and that is deliberate
//!
//! One byte, one wake. [`crate::park::lane_token`] packs slot and lane into
//! seven bits with the top bit reserved zero.
//!
//! Every other byte pipe in this crate is framed with a magic and a length
//! ([`crate::stream`]) because a `ColdMessage` that is misparsed is a corrupt
//! session. A doorbell has the opposite property: **a lost byte costs one slice
//! and a spurious byte costs one syscall**, so there is nothing to resynchronise
//! and a reader cannot lose its place in a stream of independent single bytes. A
//! stale byte from a previous session, or a console banner written to the wrong
//! port, is at worst a wake on a lane that then finds its ring empty — which is
//! exactly what a spurious wake is supposed to cost. Adding a magic here would
//! buy nothing and would turn a self-resynchronising stream into one that can
//! desynchronise.
//!
//! # No descriptor crosses (§1a, §14.7)
//!
//! [`CrossWake::poke`] takes a `u8`. The port carries bytes. There is no
//! parameter here through which a descriptor could be named, which is the same
//! argument §14.7 makes about the Wayland hop and is if anything easier to make:
//! a byte is not a descriptor in any encoding.
//!
//! # Deployment, stated precisely because it is the part that is not done
//!
//! This module is mechanism. Running it against the live guest needs a port, and
//! the tree currently says that is expensive — `scripts/sv-guest-host.sh`:
//!
//! > The guest has exactly two virtio-serial ports and both are spoken for;
//! > adding a third means editing this machine's NixOS configuration and
//! > restarting a VM other people are using.
//!
//! **That is out of date and the restart is avoidable.** The controller already
//! exists (`virtio-serial-pci,id=vioserial`), so a port can be *hot-added* to
//! the running guest over QMP — which the host configuration's own comment says,
//! at `testhost/modules/windows-vm.nix` near the `vioserial` device, and which
//! `docs/hardware-constraints.md` repeats ("Ports can be hot-added to a running
//! guest"). The QMP socket is `/run/windows-vm/qmp.sock`. Nothing in either
//! repository issues `chardev-add`/`device_add` yet.
//!
//! Two constraints that shape what can be built on top, recorded here so the
//! next person does not rediscover them:
//!
//! 1. **A virtio-serial port permits exactly one opener.** So one port serves
//!    one guest *process*. `sv-guest` dispatches on `argv[1]` and runs the GPU
//!    server and the compositor as separate processes, and N Vulkan sessions are
//!    N processes on N ports — so a doorbell for all of them is N ports, or one
//!    port and a guest-side daemon that cannot reach the others' private
//!    `VirtualAlloc` windows. The clean resolution is the one `docs/one-binary.md`
//!    already wants for other reasons: both halves in one process, one control
//!    region, one port.
//! 2. **There is no `open_virtio_port` for Windows.** `crate::stream`'s is
//!    `#[cfg(target_os = "linux")]` and opens `/dev/virtio-ports/<name>`; the
//!    Windows spelling is `\\.\Global\<name>` opened with `CreateFileW`, which
//!    `crate::winpipe` already has the machinery for. [`PortWake`] and
//!    [`serve_port`] are written against `Read`/`Write` precisely so that
//!    filling that gap changes nothing here.

use crate::control::Control;
use crate::park::CrossWake;
use crate::ring::RingCtrl;
use std::io::{Read, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

/// The producer half: a byte pipe to poke.
///
/// Generic over the writer so that a `File` on `/dev/virtio-ports/...`, a
/// `UnixStream` on the host end of the chardev, a Windows handle and a
/// `socketpair` in a test are all the same thing — which is the same argument
/// [`crate::stream::ColdChannel`] makes, for the same reason.
pub struct PortWake<W: Write + Send> {
    port: std::sync::Mutex<W>,
    /// Bytes written, and bytes that could not be. Diagnostics, and the two
    /// numbers to print when presents are slow again.
    poked: AtomicU64,
    dropped: AtomicU64,
}

impl<W: Write + Send> PortWake<W> {
    pub fn new(port: W) -> PortWake<W> {
        PortWake { port: std::sync::Mutex::new(port), poked: AtomicU64::new(0), dropped: AtomicU64::new(0) }
    }

    /// `(poked, dropped)`.
    pub fn stats(&self) -> (u64, u64) {
        (self.poked.load(Ordering::Relaxed), self.dropped.load(Ordering::Relaxed))
    }
}

impl<W: Write + Send> CrossWake for PortWake<W> {
    fn poke(&self, token: u8) {
        // A failed poke is counted and swallowed, never propagated. This runs
        // inside `RingProducer::commit`, on the path that has just successfully
        // published a packet, and a doorbell that could turn a good publish into
        // an error would be a worse bug than the latency it exists to fix — the
        // consumer's slice still bounds the damage, which is precisely what
        // `slice` being finite is for.
        let Ok(mut p) = self.port.lock() else {
            self.dropped.fetch_add(1, Ordering::Relaxed);
            return;
        };
        // No flush of a partial write and no retry: the byte is one byte, and a
        // port that took none of it is a port that is gone.
        match p.write_all(&[token]).and_then(|()| p.flush()) {
            Ok(()) => self.poked.fetch_add(1, Ordering::Relaxed),
            Err(_) => self.dropped.fetch_add(1, Ordering::Relaxed),
        };
    }
}

/// Why [`serve_port`] returned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServeEnd {
    /// The port closed. On a virtio-serial port this means the host end
    /// disconnected, which is the ordinary end of a session.
    Closed,
    /// [`Doorman::stop`] was called.
    Stopped,
    /// The port erred. The wakes stop; the slices resume; nothing breaks.
    Failed,
}

/// The consumer half: block on the port and convert each byte into a local wake.
///
/// Runs until the port closes or `stop` is set, so it belongs on its own thread
/// — see [`Doorman::spawn`], which is the form callers should use.
pub fn serve_port<R: Read>(
    mut port: R,
    control: &Control,
    stop: &AtomicBool,
    woken: &AtomicU64,
) -> ServeEnd {
    let mut buf = [0u8; 64];
    loop {
        if stop.load(Ordering::Acquire) {
            return ServeEnd::Stopped;
        }
        // A blocking read. This thread costs nothing while it waits, which is
        // the whole difference between this and a poll: there is no interval
        // here to choose and no duty cycle to trade off.
        let n = match port.read(&mut buf) {
            Ok(0) => return ServeEnd::Closed,
            Ok(n) => n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => return ServeEnd::Failed,
        };
        // Several bytes may arrive in one read when the peer rang more than one
        // lane before this thread was scheduled. Each is independent.
        for &token in &buf[..n] {
            let (slot, lane) = crate::park::lane_from_token(token);
            // Every failure here is a *bad token*, and the right response to one
            // is to ignore it: a slot out of range or a ring that is not mapped
            // is a stale or corrupt byte, and this module's framing argument is
            // that such a byte must cost nothing. Erroring out would let one
            // stray byte take the doorbell down for the whole session.
            let Ok(ctrl) = RingCtrl::open(control, slot) else { continue };
            let Ok(bell) = ctrl.bell(lane) else { continue };
            bell.ring_local();
            woken.fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// The reader thread, owned.
///
/// Named for what it does rather than what it is: it answers the door.
pub struct Doorman {
    stop: Arc<AtomicBool>,
    woken: Arc<AtomicU64>,
    handle: Option<std::thread::JoinHandle<ServeEnd>>,
}

impl Doorman {
    /// Start answering the door.
    ///
    /// `control` is cloned into the thread, which is why it is taken by
    /// `Arc`: the reader outlives the caller's borrow by construction.
    pub fn spawn<R: Read + Send + 'static>(port: R, control: Arc<Control>) -> Doorman {
        let stop = Arc::new(AtomicBool::new(false));
        let woken = Arc::new(AtomicU64::new(0));
        let handle = {
            let stop = Arc::clone(&stop);
            let woken = Arc::clone(&woken);
            std::thread::Builder::new()
                .name("sv-doorbell".into())
                .spawn(move || serve_port(port, &control, &stop, &woken))
                .expect("a thread")
        };
        Doorman { stop, woken, handle: Some(handle) }
    }

    /// How many wakes have been delivered. The number that says whether the
    /// doorbell is working, and the first thing to print when it is not.
    pub fn woken(&self) -> u64 {
        self.woken.load(Ordering::Relaxed)
    }

    /// Ask the thread to stop. It will not notice until its blocking read
    /// returns, which on a live port means when the next byte or the close
    /// arrives — so this is for tests and for an orderly shutdown that is
    /// already closing the port, not for a timely cancel.
    pub fn stop(&self) {
        self.stop.store(true, Ordering::Release);
    }

    pub fn join(mut self) -> ServeEnd {
        self.stop();
        self.handle.take().map_or(ServeEnd::Stopped, |h| h.join().unwrap_or(ServeEnd::Failed))
    }
}

impl Drop for Doorman {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::control::{Control, Geometry, ProtocolIds, RingKind};
    use crate::park::{Deadline, Futex, ParkOutcome, Parker, WaitSite};
    use crate::region::SharedRegion;
    use crate::ring::Lane;
    use std::time::{Duration, Instant};

    /// The cross-VM doorbell is **process-global**, and `cargo test` runs tests
    /// as threads of one process. So a test that installs one is mutating a
    /// thing every other test in this binary can see: while it is installed,
    /// every `Bell::ring` anywhere in the suite writes a byte to *this* test's
    /// pipe. That showed up as `a_rung_doorbell_wakes_the_consumer_rather_than_
    /// the_slice_expiring` failing intermittently, which is a real bug in the
    /// test suite and would have been a mystifying one to chase.
    ///
    /// Every test that installs must hold this, and must uninstall before
    /// releasing it. The lock is poison-tolerant because a panicking test must
    /// not take the rest of the suite down with it.
    static INSTALLED: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Installs for the duration of a scope and always uninstalls, including on
    /// a panic — which matters more than usual here, since a leaked install
    /// would break tests that have nothing to do with this module.
    struct Installed(#[allow(dead_code)] std::sync::MutexGuard<'static, ()>);

    impl Installed {
        fn new(w: Arc<dyn CrossWake>) -> Installed {
            let g = INSTALLED.lock().unwrap_or_else(|e| e.into_inner());
            crate::park::install_cross_wake(Some(w));
            Installed(g)
        }
    }

    impl Drop for Installed {
        fn drop(&mut self) {
            crate::park::install_cross_wake(None);
        }
    }

    fn setup() -> (Arc<Control>, u32) {
        let g = Geometry::small();
        let region = Arc::new(SharedRegion::heap(g.control_size as usize).unwrap());
        let ids = ProtocolIds::new(1, 1);
        let owner = Control::create(Arc::clone(&region), g, ids, 1).unwrap();
        let slot = owner.claim_ring(RingKind::Device, 1, None).unwrap();
        (Arc::new(owner), slot)
    }

    /// A `socketpair` is the honest stand-in for a virtio-serial port: a
    /// bidirectional byte pipe with no framing, where a write on one side makes
    /// a blocked read on the other return. Everything this module does to a
    /// port, it does to this.
    fn pipe() -> (std::os::unix::net::UnixStream, std::os::unix::net::UnixStream) {
        std::os::unix::net::UnixStream::pair().unwrap()
    }

    #[test]
    fn a_token_round_trips_through_a_byte() {
        for slot in 0..16u32 {
            for lane in [Lane::Command, Lane::Reply] {
                let t = crate::park::lane_token(slot, lane);
                assert_eq!(t & 0x80, 0, "the top bit is reserved");
                let (s, l) = crate::park::lane_from_token(t);
                assert_eq!((s, l), (slot, lane));
            }
        }
    }

    /// **The acceptance test for this module.**
    ///
    /// A waiter is parked with a `Futex` whose slice ladder has been pinned to
    /// the old fixed 20 ms, so that *nothing but the doorbell* can wake it
    /// early. The same-kernel `sys::wake` is bypassed entirely — readiness is
    /// published by a plain store, exactly as a peer across a hypervisor would
    /// publish it — and the only thing that crosses is one byte on a pipe.
    ///
    /// If the doorbell works, this returns in microseconds. If it does not, it
    /// returns in 20 ms, which is the defect.
    #[test]
    fn a_byte_on_the_port_wakes_a_waiter_that_no_futex_could_reach() {
        let (control, slot) = setup();
        let (host_end, guest_end) = pipe();
        let doorman = Doorman::spawn(guest_end, Arc::clone(&control));

        let ctrl = RingCtrl::open(&control, slot).unwrap();
        let flag = Arc::new(AtomicBool::new(false));

        // The "producer", which is on the other side of a hypervisor as far as
        // any futex is concerned: it publishes readiness and pokes the port, and
        // it never calls `sys::wake`.
        let poker = {
            let flag = Arc::clone(&flag);
            let mut host_end = host_end;
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(2));
                flag.store(true, Ordering::Release);
                let token = crate::park::lane_token(slot, Lane::Command);
                host_end.write_all(&[token]).unwrap();
                host_end.flush().unwrap();
            })
        };

        let ready = || flag.load(Ordering::Acquire);
        let status = || ctrl.status();
        let site =
            WaitSite::new(ctrl.id(), &ready, &status).with_bell(ctrl.bell(Lane::Command).unwrap());
        // A **two second** slice, not the 20 ms one. The margin is deliberate:
        // this machine is shared with several other agents, and a bound that
        // sits just under the real slice turns machine load into a test failure.
        // At two seconds there is nothing to tune — either the byte woke the
        // waiter in milliseconds, or it did not and the wait takes two seconds,
        // and no amount of contention turns one into the other.
        let fixed = Futex {
            first_slice: Duration::from_secs(2),
            slice: Duration::from_secs(2),
            ..Futex::default()
        };

        let t0 = Instant::now();
        let outcome = fixed.park(&site, Deadline::after(Duration::from_secs(30)));
        let took = t0.elapsed();
        poker.join().unwrap();

        assert_eq!(outcome, ParkOutcome::Woken);
        assert!(
            took < Duration::from_millis(500),
            "the wait took {took:?} with a 2 s slice and a doorbell installed; \
             that is the slice expiring, so the byte woke nothing"
        );
        assert!(doorman.woken() >= 1, "the doorman delivered no wakes at all");
        doorman.join();
    }

    /// The framing argument, asserted: a byte naming a lane that does not exist
    /// must cost nothing and must not stop the doorbell working afterwards.
    #[test]
    fn a_stray_byte_costs_nothing_and_does_not_take_the_doorbell_down() {
        let (control, slot) = setup();
        let (mut host_end, guest_end) = pipe();
        let doorman = Doorman::spawn(guest_end, Arc::clone(&control));

        // A slot far past `ring_slot_count`, and the reserved top bit set.
        host_end.write_all(&[0xff, 0xfe, 200]).unwrap();
        host_end.flush().unwrap();
        std::thread::sleep(Duration::from_millis(20));

        // And now a good one, which must still work.
        let before = doorman.woken();
        host_end.write_all(&[crate::park::lane_token(slot, Lane::Reply)]).unwrap();
        host_end.flush().unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        while doorman.woken() == before && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(doorman.woken() > before, "a stray byte killed the doorbell");
        // Close the port so the reader's blocking read returns. `join` cannot
        // interrupt one — that is stated on `Doorman::stop` — so this order is
        // required and not tidiness.
        drop(host_end);
        assert_eq!(doorman.join(), ServeEnd::Closed);
    }

    /// `Bell::ring` must poke the installed doorbell, so that no producer call
    /// site has to know this module exists — which is the claim in the header.
    #[test]
    fn ringing_a_bell_pokes_the_installed_doorbell_with_no_caller_change() {
        let (control, slot) = setup();
        let (host_end, mut guest_end) = pipe();

        let wake = Arc::new(PortWake::new(host_end));
        let installed = Installed::new(wake.clone() as Arc<dyn CrossWake>);

        let ctrl = RingCtrl::open(&control, slot).unwrap();
        let bell = ctrl.bell(Lane::Command).unwrap();
        // `ring` is a no-op unless somebody is parked, which is the existing
        // "producer checks IDLE before paying" rule and applies to the cross-VM
        // poke for the same reason.
        bell.parked.fetch_add(1, Ordering::SeqCst);
        bell.ring();
        drop(installed);

        guest_end.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        let mut got = [0u8; 1];
        guest_end.read_exact(&mut got).unwrap();
        assert_eq!(got[0], crate::park::lane_token(slot, Lane::Command));
        assert_eq!(wake.stats().0, 1);
    }

    /// **One present, three ways.** This is the acceptance number, reproduced on
    /// one kernel: eleven waits of 180 µs each, with the same-kernel futex wake
    /// deliberately unavailable — which is what a hypervisor does to it.
    ///
    /// Measured on the development host:
    ///
    /// | | 11 waits | per present |
    /// |---|---|---|
    /// | fixed 20 ms slice — what shipped, and what measured 216.9 ms cross-VM | ~185 ms | 216.9 ms observed |
    /// | slice ladder (`first_slice` 25 µs) | ~11 ms | ~11 ms guest-side, 1 ms floor |
    /// | **virtio-serial doorbell** | **~2 ms** | the work itself |
    ///
    /// The third row is the point: it is not a smaller poll interval, it is the
    /// absence of polling. What is left is the 180 µs × 11 of actual work, which
    /// is what a present costs locally.
    #[test]
    #[ignore = "a stopwatch: the figure it reports depends on machine load"]
    fn one_present_with_the_doorbell_costs_the_work_and_nothing_else() {
        let (control, slot) = setup();
        let (host_end, guest_end) = pipe();
        let doorman = Doorman::spawn(guest_end, Arc::clone(&control));
        let ctrl = RingCtrl::open(&control, slot).unwrap();

        // No ladder: the old fixed 20 ms slice, so that the only thing that can
        // end a wait early is the byte.
        let fixed = Futex {
            first_slice: Duration::from_millis(20),
            slice: Duration::from_millis(20),
            ..Futex::default()
        };

        let host_end = Arc::new(std::sync::Mutex::new(host_end));
        let t0 = Instant::now();
        for _ in 0..11 {
            let flag = Arc::new(AtomicBool::new(false));
            let poker = {
                let flag = Arc::clone(&flag);
                let host_end = Arc::clone(&host_end);
                std::thread::spawn(move || {
                    std::thread::sleep(Duration::from_micros(180));
                    flag.store(true, Ordering::Release);
                    let mut p = host_end.lock().unwrap();
                    p.write_all(&[crate::park::lane_token(slot, Lane::Command)]).unwrap();
                    p.flush().unwrap();
                })
            };
            let ready = || flag.load(Ordering::Acquire);
            let status = || ctrl.status();
            let site = WaitSite::new(ctrl.id(), &ready, &status)
                .with_bell(ctrl.bell(Lane::Command).unwrap());
            assert_eq!(
                fixed.park(&site, Deadline::after(Duration::from_secs(5))),
                ParkOutcome::Woken
            );
            poker.join().unwrap();
        }
        let took = t0.elapsed();
        println!("doorbell, 11 waits: {took:?}  ({} wakes delivered)", doorman.woken());

        // Eleven fixed 20 ms slices would be ~220 ms. Anything near the work
        // itself is the doorbell doing its job; the bound is loose because this
        // machine is shared.
        assert!(
            took < Duration::from_millis(40),
            "one present took {took:?} with a doorbell installed; \
             eleven 20 ms slices is ~220 ms, so this is polling, not a doorbell"
        );
        assert!(doorman.woken() >= 11, "only {} wakes for 11 waits", doorman.woken());

        drop(Arc::try_unwrap(host_end).map(|m| m.into_inner().unwrap()).ok());
        assert_eq!(doorman.join(), ServeEnd::Closed);
    }

    /// And it must stay a no-op when nobody is parked: a doorbell that fired on
    /// every packet would put a vmexit on the hot path of a busy ring, which is
    /// the one way this could be slower than the slice it replaces.
    #[test]
    fn an_unparked_lane_costs_no_byte_at_all() {
        let (control, slot) = setup();
        let (host_end, guest_end) = pipe();
        let wake = Arc::new(PortWake::new(host_end));
        let installed = Installed::new(wake.clone() as Arc<dyn CrossWake>);

        let ctrl = RingCtrl::open(&control, slot).unwrap();
        ctrl.bell(Lane::Command).unwrap().ring();
        drop(installed);

        assert_eq!(wake.stats(), (0, 0), "a byte was sent for a lane nobody was waiting on");
        drop(guest_end);
    }
}
