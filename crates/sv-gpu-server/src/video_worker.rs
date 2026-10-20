//! Bounded execution ownership for accepted external-video submissions.
//! Enqueue never waits for a GPU fence or a future semaphore. The worker owns
//! every accepted job until its producer/native consumers retire. This module
//! supplies execution, not a second resource registry or reclamation timer.
use ash::vk;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicI32, AtomicUsize, Ordering},
    mpsc,
};

// Quarantined devices keep their permit. Recreating sessions cannot bypass it.
const MAX_NATIVE_EXECUTORS: usize = 16;
static NATIVE_EXECUTORS: AtomicUsize = AtomicUsize::new(0);
struct State {
    failure: AtomicI32,
    accepted: AtomicUsize,
    quarantine: Mutex<Vec<(Arc<Mutex<Box<dyn Submission>>>, Arc<State>)>>,
    pending: Mutex<Vec<Box<dyn Submission>>>,
}
impl Drop for State {
    fn drop(&mut self) {
        NATIVE_EXECUTORS.fetch_sub(1, Ordering::AcqRel);
    }
}

/// Evidence that pending native objects may be retired. A failed idle caused
/// by allocation pressure is not evidence that the device stopped reading.
#[derive(Clone, Copy, Debug)]
pub(crate) struct RetirementProof(());
impl RetirementProof {
    pub(crate) fn from_idle_result(result: Result<(), vk::Result>) -> Result<Self, vk::Result> {
        match result {
            Ok(()) | Err(vk::Result::ERROR_DEVICE_LOST) => Ok(Self(())),
            Err(error) => Err(error),
        }
    }
}

pub(crate) trait Submission: Send {
    /// All shared allocation/parameter/driver owners belong to `self`, rather
    /// than temporary values that would disappear during unwinding.
    fn execute(&mut self) -> Result<(), vk::Result>;
    /// Called after failed execution or panic while `self` still owns every
    /// resource. Retire native work before allowing those owners to drop.
    fn retire_failed(&mut self) -> bool;
    /// Nonblocking native completion proof for successful asynchronous work.
    /// A false result keeps the whole accepted Job in the canonical executor.
    fn completed(&mut self) -> bool {
        true
    }
    /// Deliver completion/error to the canonical session machinery. Successful
    /// enqueue alone is never a decode/fence completion notification.
    fn finished(&mut self, result: Result<(), vk::Result>);
}

pub(crate) struct Queue {
    sender: Option<mpsc::SyncSender<Box<dyn Submission>>>,
    worker: Mutex<Option<std::thread::JoinHandle<()>>>,
    state: Arc<State>,
}
impl Queue {
    /// One queue per native device. Ordinary queue submissions and video jobs
    /// must enter the same ordered executor once this runtime is active.
    pub(crate) fn new() -> std::io::Result<Self> {
        NATIVE_EXECUTORS
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| {
                (n < MAX_NATIVE_EXECUTORS).then_some(n + 1)
            })
            .map_err(|_| std::io::Error::other("native executor admission exhausted"))?;
        let state = Arc::new(State {
            failure: AtomicI32::new(0),
            accepted: AtomicUsize::new(0),
            quarantine: Mutex::new(Vec::new()),
            pending: Mutex::new(Vec::new()),
        });
        let worker_state = Arc::clone(&state);
        let (sender, receiver) = mpsc::sync_channel::<Box<dyn Submission>>(16);
        let worker = std::thread::Builder::new()
            .name("native-video-submit".into())
            .stack_size(256 * 1024)
            .spawn(move || {
                while let Ok(mut job) = receiver.recv() {
                    let failure = worker_state.failure.load(Ordering::Acquire);
                    let result = if failure != 0 {
                        Err(vk::Result::ERROR_DEVICE_LOST)
                    } else {
                        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| job.execute()))
                            .unwrap_or(Err(vk::Result::ERROR_DEVICE_LOST))
                    };
                    if result.is_err() {
                        // Close admission before callbacks or owner disposition.
                        worker_state
                            .failure
                            .store(vk::Result::ERROR_DEVICE_LOST.as_raw(), Ordering::Release);
                    }
                    let retired = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        if result.is_ok() {
                            job.completed()
                        } else {
                            job.retire_failed()
                        }
                    }))
                    .unwrap_or(false);
                    if retired {
                        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                            job.finished(result)
                        }));
                        worker_state.accepted.fetch_sub(1, Ordering::AcqRel);
                    } else if result.is_ok() {
                        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                            job.finished(result)
                        }));
                        let mut pending = worker_state
                            .pending
                            .lock()
                            .unwrap_or_else(|e| e.into_inner());
                        if worker_state.failure.load(Ordering::Acquire) != 0 {
                            worker_state
                                .quarantine
                                .lock()
                                .unwrap_or_else(|e| e.into_inner())
                                .push((Arc::new(Mutex::new(job)), Arc::clone(&worker_state)));
                        } else {
                            pending.push(job);
                        }
                    } else {
                        let held_job = Arc::new(Mutex::new(job));
                        // Install complete ownership before invoking callbacks,
                        // then release the global lock so callbacks may cancel
                        // this executor reentrantly.
                        worker_state
                            .quarantine
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .push((Arc::clone(&held_job), Arc::clone(&worker_state)));
                        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                            held_job
                                .lock()
                                .unwrap_or_else(|e| e.into_inner())
                                .finished(result)
                        }));
                    }
                }
            })?;
        Ok(Self {
            sender: Some(sender),
            worker: Mutex::new(Some(worker)),
            state,
        })
    }
    /// The caller must validate all supported-command/handle/synchronization
    /// shapes before enqueue. Full/disconnected queues refuse without accepting
    /// the job. No semaphore/fence wait occurs on the submitting RPC thread.
    pub(crate) fn enqueue(&self, job: Box<dyn Submission>) -> Result<(), vk::Result> {
        if self.failure().is_some() {
            return Err(vk::Result::ERROR_DEVICE_LOST);
        }
        self.reap_completed();
        self.state
            .accepted
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| {
                (n < 17).then_some(n + 1)
            })
            .map_err(|_| vk::Result::ERROR_OUT_OF_HOST_MEMORY)?;
        let result = self
            .sender
            .as_ref()
            .ok_or(vk::Result::ERROR_DEVICE_LOST)
            .and_then(|sender| {
                sender.try_send(job).map_err(|error| match error {
                    mpsc::TrySendError::Full(_) => vk::Result::ERROR_OUT_OF_HOST_MEMORY,
                    mpsc::TrySendError::Disconnected(_) => vk::Result::ERROR_DEVICE_LOST,
                })
            });
        if result.is_err() {
            self.state.accepted.fetch_sub(1, Ordering::AcqRel);
        }
        result
    }
    fn reap_completed(&self) {
        let mut pending = self.state.pending.lock().unwrap_or_else(|e| e.into_inner());
        let mut index = 0;
        while index < pending.len() {
            if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| pending[index].completed()))
                .unwrap_or(false)
            {
                pending.swap_remove(index);
                self.state.accepted.fetch_sub(1, Ordering::AcqRel);
            } else {
                index += 1;
            }
        }
    }
    pub(crate) fn failure(&self) -> Option<vk::Result> {
        (self.state.failure.load(Ordering::Acquire) != 0).then_some(vk::Result::ERROR_DEVICE_LOST)
    }
    pub(crate) fn cancel_if_pending(&self) -> bool {
        self.reap_completed();
        if self.state.accepted.load(Ordering::Acquire) == 0 {
            return false;
        }
        self.cancel();
        true
    }
    pub(crate) fn cancel(&self) {
        self.state
            .failure
            .store(vk::Result::ERROR_DEVICE_LOST.as_raw(), Ordering::Release);
        let mut pending = self.state.pending.lock().unwrap_or_else(|e| e.into_inner());
        let mut held = self
            .state
            .quarantine
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        for job in pending.drain(..) {
            held.push((Arc::new(Mutex::new(job)), Arc::clone(&self.state)));
        }
    }
}
impl Drop for Queue {
    fn drop(&mut self) {
        // Disconnect never joins a worker blocked behind a future native
        // signal. Its original Job/State retain every owner and admission permit.
        self.cancel();
        self.sender.take();
        self.worker
            .get_mut()
            .unwrap_or_else(|e| e.into_inner())
            .take();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    struct FutureWait {
        gate: mpsc::Receiver<()>,
        completed: mpsc::Sender<()>,
    }
    impl Submission for FutureWait {
        fn execute(&mut self) -> Result<(), vk::Result> {
            self.gate.recv().map_err(|_| vk::Result::ERROR_DEVICE_LOST)
        }
        fn retire_failed(&mut self) -> bool {
            true
        }
        fn finished(&mut self, result: Result<(), vk::Result>) {
            result.unwrap();
            self.completed.send(()).unwrap();
        }
    }
    #[test]
    fn submit_returns_before_future_signal_and_drop_drains_accepted_work() {
        let queue = Queue::new().unwrap();
        let (signal, gate) = mpsc::channel();
        let (completed, done) = mpsc::channel();
        queue
            .enqueue(Box::new(FutureWait { gate, completed }))
            .unwrap();
        // This signal is deliberately issued only after enqueue returned. A
        // synchronous fence wait in the RPC would deadlock this valid ordering.
        assert!(done.try_recv().is_err());
        signal.send(()).unwrap();
        done.recv().unwrap();
        drop(queue);
    }
}
