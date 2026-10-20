//! Deterministic worker ownership tests. These execute the production bounded
//! queue and retirement-proof classifier with simulated native completion.
//! They do not establish native driver cancellation or video readiness.
#![allow(dead_code)]
#[path = "../src/video_worker.rs"]
mod video_worker;
use ash::vk;
use std::sync::{Arc, Mutex, mpsc};
use video_worker::{Queue, RetirementProof, Submission};

struct Owner(Arc<Mutex<Vec<&'static str>>>);
impl Drop for Owner {
    fn drop(&mut self) {
        self.0.lock().unwrap().push("drop");
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
    fn retire_failed(&mut self) {
        self.log.lock().unwrap().push("retire");
        self.observed.send("retiring").unwrap();
        // The simulated owner does not get released on OOM from idle. A later
        // explicit native-retirement event supplies the only accepted proof.
        let failure = self.idle.recv().unwrap();
        assert!(RetirementProof::from_idle_result(failure).is_err());
        self.observed.send("idle-failed").unwrap();
        let _proof = RetirementProof::from_idle_result(self.idle.recv().unwrap()).unwrap();
        self.log.lock().unwrap().push("retired");
    }
    fn finished(&mut self, result: Result<(), vk::Result>) {
        assert!(result.is_err());
        self.log.lock().unwrap().push("finished");
    }
}
fn failed_job_keeps_owners_until_proven_retirement(panic: bool) {
    let log = Arc::new(Mutex::new(Vec::new()));
    let (event, observed) = mpsc::channel();
    let (signal, idle) = mpsc::channel();
    let queue = Queue::new().unwrap();
    queue
        .enqueue(Box::new(FailedJob {
            _owner: Owner(log.clone()),
            log: log.clone(),
            idle,
            observed: event,
            panic,
        }))
        .unwrap();
    assert_eq!(
        observed
            .recv_timeout(std::time::Duration::from_secs(5))
            .unwrap(),
        "retiring"
    );
    assert_eq!(*log.lock().unwrap(), ["execute", "retire"]);
    signal
        .send(Err(vk::Result::ERROR_OUT_OF_DEVICE_MEMORY))
        .unwrap();
    assert_eq!(
        observed
            .recv_timeout(std::time::Duration::from_secs(5))
            .unwrap(),
        "idle-failed"
    );
    assert_eq!(
        *log.lock().unwrap(),
        ["execute", "retire"],
        "failed idle must not drop accepted backing"
    );
    signal.send(Ok(())).unwrap();
    drop(queue); // Drains, then joins. There is no timeout-based reclamation.
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
