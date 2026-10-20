//! One pending immutable output per window. Channel wakeups carry the mailbox,
//! so a stalled UI cannot retain every intermediate full-sized GPU frame.
use std::sync::Mutex;

#[derive(Debug)]
pub struct Latest<T>(Mutex<Option<T>>);
impl<T> Default for Latest<T> {
    fn default() -> Self {
        Self(Mutex::new(None))
    }
}
impl<T> Latest<T> {
    /// True only when a new wakeup is required. Replacements reuse its wakeup.
    pub fn replace(&self, value: T) -> bool {
        self.0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .replace(value)
            .is_none()
    }
    pub fn take(&self) -> Option<T> {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).take()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    #[test]
    fn stalled_ui_retains_only_the_latest_pending_output() {
        let mailbox = Latest::default();
        let old = Arc::new(1);
        let weak = Arc::downgrade(&old);
        assert!(mailbox.replace(old));
        for value in 2..1000 {
            assert!(!mailbox.replace(Arc::new(value)));
        }
        assert!(weak.upgrade().is_none());
        assert_eq!(*mailbox.take().unwrap(), 999);
        assert!(mailbox.take().is_none());
        assert!(mailbox.replace(Arc::new(1000)));
    }
    #[test]
    fn consumed_output_remains_alive_until_display_releases_its_reference() {
        let mailbox = Latest::default();
        let frame = Arc::new(7);
        let weak = Arc::downgrade(&frame);
        mailbox.replace(frame);
        let displayed = mailbox.take().unwrap();
        assert!(mailbox.replace(Arc::new(8)));
        assert_eq!(*displayed, 7);
        assert!(weak.upgrade().is_some());
        drop(displayed);
        assert!(weak.upgrade().is_none());
    }
    #[test]
    fn cpu_transition_clears_old_mailbox_before_a_new_gpu_epoch() {
        let old = Latest::default();
        old.replace(1);
        assert_eq!(old.take(), Some(1)); // CPU transition cancels its pending output.
        let new = Latest::default();
        assert!(new.replace(3));
        assert_eq!(old.take(), None); // An old queued wakeup cannot steal frame3.
        assert_eq!(new.take(), Some(3));
    }
}
