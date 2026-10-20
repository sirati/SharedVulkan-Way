//! Idle shutdown is an ownership decision, not just a session count.
use std::sync::{Arc, Mutex};

#[derive(Default)]
struct State {
    active: usize,
    retained: bool,
    closing: bool,
}

#[derive(Default)]
pub(crate) struct IdleOwner(Mutex<State>);

impl IdleOwner {
    pub(crate) fn admit(self: &Arc<Self>) -> Option<SessionOwner> {
        let mut state = self.0.lock().ok()?;
        if state.closing { return None; }
        state.active = state.active.checked_add(1)?;
        Some(SessionOwner { owner: Arc::clone(self), confirmed: false })
    }

    pub(crate) fn claim_exit(&self, external_retention: bool) -> bool {
        let Ok(mut state) = self.0.lock() else { return false; };
        state.retained |= external_retention;
        if state.active != 0 || state.retained || state.closing { return false; }
        state.closing = true;
        true
    }
}

/// Dropping an unstarted, failed, panicked or partially initialized worker
/// retains the process. Only explicit completed cleanup releases that veto.
pub(crate) struct SessionOwner {
    owner: Arc<IdleOwner>,
    confirmed: bool,
}

impl SessionOwner {
    pub(crate) fn confirm(mut self) { self.confirmed = true; }

    pub(crate) fn finish<T, E>(mut self, result: &Result<T, E>, external_retention: bool) {
        self.confirmed = result.is_ok() && !external_retention;
    }
}

impl Drop for SessionOwner {
    fn drop(&mut self) {
        if let Ok(mut state) = self.owner.0.lock() {
            state.retained |= !self.confirmed;
            if let Some(active) = state.active.checked_sub(1) {
                state.active = active;
            } else {
                state.retained = true;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unstarted_worker_closure_retains_ownership_after_its_capture_drops() {
        let owner = Arc::new(IdleOwner::default());
        let session = owner.admit().unwrap();
        let unstarted_worker = move || session.confirm();
        drop(unstarted_worker);
        assert!(!owner.claim_exit(false));
        owner.admit().unwrap().confirm();
        assert!(!owner.claim_exit(false), "a later clean session cannot clear uncertainty");
    }

    #[test]
    fn early_setup_failure_and_a_panicking_worker_keep_the_process_after_join() {
        fn early_setup(owner: &Arc<IdleOwner>) -> Result<(), &'static str> {
            let session = owner.admit().unwrap();
            let setup: Result<(), _> = Err("partial setup failed");
            setup?;
            session.confirm();
            Ok(())
        }
        let owner = Arc::new(IdleOwner::default());
        assert!(early_setup(&owner).is_err());
        assert!(!owner.claim_exit(false));
        let owner = Arc::new(IdleOwner::default());
        let session = owner.admit().unwrap();
        assert!(std::thread::spawn(move || {
            let _session = session;
            panic!("injected session worker failure");
        }).join().is_err());
        assert!(!owner.claim_exit(false));
    }

    #[test]
    fn a_normal_session_error_does_not_authorize_exit() {
        let owner = Arc::new(IdleOwner::default());
        let result: Result<u64, _> = Err("normal session failed");
        owner.admit().unwrap().finish(&result, false);
        assert!(!owner.claim_exit(false));
        owner.admit().unwrap().finish(&Ok::<_, ()>(12u64), false);
        assert!(!owner.claim_exit(false));
    }

    #[test]
    fn successful_serving_with_retained_resources_is_not_completed_cleanup() {
        let owner = Arc::new(IdleOwner::default());
        owner.admit().unwrap().finish(&Ok::<_, ()>(12u64), true);
        assert!(!owner.claim_exit(false));
    }

    #[test]
    fn quarantine_vetoes_exit_after_confirmed_workers_and_cannot_be_cleared() {
        let owner = Arc::new(IdleOwner::default());
        owner.admit().unwrap().confirm();
        assert!(!owner.claim_exit(true));
        assert!(!owner.claim_exit(false));
    }

    #[test]
    fn admission_and_shutdown_have_one_winner() {
        for _ in 0..128 {
            let owner = Arc::new(IdleOwner::default());
            let barrier = Arc::new(std::sync::Barrier::new(2));
            let peer = Arc::clone(&owner);
            let ready = Arc::clone(&barrier);
            let worker = std::thread::spawn(move || {
                ready.wait();
                let session = peer.admit();
                let admitted = session.is_some();
                ready.wait();
                if let Some(session) = session { session.confirm(); }
                admitted
            });
            barrier.wait();
            let exiting = owner.claim_exit(false);
            barrier.wait();
            let admitted = worker.join().unwrap();
            assert_ne!(admitted, exiting);
            if exiting {
                assert!(owner.admit().is_none());
            } else {
                assert!(owner.claim_exit(false), "confirmed retirement allows idle exit");
            }
        }
    }
}
