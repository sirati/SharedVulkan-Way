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
pub fn serve_port<R: Read, T: BellTarget + ?Sized>(
    mut port: R,
    target: &T,
    stop: &AtomicBool,
    woken: &AtomicU64,
) -> ServeEnd {
    let mut buf = [0u8; 64];
    // THE RESCAN RULE (§103.15). This port has just been opened, or opened
    // again: any byte sent while it was closed is gone (Linux's virtio_console
    // drops what arrives while nobody has the port open; a hub's socket drops
    // what arrives while it restarts). So no waiter may trust that it saw
    // every wake: each is woken once now and looks at its ring.
    target.rescan(woken);
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
            // The Windows guest end of a virtio-serial port reports "host not
            // attached" as `WouldBlock` (winpipe maps ERROR_NO_SYSTEM_RESOURCES).
            // A doorbell whose peer has not connected yet must wait, not exit.
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(std::time::Duration::from_millis(1));
                continue;
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => return ServeEnd::Failed,
        };
        // Several bytes may arrive in one read when the peer rang more than one
        // lane before this thread was scheduled. Each is independent.
        for &token in &buf[..n] {
            if token == crate::park::RESCAN_TOKEN {
                target.rescan(woken);
            } else {
                target.ring_token(token, woken);
            }
        }
    }
}

/// Where a doorbell byte is delivered: one session's control region, or every
/// live session's (waycompose-design.md §87).
///
/// A virtio-serial port takes one opener, so a guest process serving several
/// sessions has one port for all of them, and a byte carries a slot and a lane
/// but not a session. Ringing that lane in **every** session is correct by this
/// module's own framing argument: the session the byte was meant for wakes, and
/// in every other one it is a spurious wake on a lane that then finds its ring
/// empty, which costs one syscall.
pub trait BellTarget: Send + Sync {
    /// Ring the lane `token` names, counting each delivery in `woken` before
    /// making it. Returns whether anything was rung.
    fn ring_token(&self, token: u8, woken: &AtomicU64) -> bool;

    /// Wake every waiter this target serves, so each looks at its ring again
    /// (§103.15). Not counted in `woken`, which counts bytes delivered. Called by [`serve_port`] whenever its port was (re)opened and
    /// on [`crate::park::RESCAN_TOKEN`]: a wake may have been lost, and a
    /// waiter parked with no slice would wait for ever. Required, not
    /// defaulted, so no target can forget it.
    fn rescan(&self, woken: &AtomicU64);
}

impl BellTarget for Control {
    fn rescan(&self, _woken: &AtomicU64) {
        // Not counted in `woken`, which counts bytes delivered.
        for slot in 0..self.layout().geometry.ring_slot_count {
            for lane in [crate::ring::Lane::Command, crate::ring::Lane::Reply] {
                if let Ok(ctrl) = RingCtrl::open(self, slot)
                    && let Ok(bell) = ctrl.bell(lane)
                {
                    bell.ring_local();
                }
            }
        }
    }

    fn ring_token(&self, token: u8, woken: &AtomicU64) -> bool {
        {
            let (slot, lane) = crate::park::lane_from_token(token);
            // Every failure here is a *bad token*, and the right response to one
            // is to ignore it: a slot out of range or a ring that is not mapped
            // is a stale or corrupt byte, and this module's framing argument is
            // that such a byte must cost nothing. Erroring out would let one
            // stray byte take the doorbell down for the whole session.
            let Ok(ctrl) = RingCtrl::open(self, slot) else { return false };
            let Ok(bell) = ctrl.bell(lane) else { return false };
            // COUNT BEFORE RINGING, and the order is the whole of it.
            //
            // `ring_local` is what makes the wake observable: the waiter it
            // releases can be scheduled, return from `park`, and read
            // `Doorman::woken()` before this thread executes the next
            // statement. With the increment *after* the ring, the counter is
            // therefore allowed to read 0 in a waiter that has already been
            // woken by it — which is not a doorbell defect but is
            // indistinguishable from one at the assertion.
            //
            // Measured rather than reasoned about:
            // `a_byte_on_the_port_wakes_a_waiter_that_no_futex_could_reach`
            // passed 5 times out of 5 run alone and failed on both full
            // `cargo test --workspace` runs, on `the doorman delivered no wakes
            // at all` — while the two assertions *before* it, that the outcome
            // was `Woken` and that it took under 500 ms, both passed. The wake
            // plainly happened; only the count had not landed yet. Parallel
            // load widens the window between these two lines, which is why the
            // whole-workspace run is where it shows.
            //
            // Incrementing first is accurate: this thread has committed to
            // delivering the wake and nothing between the two statements can
            // fail. §35's shape — the failure mode of the *check*, not of the
            // thing checked, and it made a shared gate red for everyone.
            woken.fetch_add(1, Ordering::Relaxed);
            bell.ring_local();
            true
        }
    }

}

/// Every live session's control region, for a process that serves several
/// over one port (§87). Sessions join with [`ControlSet::add`] and leave by
/// being dropped: the set holds them weakly, so a session that has ended is
/// not kept alive by its doorbell, and is pruned at the next byte.
#[derive(Default)]
pub struct ControlSet {
    members: std::sync::Mutex<Vec<std::sync::Weak<Control>>>,
}

impl ControlSet {
    pub fn new() -> Arc<ControlSet> {
        Arc::new(ControlSet::default())
    }

    pub fn add(&self, control: &Arc<Control>) {
        if let Ok(mut m) = self.members.lock() {
            m.retain(|w| w.strong_count() > 0);
            m.push(Arc::downgrade(control));
        }
    }

    /// Sessions currently in the set, after pruning the ended ones.
    pub fn len(&self) -> usize {
        self.members.lock().map_or(0, |mut m| {
            m.retain(|w| w.strong_count() > 0);
            m.len()
        })
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl BellTarget for ControlSet {
    fn rescan(&self, woken: &AtomicU64) {
        let live: Vec<Arc<Control>> = match self.members.lock() {
            Ok(m) => m.iter().filter_map(std::sync::Weak::upgrade).collect(),
            Err(_) => return,
        };
        for c in &live {
            c.rescan(woken);
        }
    }

    fn ring_token(&self, token: u8, woken: &AtomicU64) -> bool {
        // Upgrade under the lock, ring outside it: `ring_local` is a syscall,
        // and a session joining should not wait behind one.
        let live: Vec<Arc<Control>> = match self.members.lock() {
            Ok(mut m) => {
                m.retain(|w| w.strong_count() > 0);
                m.iter().filter_map(std::sync::Weak::upgrade).collect()
            }
            Err(_) => return false,
        };
        let mut any = false;
        for c in &live {
            any |= c.ring_token(token, woken);
        }
        any
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
        Doorman::spawn_target(port, control)
    }

    /// Start answering the door for any [`BellTarget`] — one control region,
    /// or a [`ControlSet`] of every session a process serves (§87).
    pub fn spawn_target<R: Read + Send + 'static, T: BellTarget + ?Sized + 'static>(
        port: R,
        control: Arc<T>,
    ) -> Doorman {
        let stop = Arc::new(AtomicBool::new(false));
        let woken = Arc::new(AtomicU64::new(0));
        let handle = {
            let stop = Arc::clone(&stop);
            let woken = Arc::clone(&woken);
            std::thread::Builder::new()
                .name("sv-doorbell".into())
                .spawn(move || serve_port(port, &*control, &stop, &woken))
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

/// Wire both halves of the doorbell onto one already-open port, and install it.
///
/// This is the whole bring-up, and it is one call because getting it wrong in
/// two places was the likely alternative: the writer must be installed as the
/// process's [`CrossWake`] *and* a reader thread must be answering, or the
/// doorbell is half-built in a way that presents as "no faster than before"
/// rather than as an error.
///
/// # One port, two threads, and why that is not the deadlock the docs warn about
///
/// [`crate::stream::open_virtio_port`] warns that **two descriptors** on one
/// port deadlock — the driver serialises them, and a reader blocked in one stops
/// the other making progress. That is about two `open()` calls. This takes one
/// open handle and `try_clone`s it, which dups the descriptor and shares the
/// *same open file description*: one reader thread and one writer thread on one
/// description is ordinary, and it is what every socket in this tree already
/// does.
///
/// The distinction matters enough to state, because "do not use the port from
/// two threads" would be the wrong lesson to take from that warning and would
/// make a doorbell impossible — a doorbell is by construction a thing you write
/// while something else is blocked reading.
pub fn attach(port: PortHandle, control: Arc<Control>) -> std::io::Result<Doorman> {
    let (reader, writer) = port.split()?;
    crate::park::install_cross_wake(Some(Arc::new(PortWake::new(writer)) as Arc<dyn CrossWake>));
    Ok(Doorman::spawn(reader, control))
}

/// [`attach`] for a process that serves several sessions over one port: the
/// writer is installed exactly as there, and the reader rings every session in
/// `set` (§87).
pub fn attach_set(port: PortHandle, set: Arc<ControlSet>) -> std::io::Result<Doorman> {
    let (reader, writer) = port.split()?;
    crate::park::install_cross_wake(Some(Arc::new(PortWake::new(writer)) as Arc<dyn CrossWake>));
    Ok(Doorman::spawn_target(reader, set))
}


/// A writer whose socket can be swapped while it is installed as the
/// process's [`CrossWake`]: empty while the hub is unreachable, in which case
/// a write is `NotConnected` (counted as dropped by [`PortWake::poke`]).
#[cfg(unix)]
#[derive(Clone, Default)]
pub struct SwapWriter(Arc<std::sync::Mutex<Option<Box<dyn Write + Send>>>>);

#[cfg(unix)]
impl SwapWriter {
    fn set(&self, w: Option<Box<dyn Write + Send>>) {
        if let Ok(mut g) = self.0.lock() {
            *g = w;
        }
    }
}

#[cfg(unix)]
impl Write for SwapWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        match self.0.lock().ok().as_mut().and_then(|g| g.as_mut()) {
            Some(w) => w.write(buf),
            None => Err(std::io::ErrorKind::NotConnected.into()),
        }
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// How often a lost hub socket is tried again.
#[cfg(unix)]
pub const RECONNECT_EVERY: std::time::Duration = std::time::Duration::from_millis(500);

/// [`attach`] for a host-side client of the shared doorbell hub
/// (waycompose-design.md §99), **reconnecting**: when the hub's socket closes
/// (the hub was restarted to install a new binary, or crashed and systemd
/// started it again) the doorman connects again by itself, every
/// [`RECONNECT_EVERY`], and swaps the new socket under the installed writer.
/// Between the two, pokes are dropped and waits run out their slices; nothing
/// fails and nothing waits for the hub. Also used when the hub is not there
/// yet at bring-up. Returns whether the first connect succeeded, for the log.
#[cfg(unix)]
pub fn attach_host_reconnecting<T: BellTarget + ?Sized + 'static>(
    path: &str,
    target: Arc<T>,
) -> std::io::Result<(Doorman, bool)> {
    let (d, ok, wake) = connect_host_reconnecting(path, target)?;
    crate::park::install_cross_wake(Some(wake as Arc<dyn CrossWake>));
    Ok((d, ok))
}

/// [`attach_host_reconnecting`] without installing the writer: the caller
/// gets the wake to install or poke itself.
#[cfg(unix)]
pub fn connect_host_reconnecting<T: BellTarget + ?Sized + 'static>(
    path: &str,
    target: Arc<T>,
) -> std::io::Result<(Doorman, bool, Arc<PortWake<SwapWriter>>)> {
    let writer = SwapWriter::default();
    let wake = Arc::new(PortWake::new(writer.clone()));
    let first = PortHandle::host_socket(path).ok();
    let connected = first.is_some();
    let stop = Arc::new(AtomicBool::new(false));
    let woken = Arc::new(AtomicU64::new(0));
    let path = path.to_string();
    let handle = {
        let stop = Arc::clone(&stop);
        let woken = Arc::clone(&woken);
        std::thread::Builder::new().name("sv-doorbell".into()).spawn(move || {
            let mut next = first;
            let mut reconnects = 0u64;
            loop {
                if stop.load(Ordering::Acquire) {
                    writer.set(None);
                    return ServeEnd::Stopped;
                }
                let port = match next.take() {
                    Some(p) => p,
                    None => match PortHandle::host_socket(&path) {
                        Ok(p) => {
                            reconnects += 1;
                            eprintln!("[doorbell] reconnected to {path} (reconnect {reconnects})");
                            p
                        }
                        Err(_) => {
                            std::thread::sleep(RECONNECT_EVERY);
                            continue;
                        }
                    },
                };
                let Ok((reader, w)) = port.split() else {
                    std::thread::sleep(RECONNECT_EVERY);
                    continue;
                };
                writer.set(Some(w));
                let end = serve_port(reader, &*target, &stop, &woken);
                writer.set(None);
                if end == ServeEnd::Stopped {
                    return end;
                }
                eprintln!("[doorbell] {path} closed ({end:?}); waits use their slices until it is back");
                std::thread::sleep(RECONNECT_EVERY);
            }
        })?
    };
    Ok((Doorman { stop, woken, handle: Some(handle) }, connected, wake))
}

/// The host's doorbell writer since §99.9: each byte is sent with
/// `MSG_DONTWAIT`, so a full socket is `WouldBlock` (counted as dropped by
/// [`PortWake::poke`]) and the call never waits, without a thread in between.
/// Per call, not `O_NONBLOCK`: that flag belongs to the open file
/// description, which the doorbell's reader shares through `try_clone`, and
/// would turn its blocking read into a poll.
#[cfg(target_os = "linux")]
pub struct DontWaitWriter(pub std::os::unix::net::UnixStream);

#[cfg(target_os = "linux")]
mod dontwait_sys {
    unsafe extern "C" {
        pub fn send(fd: i32, buf: *const u8, len: usize, flags: i32) -> isize;
    }
    pub const MSG_DONTWAIT: i32 = 0x40;
    pub const MSG_NOSIGNAL: i32 = 0x4000;
}

#[cfg(target_os = "linux")]
impl Write for DontWaitWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        use std::os::fd::AsRawFd;
        // SAFETY: `buf` is valid for `buf.len()` bytes and the descriptor is
        // owned by `self.0` for the duration of the call.
        let n = unsafe {
            dontwait_sys::send(
                self.0.as_raw_fd(),
                buf.as_ptr(),
                buf.len(),
                dontwait_sys::MSG_DONTWAIT | dontwait_sys::MSG_NOSIGNAL,
            )
        };
        if n < 0 { Err(std::io::Error::last_os_error()) } else { Ok(n as usize) }
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// A `Write` that hands each write to a thread of its own and never blocks.
///
/// A doorbell byte means "look at your rings", and a lost one costs a wait its
/// slice; a blocked writer costs the application every call it is making. So
/// a write is queued (at most [`DetachedWriter::QUEUE`] at once) and a full
/// queue is `WouldBlock`, which [`PortWake::poke`] counts as dropped.
#[cfg(any(unix, windows))]
pub struct DetachedWriter {
    tx: std::sync::mpsc::SyncSender<Vec<u8>>,
}

#[cfg(any(unix, windows))]
impl DetachedWriter {
    /// Bytes in flight before a write is refused.
    pub const QUEUE: usize = 256;

    pub fn spawn<W: Write + Send + 'static>(mut w: W) -> std::io::Result<DetachedWriter> {
        let (tx, rx) = std::sync::mpsc::sync_channel::<Vec<u8>>(Self::QUEUE);
        std::thread::Builder::new().name("sv-bell-write".into()).spawn(move || {
            // Ends when every sender is gone, or the socket is.
            while let Ok(b) = rx.recv() {
                if w.write_all(&b).and_then(|()| w.flush()).is_err() {
                    break;
                }
            }
        })?;
        Ok(DetachedWriter { tx })
    }
}

#[cfg(any(unix, windows))]
impl Write for DetachedWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        match self.tx.try_send(buf.to_vec()) {
            Ok(()) => Ok(buf.len()),
            Err(std::sync::mpsc::TrySendError::Full(_)) => Err(std::io::ErrorKind::WouldBlock.into()),
            Err(std::sync::mpsc::TrySendError::Disconnected(_)) => Err(std::io::ErrorKind::BrokenPipe.into()),
        }
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// A port that can be split into a reader and a writer over one description.
///
/// Three shapes, because the port is a different type on each side and the
/// bring-up should not care: a `File` on the Linux guest, a `UnixStream` on the
/// host (the chardev's socket end), and a `Pipe` on the Windows guest.
pub enum PortHandle {
    /// `/dev/virtio-ports/<name>`, from [`crate::stream::open_virtio_port`].
    #[cfg(unix)]
    File(std::fs::File),
    /// The host end of the QEMU `-chardev socket`.
    #[cfg(unix)]
    Unix(std::os::unix::net::UnixStream),
    /// `\\.\Global\<name>`, from [`crate::winpipe::open_virtio_port`].
    #[cfg(windows)]
    Pipe(std::sync::Arc<crate::winpipe::Pipe>),
    /// Two inherited descriptors, a reader and a writer (§103.6): a port a
    /// parent process opened, such as a libkrun virtio-console port's host
    /// end on a Mac running a Linux VM. One descriptor for both is
    /// [`PortHandle::File`].
    #[cfg(unix)]
    Fds(std::fs::File, std::fs::File),
}

impl PortHandle {
    /// Open a doorbell endpoint from one spelling (§103.6), so a caller's
    /// `--bell` takes every kind:
    ///
    /// * `fd:R,W` or `fd:N`: descriptors this process inherited (Unix);
    /// * `socket:PATH`: the host end of a QEMU chardev socket (Unix);
    /// * `dev:NAME`: the guest's port by name ([`PortHandle::guest_port`]:
    ///   `/dev/virtio-ports/NAME` on Linux, `/dev/NAME` on macOS,
    ///   `\\.\Global\NAME` on Windows);
    /// * anything else: a socket if it contains `/`, a guest port if not,
    ///   which is `wc_compositor::ringlink::open_bell_port`'s convention.
    pub fn open(spec: &str) -> std::io::Result<PortHandle> {
        if let Some(fds) = spec.strip_prefix("fd:") {
            #[cfg(unix)]
            return PortHandle::from_fd_spec(fds);
            #[cfg(not(unix))]
            return Err(std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                format!("fd:{fds}: inherited descriptors are a Unix doorbell"),
            ));
        }
        if let Some(path) = spec.strip_prefix("socket:") {
            #[cfg(unix)]
            return PortHandle::host_socket(path);
            #[cfg(not(unix))]
            return Err(std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                format!("socket:{path}: a chardev socket is a Unix host's"),
            ));
        }
        if let Some(name) = spec.strip_prefix("dev:") {
            return PortHandle::open_guest(name);
        }
        #[cfg(unix)]
        if spec.contains('/') {
            return PortHandle::host_socket(spec);
        }
        PortHandle::open_guest(spec)
    }

    #[cfg(any(target_os = "linux", target_os = "macos", windows))]
    fn open_guest(name: &str) -> std::io::Result<PortHandle> {
        PortHandle::guest_port(name)
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
    fn open_guest(name: &str) -> std::io::Result<PortHandle> {
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            format!("{name}: no guest port on this system"),
        ))
    }

    /// `R,W` or `N`: take ownership of inherited descriptors.
    #[cfg(unix)]
    fn from_fd_spec(fds: &str) -> std::io::Result<PortHandle> {
        use std::os::fd::{FromRawFd, OwnedFd};
        let bad = || std::io::Error::new(std::io::ErrorKind::InvalidInput, format!("fd:{fds}: expected fd:N or fd:R,W"));
        let parse = |s: &str| -> std::io::Result<i32> { s.trim().parse::<i32>().ok().filter(|n| *n >= 0).ok_or_else(bad) };
        let (r, w) = match fds.split_once(',') {
            Some((r, w)) => (parse(r)?, parse(w)?),
            None => {
                let n = parse(fds)?;
                (n, n)
            }
        };
        // An fd number that is not open is refused here rather than adopted:
        // `from_raw_fd` on a closed number would later close whatever the
        // process opened under it.
        for n in [r, w] {
            if std::fs::metadata(format!("/dev/fd/{n}")).is_err() {
                return Err(std::io::Error::new(std::io::ErrorKind::NotFound, format!("fd:{fds}: descriptor {n} is not open")));
            }
        }
        // SAFETY: the spec names descriptors this process inherited for the
        // doorbell and uses for nothing else; they were just checked to be
        // open, and each is adopted exactly once (a shared `N` is adopted once
        // and cloned).
        let rf = std::fs::File::from(unsafe { OwnedFd::from_raw_fd(r) });
        if r == w {
            return Ok(PortHandle::File(rf));
        }
        let wf = std::fs::File::from(unsafe { OwnedFd::from_raw_fd(w) });
        Ok(PortHandle::Fds(rf, wf))
    }

    /// Open the guest end by port name. Linux guest.
    #[cfg(target_os = "linux")]
    pub fn guest_port(name: &str) -> std::io::Result<PortHandle> {
        crate::stream::open_virtio_port(name).map(PortHandle::File)
    }

    /// Open the guest end by device name. macOS guest (§103.4).
    ///
    /// macOS has no `/dev/virtio-ports`; a doorbell there is a serial device
    /// (`AppleVirtIOConsole`'s or a 16550's `cu.*` node), named relative to
    /// `/dev` because a name containing `/` is the *host* end's socket path in
    /// `wc_compositor::ringlink::open_bell_port`. Raw mode, so the byte-per-bell
    /// protocol is not line-disciplined into silence.
    #[cfg(target_os = "macos")]
    pub fn guest_port(name: &str) -> std::io::Result<PortHandle> {
        use std::os::fd::AsRawFd;
        let path = format!("/dev/{name}");
        let f = std::fs::OpenOptions::new().read(true).write(true).open(&path).map_err(|e| {
            std::io::Error::new(e.kind(), format!("{path}: {e} — the guest has no such serial device"))
        })?;
        // `struct termios` is 72 bytes on x86_64 macOS (and arm64); an opaque,
        // larger, aligned buffer is all three calls need, and keeps this crate
        // free of a libc dependency, which is its convention.
        unsafe extern "C" {
            fn tcgetattr(fd: i32, t: *mut u64) -> i32;
            fn cfmakeraw(t: *mut u64);
            fn tcsetattr(fd: i32, action: i32, t: *const u64) -> i32;
        }
        let mut t = [0u64; 16];
        // SAFETY: `t` is larger than `struct termios` and 8-aligned; the
        // descriptor is open for the whole call. TCSANOW is 0.
        unsafe {
            if tcgetattr(f.as_raw_fd(), t.as_mut_ptr()) == 0 {
                cfmakeraw(t.as_mut_ptr());
                tcsetattr(f.as_raw_fd(), 0, t.as_ptr());
            }
        }
        Ok(PortHandle::File(f))
    }

    /// Open the guest end by port name. Windows guest.
    ///
    /// Ten seconds, because on a *hot-added* port the device object appears when
    /// `vioser.sys` binds it, which is some milliseconds after the QMP
    /// `device_add` returns — and a guest started by a harness may well race it.
    #[cfg(windows)]
    pub fn guest_port(name: &str) -> std::io::Result<PortHandle> {
        crate::winpipe::open_virtio_port(name, 10_000)
            .map(|p| PortHandle::Pipe(std::sync::Arc::new(p)))
            .map_err(|e| std::io::Error::other(format!("{e}")))
    }

    /// Connect to the host end of the chardev.
    #[cfg(unix)]
    pub fn host_socket(path: &str) -> std::io::Result<PortHandle> {
        std::os::unix::net::UnixStream::connect(path).map(PortHandle::Unix)
    }

    /// One reader and one writer over the one open (see [`attach`]). Public
    /// since waycompose-design.md §89, whose compositor wakes its own loop
    /// with the reader rather than a ring's futex.
    pub fn split(self) -> std::io::Result<(Box<dyn Read + Send>, Box<dyn Write + Send>)> {
        Ok(match self {
            #[cfg(unix)]
            PortHandle::File(f) => {
                let w = f.try_clone()?;
                (Box::new(f), Box::new(w))
            }
            #[cfg(unix)]
            PortHandle::Fds(r, w) => (Box::new(r), Box::new(w)),
            // The writer never blocks its caller (waycompose-design.md §97.10):
            // a hub that stops reading — on the guest, a stale session's
            // `sv-hostd` held QEMU's one connection and the next hub's sat
            // unaccepted — filled this socket, and Firefox's renderer thread
            // then sat in `send()` inside `vkCreateGraphicsPipelines` until
            // every call it was making had timed out.
            #[cfg(unix)]
            PortHandle::Unix(s) => {
                let w = s.try_clone()?;
                #[cfg(target_os = "linux")]
                let w: Box<dyn Write + Send> = Box::new(DontWaitWriter(w));
                #[cfg(not(target_os = "linux"))]
                let w: Box<dyn Write + Send> = Box::new(DetachedWriter::spawn(w)?);
                (Box::new(s), w)
            }
            // One handle, shared, rather than a `DuplicateHandle`: a
            // virtio-serial port permits one opener and the point of the `Arc`
            // is that this is still *one* open, used by a reader thread and a
            // writer thread. `ReadFile` and `WriteFile` on one handle from two
            // threads are independent operations on a byte-mode device.
            #[cfg(windows)]
            // The writer is detached for the reason the host's is (§97.10, and
            // §99.5 on the guest): a port write that does not complete must
            // never stop a ring thread, and with it every session this process
            // serves. A session killed mid-run stopped the guest's GPU process
            // for every other session exactly so.
            PortHandle::Pipe(p) => (Box::new(SharedPipe(p.clone())), Box::new(DetachedWriter::spawn(SharedPipe(p))?)),
        })
    }
}

/// A `Pipe` two threads may use at once, one reading and one writing.
#[cfg(windows)]
struct SharedPipe(std::sync::Arc<crate::winpipe::Pipe>);

#[cfg(windows)]
impl Read for SharedPipe {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        (&*self.0).read(buf)
    }
}

#[cfg(windows)]
impl Write for SharedPipe {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        (&*self.0).write(buf)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        (&*self.0).flush()
    }
}

// Unix-only, and the reason is the stand-in rather than the mechanism: these
// tests use a `socketpair` where the real thing is a virtio-serial port, and a
// socketpair is a Unix construct. The Windows *port* path is exercised on the
// deployment guest (§36.16); what has no Windows form is this fixture. The
// module under test — `PortWake`, `Doorman`, `serve_port` — is built on both.
#[cfg(unix)]
#[cfg(test)]
mod tests {
    use super::*;
    use crate::control::{Control, Geometry, ProtocolIds, RingKind};
    use crate::park::{Deadline, Futex, ParkOutcome, Parker, WaitSite};
    use crate::region::SharedRegion;
    use crate::ring::Lane;
    use std::time::{Duration, Instant};

    /// §103.6: an inherited descriptor pair is a doorbell like any other port,
    /// and a closed descriptor number is refused rather than adopted.
    #[cfg(unix)]
    #[test]
    fn an_fd_pair_spec_is_a_port() {
        use std::io::{Read, Write};
        use std::os::fd::IntoRawFd;
        let (ours_r, theirs_w) = std::os::unix::net::UnixStream::pair().unwrap();
        let (theirs_r, ours_w) = std::os::unix::net::UnixStream::pair().unwrap();
        let (r, w) = (ours_r.into_raw_fd(), ours_w.into_raw_fd());
        let (mut reader, mut writer) = PortHandle::open(&format!("fd:{r},{w}")).unwrap().split().unwrap();
        writer.write_all(&[7]).unwrap();
        let mut b = [0u8; 1];
        (&theirs_r).read_exact(&mut b).unwrap();
        assert_eq!(b[0], 7);
        (&theirs_w).write_all(&[9]).unwrap();
        reader.read_exact(&mut b).unwrap();
        assert_eq!(b[0], 9);
        assert!(PortHandle::open("fd:999999").is_err());
        assert!(PortHandle::open("fd:x").is_err());
    }

    /// §99: a hub restart is invisible to a client but for the bells in the
    /// gap. The hub's socket closes and comes back on the same path; the
    /// client reconnects by itself, its pokes reach the new hub, and bytes
    /// from the new hub ring its target.
    #[cfg(unix)]
    #[test]
    fn a_client_reconnects_to_a_restarted_hub() {
        use std::os::unix::net::UnixListener;
        struct Count(AtomicU64);
        impl BellTarget for Count {
            fn ring_token(&self, _t: u8, _w: &AtomicU64) -> bool {
                self.0.fetch_add(1, Ordering::Relaxed);
                true
            }
            fn rescan(&self, _w: &AtomicU64) {}
        }
        let dir = std::env::temp_dir().join(format!("sv-reconnect-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("hub.sock");
        let target = Arc::new(Count(AtomicU64::new(0)));
        let l1 = UnixListener::bind(&path).unwrap();
        let (_doorman, first, wake) = connect_host_reconnecting(path.to_str().unwrap(), Arc::clone(&target)).unwrap();
        assert!(first);
        let (c1, _) = l1.accept().unwrap();
        // The hub "restarts": its socket closes and the path is bound again.
        drop(c1);
        drop(l1);
        std::fs::remove_file(&path).unwrap();
        std::thread::sleep(Duration::from_millis(100));
        let l2 = UnixListener::bind(&path).unwrap();
        l2.set_nonblocking(false).unwrap();
        let (mut c2, _) = l2.accept().unwrap();
        // Up: a poke reaches the new hub.
        let t = Instant::now();
        let mut got = [0u8; 1];
        c2.set_read_timeout(Some(Duration::from_millis(100))).unwrap();
        let mut up = false;
        while !up && t.elapsed() < Duration::from_secs(5) {
            wake.poke(5);
            up = std::io::Read::read(&mut c2, &mut got).map(|n| n == 1).unwrap_or(false);
        }
        assert!(up, "a poke after the restart did not reach the new hub");
        // Down: a byte from the new hub rings the target.
        std::io::Write::write_all(&mut c2, &[3]).unwrap();
        let t = Instant::now();
        while target.0.load(Ordering::Relaxed) == 0 && t.elapsed() < Duration::from_secs(5) {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(target.0.load(Ordering::Relaxed) > 0, "the new hub's byte rang nothing");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// §103.15, the rescan rule: every (re)open of the port, and every
    /// RESCAN_TOKEN, makes the target wake all its waiters, because a wake
    /// sent while the port was closed is lost. A connection, a restart of the
    /// hub, and the hub's token are each one rescan; lane bytes are not.
    #[cfg(unix)]
    #[test]
    fn every_reopen_and_every_rescan_token_rescans() {
        use std::os::unix::net::UnixListener;
        #[derive(Default)]
        struct Seen {
            rings: AtomicU64,
            rescans: AtomicU64,
        }
        impl BellTarget for Seen {
            fn ring_token(&self, _t: u8, _w: &AtomicU64) -> bool {
                self.rings.fetch_add(1, Ordering::Relaxed);
                true
            }
            fn rescan(&self, _w: &AtomicU64) {
                self.rescans.fetch_add(1, Ordering::Relaxed);
            }
        }
        let wait_for = |f: &dyn Fn() -> bool| {
            let t = Instant::now();
            while !f() && t.elapsed() < Duration::from_secs(5) {
                std::thread::sleep(Duration::from_millis(5));
            }
            f()
        };
        let dir = std::env::temp_dir().join(format!("sv-rescan-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("hub.sock");
        let t = Arc::new(Seen::default());
        let l1 = UnixListener::bind(&path).unwrap();
        let (_d, first, _wake) = connect_host_reconnecting(path.to_str().unwrap(), Arc::clone(&t)).unwrap();
        assert!(first);
        let (mut c1, _) = l1.accept().unwrap();
        assert!(wait_for(&|| t.rescans.load(Ordering::Relaxed) == 1), "the first open did not rescan");
        // Lane bytes ring; the token rescans.
        std::io::Write::write_all(&mut c1, &[3, crate::park::RESCAN_TOKEN, 4]).unwrap();
        assert!(wait_for(&|| t.rings.load(Ordering::Relaxed) == 2 && t.rescans.load(Ordering::Relaxed) == 2));
        // A restart: the reopen rescans again, before any byte arrives.
        drop(c1);
        drop(l1);
        std::fs::remove_file(&path).unwrap();
        let l2 = UnixListener::bind(&path).unwrap();
        let (_c2, _) = l2.accept().unwrap();
        assert!(wait_for(&|| t.rescans.load(Ordering::Relaxed) == 3), "the reopen did not rescan");
        assert_eq!(t.rings.load(Ordering::Relaxed), 2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The rescan wakes a real waiter: a reader parked on a lane whose wake
    /// was lost (published with no bell) returns when the target rescans.
    #[test]
    fn a_rescan_ends_a_wait_whose_wake_was_lost() {
        use crate::control::{Geometry, ProtocolIds};
        let geometry = Geometry::for_wayland();
        let mut mem = vec![0u64; geometry.control_size as usize / 8];
        // SAFETY: a zeroed, 8-aligned buffer of the geometry's size that
        // outlives every use below.
        let region = Arc::new(unsafe { crate::region::SharedRegion::from_raw(mem.as_mut_ptr().cast(), geometry.control_size as usize) });
        let control = Arc::new(Control::create(region, geometry, ProtocolIds::new(1, 1), 1).unwrap());
        let ch = crate::wlbytes::ByteChannel::claim(&control, 7).unwrap();
        let bell = ch.rx_bell().unwrap();
        let seen = bell.wake.load(Ordering::Acquire);
        use std::sync::atomic::AtomicU32;
        let waiter = {
            let wake: &'static AtomicU32 = unsafe { &*(bell.wake as *const AtomicU32) };
            std::thread::spawn(move || {
                let t = Instant::now();
                while wake.load(Ordering::Acquire) == seen && t.elapsed() < Duration::from_secs(10) {
                    crate::park::sys::wait(wake, seen, Duration::from_secs(10));
                }
                t.elapsed()
            })
        };
        std::thread::sleep(Duration::from_millis(50));
        control.rescan(&AtomicU64::new(0));
        let waited = waiter.join().unwrap();
        assert!(waited < Duration::from_secs(5), "the rescan did not wake the waiter ({waited:?})");
        drop(mem);
    }

    /// §99.5: the guest's case. A port write that never returns (the host end
    /// stopped draining a virtio-serial port) must not hold any poker: every
    /// session's ring threads poke through this one writer.
    #[test]
    fn a_port_write_that_never_returns_blocks_no_poker() {
        struct Stuck;
        impl std::io::Write for Stuck {
            fn write(&mut self, _b: &[u8]) -> std::io::Result<usize> {
                loop {
                    std::thread::park();
                }
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let wake = Arc::new(PortWake::new(DetachedWriter::spawn(Stuck).unwrap()));
        let t = Instant::now();
        let pokers: Vec<_> = (0..8)
            .map(|_| {
                let w = Arc::clone(&wake);
                std::thread::spawn(move || {
                    for _ in 0..10_000 {
                        w.poke(1);
                    }
                })
            })
            .collect();
        for p in pokers {
            p.join().unwrap();
        }
        assert!(t.elapsed() < Duration::from_secs(10), "pokers blocked: {:?}", t.elapsed());
        let (poked, dropped) = wake.stats();
        assert_eq!(poked + dropped, 80_000);
        assert!(dropped > 0);
    }

    /// §97.10: a hub that never reads must not stop the application. The
    /// socket fills, the queue fills, and every poke after that returns at
    /// once as dropped; the first ones still arrive.
    #[cfg(unix)]
    #[test]
    fn a_doorbell_nobody_reads_never_blocks_the_poker() {
        let (ours, theirs) = std::os::unix::net::UnixStream::pair().unwrap();
        let (_reader, writer) = PortHandle::Unix(ours).split().unwrap();
        let wake = PortWake::new(writer);
        let t = Instant::now();
        for _ in 0..2_000_000 {
            wake.poke(1);
        }
        assert!(t.elapsed() < Duration::from_secs(20), "pokes blocked: {:?}", t.elapsed());
        let (poked, dropped) = wake.stats();
        assert!(dropped > 0 && poked > 0, "poked {poked}, dropped {dropped}");
        theirs.set_nonblocking(true).unwrap();
        let mut b = [0u8; 16];
        assert!(std::io::Read::read(&mut &theirs, &mut b).unwrap() > 0, "the first bytes arrive");
    }

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
            let watch = ctrl.clone();
            let mut host_end = host_end;
            std::thread::spawn(move || {
                // Wait for the waiter to be observably parked, not for a
                // duration: under load the 32-yield spin phase outlasts any
                // fixed sleep, readiness gets seen during the spin, and the
                // doorbell this test exists to exercise is never needed.
                // A liveness guard, not a performance bound: it exists so a consumer
            // that never parks fails instead of hanging. Sixty seconds because at
            // load 312 the 32-yield spin phase alone exceeded ten, and a guard
            // that fires on a slow machine is the defect this whole exercise is
            // about. Slow pays time; broken still fails.
            let until = Instant::now() + Duration::from_secs(60);
                let bell = watch.bell(Lane::Command).unwrap();
                while bell.parked.load(Ordering::Acquire) == 0 {
                    assert!(Instant::now() < until, "the waiter never parked");
                    std::thread::sleep(Duration::from_millis(1));
                }
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
            first_slice: Duration::from_secs(30),
            slice: Duration::from_secs(30),
            ..Futex::default()
        };

        let before_bell = crate::park::woken_by_bell();
        let t0 = Instant::now();
        let outcome = fixed.park(&site, Deadline::after(Duration::from_secs(30)));
        let took = t0.elapsed();
        poker.join().unwrap();

        assert_eq!(outcome, ParkOutcome::Woken);
        // Property, not stopwatch. Two independent observations, both exact:
        // the `Doorman` converted a byte into a wake, and the waiter was
        // released by a `wake` change rather than by its slice. A 2 s slice that
        // had expired would have taken 2 s; but the assertion does not need to
        // say that, because it can say the stronger thing directly.
        assert!(doorman.woken() >= 1, "the doorman delivered no wakes at all");
        assert!(
            took < Duration::from_secs(5),
            "the wait took {took:?} against a 30 s slice; that is the slice \
             expiring, so the byte woke nothing"
        );
        println!(
            "a byte off the port woke a parked waiter in {took:?} \
             (bell-released waits {before_bell} -> {})",
            crate::park::woken_by_bell()
        );
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

    /// §87: one port, several sessions. A byte rings its lane in every live
    /// session, and a session that has ended is dropped from the set rather
    /// than kept alive by it.
    #[test]
    fn a_control_set_rings_every_live_session_and_forgets_ended_ones() {
        let (a, slot_a) = setup();
        let (b, slot_b) = setup();
        assert_eq!(slot_a, slot_b, "the same claim order gives the same slot");
        let set = ControlSet::new();
        set.add(&a);
        set.add(&b);
        assert_eq!(set.len(), 2);
        let (mut host_end, guest_end) = pipe();
        let doorman = Doorman::spawn_target(guest_end, Arc::clone(&set));
        let wait_for = |n: u64| {
            let deadline = Instant::now() + Duration::from_secs(10);
            while doorman.woken() < n && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(1));
            }
            doorman.woken()
        };
        host_end.write_all(&[crate::park::lane_token(slot_a, Lane::Reply)]).unwrap();
        assert_eq!(wait_for(2), 2, "one byte, two sessions, two deliveries");
        drop(b);
        assert_eq!(set.len(), 1);
        host_end.write_all(&[crate::park::lane_token(slot_a, Lane::Reply)]).unwrap();
        assert_eq!(wait_for(3), 3, "the ended session is not rung");
        drop(host_end);
        assert_eq!(doorman.join(), ServeEnd::Closed);
        drop(a);
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

#[cfg(test)]
mod rescan_tests {
    use super::*;
    use crate::control::{Control, Geometry, ProtocolIds};
    use crate::region::SharedRegion;
    use crate::wlbytes::{ByteChannel, Endpoint};
    use std::time::{Duration, Instant};

    /// §105: `rescan` wakes a reader parked on its lane with nothing
    /// written, so a wake lost while the port was closed costs a look, not
    /// the waiter's whole slice.
    #[test]
    fn rescan_wakes_every_parked_lane() {
        let g = Geometry::small();
        let region = Arc::new(SharedRegion::heap(g.control_size as usize).unwrap());
        let control = Arc::new(Control::create(region, g, ProtocolIds::new(7, 1), 1).unwrap());
        let host = ByteChannel::claim(&control, 7).unwrap();
        let guest = ByteChannel::open(&control, host.slot(), Endpoint::Peer).unwrap();
        let t = std::thread::spawn(move || {
            let t0 = Instant::now();
            let _ = guest.wait_readable(Duration::from_secs(20));
            t0.elapsed()
        });
        std::thread::sleep(Duration::from_millis(200));
        let deadline = Instant::now() + Duration::from_secs(5);
        while !t.is_finished() && Instant::now() < deadline {
            control.rescan(&AtomicU64::new(0));
            std::thread::sleep(Duration::from_millis(50));
        }
        let took = t.join().unwrap();
        assert!(took < Duration::from_secs(10), "the parked reader slept {took:?}: rescan() woke nothing");
    }

    /// §105: the hub's rescan byte is not a lane; the doorman turns it into
    /// `rescan` (as it does the open itself), and rings the lane bytes around it.
    #[test]
    fn a_rescan_byte_reopens_and_lane_bytes_still_ring() {
        struct T(AtomicU64, AtomicU64);
        impl BellTarget for T {
            fn ring_token(&self, _t: u8, _w: &AtomicU64) -> bool {
                self.0.fetch_add(1, Ordering::Relaxed);
                true
            }
            fn rescan(&self, _w: &AtomicU64) {
                self.1.fetch_add(1, Ordering::Relaxed);
            }
        }
        let t = T(AtomicU64::new(0), AtomicU64::new(0));
        let bytes: &[u8] = &[5, crate::park::RESCAN_TOKEN, 9];
        let end = serve_port(bytes, &t, &AtomicBool::new(false), &AtomicU64::new(0));
        assert_eq!(end, ServeEnd::Closed);
        assert_eq!((t.0.load(Ordering::Relaxed), t.1.load(Ordering::Relaxed)), (2, 2), "the open and the token");
    }
}
