//! Bounded, independent capacity for IRQ waits and immediate Mach operations.
#![cfg_attr(not(target_os = "macos"), allow(dead_code))]
use std::{
    io,
    sync::{
        mpsc::{self, SyncSender, TrySendError},
        Arc, Mutex,
    },
};
pub(crate) const WAIT_WORKERS: usize = 8;
const FAST_WORKERS: usize = 2;
const QUEUE_SIZE: usize = 64;

pub(crate) struct Workers<T> {
    immediate: SyncSender<T>,
    waiting: SyncSender<T>,
}
impl<T: Send + 'static> Workers<T> {
    pub(crate) fn new(handle: impl Fn(T) + Send + Sync + 'static) -> io::Result<Self> {
        let handle = Arc::new(handle);
        let spawn = |name: &str, count: usize| -> io::Result<SyncSender<T>> {
            let (sender, receiver) = mpsc::sync_channel(QUEUE_SIZE);
            let receiver = Arc::new(Mutex::new(receiver));
            for n in 0..count {
                let receiver = Arc::clone(&receiver);
                let handle = Arc::clone(&handle);
                std::thread::Builder::new()
                    .name(format!("wc-mach-{name}-{n}"))
                    .spawn(move || loop {
                        let packet = receiver.lock().unwrap_or_else(|e| e.into_inner()).recv();
                        let Ok(packet) = packet else { break };
                        handle(packet);
                    })?;
            }
            Ok(sender)
        };
        Ok(Self {
            immediate: spawn("immediate", FAST_WORKERS)?,
            waiting: spawn("wait", WAIT_WORKERS)?,
        })
    }
    pub(crate) fn submit(&self, op: u32, packet: T) -> Result<(), TrySendError<T>> {
        if op == 2 {
            self.waiting.try_send(packet)
        } else {
            self.immediate.try_send(packet)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{sync::Condvar, time::Duration};
    #[test]
    fn all_irq_wait_slots_can_be_occupied_while_ring_and_redeem_complete() {
        let gate = Arc::new((Mutex::new(false), Condvar::new()));
        struct Release(Arc<(Mutex<bool>, Condvar)>);
        impl Drop for Release {
            fn drop(&mut self) {
                *self.0 .0.lock().unwrap() = true;
                self.0 .1.notify_all();
            }
        }
        let release = Release(Arc::clone(&gate));
        let (started, ready) = mpsc::channel();
        let (completed, done) = mpsc::channel();
        let workers = Workers::new(move |op: u32| {
            if op == 2 {
                started.send(()).unwrap();
                let (lock, cv) = &*gate;
                drop(cv.wait_while(lock.lock().unwrap(), |open| !*open).unwrap());
            }
            completed.send(op).unwrap();
        })
        .unwrap();
        for _ in 0..WAIT_WORKERS {
            workers.submit(2, 2).unwrap();
        }
        for _ in 0..WAIT_WORKERS {
            ready.recv_timeout(Duration::from_secs(2)).unwrap();
        }
        // Every wait worker is now blocked on a gate with no timeout. Neither
        // immediate completion can be explained by an IRQ wait expiring.
        workers.submit(0, 0).unwrap();
        workers.submit(1, 1).unwrap();
        let mut immediate = vec![
            done.recv_timeout(Duration::from_secs(2)).unwrap(),
            done.recv_timeout(Duration::from_secs(2)).unwrap(),
        ];
        immediate.sort();
        assert_eq!(immediate, vec![0, 1]);
        assert!(done.try_recv().is_err());
        drop(release);
        for _ in 0..WAIT_WORKERS {
            assert_eq!(done.recv_timeout(Duration::from_secs(2)).unwrap(), 2);
        }
    }
}
