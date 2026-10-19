//! §106: the host-integration channel on the host. The `OWNER_HOSTINT` ring
//! slot, as a [`wc_hostint::host::Link`], and the thread that serves it.
//!
//! The process that owns the session's rings calls [`spawn`] once it has the
//! slot: on Windows the compositor (the only process that can map the
//! ivshmem device), on macOS the main process. The thread is its own, never
//! the Wayland thread, and it ends when the ring reaches EOF or the peer
//! dies. Requests are checked by `wc_hostint::host::Service` before the
//! platform's opener runs (security-model.md, "Opening links on the host").

use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};

use sv_transport::wlbytes::{ByteChannel, ReadOutcome};
use wc_hostint::host::{Event, Link, Opener, Poll, Service};
use wc_hostint::wire::LineBuf;

/// How a reader sleeps until the ring has bytes, for at most the given time.
/// It must wait on the doorbell (`ByteChannel::wait_readable` with
/// `ringlink::ring_wait_slice()` once the rings have one), not spin.
pub type Wait = Box<dyn FnMut(&ByteChannel, Duration) + Send>;

pub struct RingLink {
    ch: ByteChannel,
    lines: LineBuf,
    wait: Wait,
}

impl RingLink {
    pub fn new(ch: ByteChannel, wait: Wait) -> Self {
        Self { ch, lines: LineBuf::default(), wait }
    }
}

impl Link for RingLink {
    fn poll_line(&mut self, timeout: Duration) -> Poll {
        let deadline = Instant::now() + timeout;
        let mut tmp = [0u8; 4096];
        loop {
            if let Some(l) = self.lines.next_line() {
                return Poll::Line(l);
            }
            match self.ch.read(&mut tmp) {
                Ok(ReadOutcome::Bytes(n)) => self.lines.push(&tmp[..n]),
                Ok(ReadOutcome::Eof) | Err(_) => return Poll::Closed,
                Ok(ReadOutcome::Empty) => {
                    let now = Instant::now();
                    if now >= deadline {
                        return Poll::Idle;
                    }
                    (self.wait)(&self.ch, deadline - now);
                }
            }
        }
    }

    fn send_line(&mut self, line: &str) -> bool {
        let mut b = Vec::with_capacity(line.len() + 1);
        b.extend_from_slice(line.as_bytes());
        b.push(b'\n');
        let mut rest = &b[..];
        let t0 = Instant::now();
        while !rest.is_empty() {
            match self.ch.write(rest) {
                Ok(n) if n > 0 => rest = &rest[n..],
                // A full ring: the Linux side is slow. Two seconds, then the
                // peer is taken as gone.
                Ok(_) if t0.elapsed() < Duration::from_secs(2) => std::thread::sleep(Duration::from_millis(2)),
                _ => return false,
            }
        }
        true
    }
}

/// Serve the channel on a thread of its own until it ends. `log` gets one
/// line per request.
pub fn spawn(
    ch: ByteChannel,
    wait: Wait,
    mut svc: Service,
    mut opener: Box<dyn Opener + Send>,
    events: Receiver<Event>,
    mut log: Box<dyn FnMut(&str) + Send>,
) -> std::io::Result<std::thread::JoinHandle<()>> {
    std::thread::Builder::new().name("hostint".into()).spawn(move || {
        log(&format!("hostint: serving ring slot {}", ch.slot()));
        let mut link = RingLink::new(ch, wait);
        wc_hostint::host::serve(&mut link, &mut svc, opener.as_mut(), &events, Duration::from_millis(250), &mut *log);
        log("hostint: the channel ended");
    })
}

/// The wait every caller should pass to [`spawn`]: sleep on the lane's
/// doorbell (§103.15), at most one `ring_wait_slice`, which is long when the
/// rings have a doorbell and a millisecond when they do not.
pub fn doorbell_wait() -> Wait {
    Box::new(|ch, t| {
        let _ = ch.wait_readable(t.min(crate::ring_wait_slice()));
    })
}
