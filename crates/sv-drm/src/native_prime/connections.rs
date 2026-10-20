//! Bounded blocking owner workers, serviced by the compositor's existing
//! accept turn. Idle owners retain their registrations without an idle timer.
use super::{Registry, serve_connection};
use std::io;
use std::net::Shutdown;
use std::os::unix::net::{UnixListener, UnixStream};
use std::thread::JoinHandle;

/// One native owner connection can own many tokens. The current runtime uses
/// one connection per backing, so 128 permits 128 independent live backings.
/// Each worker requests a 128 KiB stack, at most 16 MiB requested in total.
/// Platform thread bookkeeping and guard pages are additional overhead.
pub const MAX_CONNECTIONS: usize = 128;
/// Accept at most eight connections per compositor turn, including rejected
/// excess connections. A permanently full listen queue cannot starve rendering.
pub const ACCEPT_BUDGET: usize = 8;
const STACK_SIZE: usize = 128 * 1024;

#[derive(Default, Debug, PartialEq, Eq)]
pub struct AcceptReport {
    pub accepted: usize,
    pub rejected: usize,
}
pub struct Connections {
    workers: Vec<(UnixStream, JoinHandle<()>)>,
    limit: usize,
    budget: usize,
}
impl Default for Connections {
    fn default() -> Self {
        Self {
            workers: Vec::new(),
            limit: MAX_CONNECTIONS,
            budget: ACCEPT_BUDGET,
        }
    }
}
impl Connections {
    /// `listener` must be nonblocking. Reject excess/new failed workers by
    /// closing only that new socket. Existing owners never get evicted.
    pub fn accept_pending(
        &mut self,
        listener: &UnixListener,
        registry: &Registry,
    ) -> io::Result<AcceptReport> {
        let mut index = 0;
        while index < self.workers.len() {
            if self.workers[index].1.is_finished() {
                let (_, worker) = self.workers.swap_remove(index);
                let _ = worker.join();
            } else {
                index += 1;
            }
        }
        let mut report = AcceptReport::default();
        // EINTR consumes a turn slot too. Signals cannot turn the bounded
        // accept phase into an unbounded loop.
        for _ in 0..self.budget {
            let stream = match listener.accept() {
                Ok((stream, _)) => stream,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                // Descriptor/socket pressure leaves requests queued. Do not
                // terminate the compositor or revoke existing transport owners.
                Err(error)
                    if matches!(
                        error.raw_os_error(),
                        Some(libc::EMFILE | libc::ENFILE | libc::ENOMEM | libc::ENOBUFS)
                    ) =>
                {
                    break;
                }
                Err(error) => return Err(error),
            };
            if self.workers.len() >= self.limit {
                report.rejected += 1;
                continue;
            }
            // BSD accepted sockets may inherit listener O_NONBLOCK.
            if stream.set_nonblocking(false).is_err() {
                report.rejected += 1;
                continue;
            }
            let cancel = match stream.try_clone() {
                Ok(cancel) => cancel,
                Err(_) => {
                    report.rejected += 1;
                    continue;
                }
            };
            let registry = registry.clone();
            match std::thread::Builder::new()
                .name("prime-owner".into())
                .stack_size(STACK_SIZE)
                .spawn(move || {
                    let _ = serve_connection(stream, unsafe { libc::geteuid() }, &registry);
                }) {
                Ok(worker) => {
                    self.workers.push((cancel, worker));
                    report.accepted += 1;
                }
                Err(_) => {
                    report.rejected += 1;
                }
            }
        }
        Ok(report)
    }
}
impl Drop for Connections {
    fn drop(&mut self) {
        for (stream, _) in &self.workers {
            let _ = stream.shutdown(Shutdown::Both);
        }
        for (_, worker) in self.workers.drain(..) {
            let _ = worker.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    struct Socket {
        listener: UnixListener,
        path: std::path::PathBuf,
    }
    impl Socket {
        fn new() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let path = std::env::temp_dir().join(format!(
                "prime-accept-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            let listener = UnixListener::bind(&path).unwrap();
            listener.set_nonblocking(true).unwrap();
            Self { listener, path }
        }
    }
    impl Drop for Socket {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.path);
        }
    }
    #[test]
    fn idle_at_cap_leaves_existing_owners_and_other_event_loop_work_running() {
        let socket = Socket::new();
        let registry = Registry::default();
        let mut owners = Connections {
            workers: Vec::new(),
            limit: 2,
            budget: 1,
        };
        let first = UnixStream::connect(&socket.path).unwrap();
        let second = UnixStream::connect(&socket.path).unwrap();
        let mut rejected = UnixStream::connect(&socket.path).unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        let driver = std::thread::spawn(move || {
            let mut render_turns = 0;
            for expected in [
                AcceptReport {
                    accepted: 1,
                    rejected: 0,
                },
                AcceptReport {
                    accepted: 1,
                    rejected: 0,
                },
                AcceptReport {
                    accepted: 0,
                    rejected: 1,
                },
            ] {
                assert_eq!(
                    owners.accept_pending(&socket.listener, &registry).unwrap(),
                    expected
                );
                // This is the compositor work scheduled after each accept
                // phase. No GPU or actual rendered-frame claim is made here.
                render_turns += 1;
                tx.send(render_turns).unwrap();
            }
            assert_eq!(owners.workers.len(), 2);
            // Keep idle owners alive through another empty accept/render turn.
            assert_eq!(
                owners.accept_pending(&socket.listener, &registry).unwrap(),
                AcceptReport::default()
            );
            tx.send(render_turns + 1).unwrap();
            drop(owners);
        });
        for turn in 1..=4 {
            assert_eq!(
                rx.recv_timeout(std::time::Duration::from_secs(5)).unwrap(),
                turn
            );
        }
        use std::io::Read;
        rejected
            .set_read_timeout(Some(std::time::Duration::from_secs(1)))
            .unwrap();
        assert_eq!(rejected.read(&mut [0]).unwrap(), 0);
        drop(first);
        drop(second);
        driver.join().unwrap();
    }
}
