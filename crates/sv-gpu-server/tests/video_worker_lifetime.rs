//! Actual bounded-worker ownership tests with simulated native completion.
//! These do not establish native driver cancellation or video readiness.
#![allow(dead_code)]
#[path = "../src/video_worker.rs"]
mod video_worker;
use ash::vk;
use std::sync::{Arc, Mutex, mpsc};
use video_worker::{Queue, RetirementProof, Submission};
const WAIT: std::time::Duration = std::time::Duration::from_secs(5);
struct Owner {
    log: Arc<Mutex<Vec<&'static str>>>,
    event: mpsc::Sender<&'static str>,
}
impl Drop for Owner {
    fn drop(&mut self) {
        self.log.lock().unwrap().push("drop");
        let _ = self.event.send("drop");
    }
}
struct FailedJob {
    _owner: Owner,
    log: Arc<Mutex<Vec<&'static str>>>,
    idle: mpsc::Receiver<Result<(), vk::Result>>,
    observed: mpsc::Sender<&'static str>,
    panic: bool,
}
impl Submission for FailedJob {
    fn execute(&mut self) -> Result<(), vk::Result> {
        self.log.lock().unwrap().push("execute");
        if self.panic {
            panic!("simulated native dispatch panic");
        }
        Err(vk::Result::ERROR_OUT_OF_HOST_MEMORY)
    }
    fn retire_failed(&mut self) -> bool {
        self.log.lock().unwrap().push("retire");
        self.observed.send("retiring").unwrap();
        assert!(RetirementProof::from_idle_result(self.idle.recv().unwrap()).is_err());
        self.observed.send("idle-failed").unwrap();
        let _proof = RetirementProof::from_idle_result(self.idle.recv().unwrap()).unwrap();
        self.log.lock().unwrap().push("retired");
        true
    }
    fn finished(&mut self, result: Result<(), vk::Result>) {
        assert!(result.is_err());
        self.log.lock().unwrap().push("finished");
        self.observed.send("finished").unwrap();
    }
}
fn failed_job_keeps_owners_until_proven_retirement(panic: bool) {
    let log = Arc::new(Mutex::new(Vec::new()));
    let (event, observed) = mpsc::channel();
    let (signal, idle) = mpsc::channel();
    let queue = Queue::new().unwrap();
    queue
        .enqueue(Box::new(FailedJob {
            _owner: Owner {
                log: log.clone(),
                event: event.clone(),
            },
            log: log.clone(),
            idle,
            observed: event,
            panic,
        }))
        .unwrap();
    assert_eq!(observed.recv_timeout(WAIT).unwrap(), "retiring");
    assert_eq!(*log.lock().unwrap(), ["execute", "retire"]);
    signal
        .send(Err(vk::Result::ERROR_OUT_OF_DEVICE_MEMORY))
        .unwrap();
    assert_eq!(observed.recv_timeout(WAIT).unwrap(), "idle-failed");
    assert_eq!(*log.lock().unwrap(), ["execute", "retire"]);
    signal.send(Ok(())).unwrap();
    assert_eq!(observed.recv_timeout(WAIT).unwrap(), "finished");
    assert_eq!(observed.recv_timeout(WAIT).unwrap(), "drop");
    drop(queue);
    assert_eq!(
        *log.lock().unwrap(),
        ["execute", "retire", "retired", "finished", "drop"]
    );
}
#[test]
fn execution_error_retains_owner_until_retirement_before_notification_and_drop() {
    failed_job_keeps_owners_until_proven_retirement(false);
}
#[test]
fn unwinding_execution_preserves_owner_for_retirement() {
    failed_job_keeps_owners_until_proven_retirement(true);
}
#[test]
fn native_retirement_proof_refuses_allocation_failure_and_timeout() {
    assert!(RetirementProof::from_idle_result(Ok(())).is_ok());
    assert!(RetirementProof::from_idle_result(Err(vk::Result::ERROR_DEVICE_LOST)).is_ok());
    for error in [
        vk::Result::ERROR_OUT_OF_HOST_MEMORY,
        vk::Result::ERROR_OUT_OF_DEVICE_MEMORY,
        vk::Result::TIMEOUT,
        vk::Result::NOT_READY,
        vk::Result::ERROR_UNKNOWN,
    ] {
        assert!(
            matches!(RetirementProof::from_idle_result(Err(error)), Err(actual) if actual == error)
        );
    }
}
struct ControlledJob {
    _owner: Owner,
    event: mpsc::Sender<&'static str>,
    gate: mpsc::Receiver<Result<(), vk::Result>>,
    started: bool,
    retire: bool,
    panic_finished: bool,
}
impl Submission for ControlledJob {
    fn execute(&mut self) -> Result<(), vk::Result> {
        self.started = true;
        self.event.send("execute").unwrap();
        self.gate.recv().unwrap()
    }
    fn retire_failed(&mut self) -> bool {
        !self.started || self.retire
    }
    fn finished(&mut self, result: Result<(), vk::Result>) {
        self.event
            .send(if result.is_ok() { "success" } else { "failed" })
            .unwrap();
        if self.panic_finished {
            panic!("simulated canonical fatal callback panic");
        }
    }
}
fn controlled(
    retire: bool,
    panic_finished: bool,
) -> (
    Box<dyn Submission>,
    mpsc::Sender<Result<(), vk::Result>>,
    mpsc::Receiver<&'static str>,
) {
    let (event, observed) = mpsc::channel();
    let (signal, gate) = mpsc::channel();
    (
        Box::new(ControlledJob {
            _owner: Owner {
                log: Arc::new(Mutex::new(Vec::new())),
                event: event.clone(),
            },
            event,
            gate,
            started: false,
            retire,
            panic_finished,
        }),
        signal,
        observed,
    )
}
#[test]
fn cancellation_closes_admission_and_quarantines_started_owners() {
    let queue = Queue::new().unwrap();
    assert!(!queue.cancel_if_pending());
    let (job, signal, observed) = controlled(false, false);
    queue.enqueue(job).unwrap();
    assert_eq!(observed.recv_timeout(WAIT).unwrap(), "execute");
    assert!(queue.cancel_if_pending());
    assert_eq!(queue.failure(), Some(vk::Result::ERROR_DEVICE_LOST));
    let (refused, _, refused_events) = controlled(true, false);
    assert_eq!(
        queue.enqueue(refused).unwrap_err(),
        vk::Result::ERROR_DEVICE_LOST
    );
    assert_eq!(refused_events.recv_timeout(WAIT).unwrap(), "drop");
    signal.send(Err(vk::Result::ERROR_DEVICE_LOST)).unwrap();
    assert_eq!(observed.recv_timeout(WAIT).unwrap(), "failed");
    drop(queue);
    assert!(
        observed
            .recv_timeout(std::time::Duration::from_millis(50))
            .is_err(),
        "cancelled native owner must remain quarantined"
    );
}
#[test]
fn queue_drop_does_not_join_work_waiting_on_a_future_signal() {
    let queue = Queue::new().unwrap();
    let (job, signal, observed) = controlled(false, false);
    queue.enqueue(job).unwrap();
    assert_eq!(observed.recv_timeout(WAIT).unwrap(), "execute");
    let (dropped, done) = mpsc::channel();
    let thread = std::thread::spawn(move || {
        drop(queue);
        dropped.send(()).unwrap();
    });
    let nonblocking = done.recv_timeout(std::time::Duration::from_secs(1));
    // Release simulated native execution even when asserting an old blocking
    // Drop implementation, so test failure never strands a worker thread.
    signal.send(Err(vk::Result::ERROR_DEVICE_LOST)).unwrap();
    thread.join().unwrap();
    assert!(
        nonblocking.is_ok(),
        "Queue destruction must not wait for a never-signaled native semaphore"
    );
    assert_eq!(observed.recv_timeout(WAIT).unwrap(), "failed");
    assert!(
        observed
            .recv_timeout(std::time::Duration::from_millis(50))
            .is_err()
    );
}
#[test]
fn callback_panic_cannot_drop_an_unretired_quarantined_job() {
    let queue = Queue::new().unwrap();
    let (job, signal, observed) = controlled(false, true);
    queue.enqueue(job).unwrap();
    assert_eq!(observed.recv_timeout(WAIT).unwrap(), "execute");
    signal.send(Err(vk::Result::ERROR_UNKNOWN)).unwrap();
    assert_eq!(observed.recv_timeout(WAIT).unwrap(), "failed");
    drop(queue);
    assert!(
        observed
            .recv_timeout(std::time::Duration::from_millis(100))
            .is_err(),
        "callback unwinding must retain native-live ownership"
    );
}
#[test]
fn failure_discards_unstarted_jobs_without_executing_them() {
    let queue = Queue::new().unwrap();
    let (first, signal, first_events) = controlled(false, false);
    queue.enqueue(first).unwrap();
    assert_eq!(first_events.recv_timeout(WAIT).unwrap(), "execute");
    let (second, _, second_events) = controlled(false, false);
    queue.enqueue(second).unwrap();
    signal.send(Err(vk::Result::ERROR_UNKNOWN)).unwrap();
    assert_eq!(first_events.recv_timeout(WAIT).unwrap(), "failed");
    assert_eq!(second_events.recv_timeout(WAIT).unwrap(), "failed");
    assert_eq!(second_events.recv_timeout(WAIT).unwrap(), "drop");
    drop(queue);
}
#[test]
fn quarantined_executors_keep_their_finite_process_admission_permits() {
    let result = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "quarantine_admission_is_process_bounded_child",
            "--ignored",
            "--nocapture",
        ])
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&result.stdout),
        String::from_utf8_lossy(&result.stderr)
    );
}
#[test]
#[ignore = "isolated child process intentionally retains every native executor permit"]
fn quarantine_admission_is_process_bounded_child() {
    for _ in 0..16 {
        let queue = Queue::new().unwrap();
        let (job, signal, observed) = controlled(false, false);
        queue.enqueue(job).unwrap();
        assert_eq!(observed.recv_timeout(WAIT).unwrap(), "execute");
        signal.send(Err(vk::Result::ERROR_UNKNOWN)).unwrap();
        assert_eq!(observed.recv_timeout(WAIT).unwrap(), "failed");
        drop(queue);
    }
    assert!(
        Queue::new().is_err(),
        "recreating a session cannot bypass quarantined executor admission"
    );
}

struct AsyncJob {
    drops: Arc<std::sync::atomic::AtomicUsize>,
    native_done: Arc<std::sync::atomic::AtomicBool>,
    finished: mpsc::Sender<()>,
    finish_gate: Option<mpsc::Receiver<()>>,
}
impl Drop for AsyncJob {
    fn drop(&mut self) {
        self.drops.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
    }
}
impl Submission for AsyncJob {
    fn execute(&mut self) -> Result<(), vk::Result> {
        Ok(())
    }
    fn retire_failed(&mut self) -> bool {
        false
    }
    fn completed(&mut self) -> bool {
        self.native_done.load(std::sync::atomic::Ordering::Acquire)
    }
    fn finished(&mut self, result: Result<(), vk::Result>) {
        result.unwrap();
        self.finished.send(()).unwrap();
        if let Some(gate) = &self.finish_gate {
            gate.recv().unwrap();
        }
    }
}
#[test]
fn successful_uncompleted_jobs_share_finite_admission_until_native_completion() {
    let queue = Queue::new().unwrap();
    let drops = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let native_done = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let (finished, events) = mpsc::channel();
    for _ in 0..17 {
        queue
            .enqueue(Box::new(AsyncJob {
                drops: drops.clone(),
                native_done: native_done.clone(),
                finished: finished.clone(),
                finish_gate: None,
            }))
            .unwrap();
        events.recv_timeout(WAIT).unwrap();
    }
    assert_eq!(drops.load(std::sync::atomic::Ordering::Acquire), 0);
    assert_eq!(
        queue
            .enqueue(Box::new(AsyncJob {
                drops: drops.clone(),
                native_done: native_done.clone(),
                finished,
                finish_gate: None
            }))
            .unwrap_err(),
        vk::Result::ERROR_OUT_OF_HOST_MEMORY
    );
    assert_eq!(
        drops.load(std::sync::atomic::Ordering::Acquire),
        1,
        "only the refused job can release owners"
    );
    native_done.store(true, std::sync::atomic::Ordering::Release);
    let (first, signal, first_events) = controlled(true, false);
    queue.enqueue(first).unwrap();
    assert_eq!(first_events.recv_timeout(WAIT).unwrap(), "execute");
    // Execution of this sentinel proves the prior worker iteration published
    // its final pending job. A second admission can reap every completed job.
    let (second, second_signal, second_events) = controlled(true, false);
    queue.enqueue(second).unwrap();
    assert_eq!(drops.load(std::sync::atomic::Ordering::Acquire), 18);
    signal.send(Ok(())).unwrap();
    assert_eq!(first_events.recv_timeout(WAIT).unwrap(), "success");
    assert_eq!(first_events.recv_timeout(WAIT).unwrap(), "drop");
    assert_eq!(second_events.recv_timeout(WAIT).unwrap(), "execute");
    second_signal.send(Ok(())).unwrap();
    assert_eq!(second_events.recv_timeout(WAIT).unwrap(), "success");
    assert_eq!(second_events.recv_timeout(WAIT).unwrap(), "drop");
    assert!(!queue.cancel_if_pending());
}
#[test]
fn cancellation_during_success_callback_quarantines_job_before_worker_exit() {
    let queue = Queue::new().unwrap();
    let drops = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let (finished, observed) = mpsc::channel();
    let (release, gate) = mpsc::channel();
    queue
        .enqueue(Box::new(AsyncJob {
            drops: drops.clone(),
            native_done: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            finished,
            finish_gate: Some(gate),
        }))
        .unwrap();
    observed.recv_timeout(WAIT).unwrap();
    drop(queue);
    release.send(()).unwrap();
    // No timer releases this owner. This bounded observation lets an incorrect
    // detached worker exit and drop its State, exposing the regression.
    std::thread::sleep(std::time::Duration::from_millis(100));
    assert_eq!(drops.load(std::sync::atomic::Ordering::Acquire), 0);
}

struct ReentrantFailure {
    _owner: Owner,
    queue: std::sync::Weak<Queue>,
    finished: mpsc::Sender<()>,
}
impl Submission for ReentrantFailure {
    fn execute(&mut self) -> Result<(), vk::Result> {
        Err(vk::Result::ERROR_UNKNOWN)
    }
    fn retire_failed(&mut self) -> bool {
        false
    }
    fn finished(&mut self, _: Result<(), vk::Result>) {
        self.queue.upgrade().unwrap().cancel();
        self.finished.send(()).unwrap();
    }
}
#[test]
fn failure_callback_can_cancel_its_own_queue_without_quarantine_lock_deadlock() {
    let queue = Arc::new(Queue::new().unwrap());
    let (dropped, observed) = mpsc::channel();
    let (finished, done) = mpsc::channel();
    queue
        .enqueue(Box::new(ReentrantFailure {
            _owner: Owner {
                log: Arc::new(Mutex::new(Vec::new())),
                event: dropped,
            },
            queue: Arc::downgrade(&queue),
            finished,
        }))
        .unwrap();
    // A regressed callback holds its upgraded Queue Arc while blocked, so
    // this assertion can fail without Queue::drop itself joining/deadlocking.
    assert!(
        done.recv_timeout(WAIT).is_ok(),
        "fatal callback must be able to cancel the canonical queue reentrantly"
    );
    drop(queue);
    assert!(
        observed
            .recv_timeout(std::time::Duration::from_millis(50))
            .is_err()
    );
}
