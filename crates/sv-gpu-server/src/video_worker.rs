//! Bounded execution ownership for accepted external-video submissions.
//! Enqueue never waits for a GPU fence or a future semaphore. The worker owns
//! every accepted job until its producer/native consumers retire. This module
//! supplies execution, not a second resource registry or reclamation timer.
use ash::vk;
use std::sync::{Mutex, mpsc};

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
    fn retire_failed(&mut self);
    /// Deliver completion/error to the canonical session machinery. Successful
    /// enqueue alone is never a decode/fence completion notification.
    fn finished(&mut self, result: Result<(), vk::Result>);
}

pub(crate) struct Queue {
    sender: Option<mpsc::SyncSender<Box<dyn Submission>>>,
    worker: Mutex<Option<std::thread::JoinHandle<()>>>,
}
impl Queue {
    /// One queue per native device. Ordinary queue submissions and video jobs
    /// must enter the same ordered executor once this runtime is active.
    pub(crate) fn new() -> std::io::Result<Self> {
        let (sender, receiver) = mpsc::sync_channel::<Box<dyn Submission>>(16);
        let worker = std::thread::Builder::new()
            .name("native-video-submit".into())
            .stack_size(256 * 1024)
            .spawn(move || {
                while let Ok(mut job) = receiver.recv() {
                    let result =
                        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| job.execute()))
                            .unwrap_or(Err(vk::Result::ERROR_DEVICE_LOST));
                    if result.is_err() {
                        job.retire_failed();
                    }
                    job.finished(result);
                    // Owners drop only after execution/failed-native retirement.
                }
            })?;
        Ok(Self {
            sender: Some(sender),
            worker: Mutex::new(Some(worker)),
        })
    }
    /// The caller must validate all supported-command/handle/synchronization
    /// shapes before enqueue. Full/disconnected queues refuse without accepting
    /// the job. No semaphore/fence wait occurs on the submitting RPC thread.
    pub(crate) fn enqueue(&self, job: Box<dyn Submission>) -> Result<(), vk::Result> {
        self.sender
            .as_ref()
            .ok_or(vk::Result::ERROR_DEVICE_LOST)?
            .try_send(job)
            .map_err(|error| match error {
                mpsc::TrySendError::Full(_) => vk::Result::ERROR_OUT_OF_HOST_MEMORY,
                mpsc::TrySendError::Disconnected(_) => vk::Result::ERROR_DEVICE_LOST,
            })
    }
}
impl Drop for Queue {
    fn drop(&mut self) {
        // Closing sends no cancellation that could discard a live allocation.
        // Every accepted job drains before the execution thread exits.
        self.sender.take();
        if let Some(worker) = self
            .worker
            .get_mut()
            .unwrap_or_else(|e| e.into_inner())
            .take()
        {
            if worker.thread().id() != std::thread::current().id() {
                let _ = worker.join();
            }
        }
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
        fn retire_failed(&mut self) {}
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
        drop(queue);
        done.recv().unwrap();
    }
}
