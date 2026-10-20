//! A native graph must survive unwinding until its completion is established.
use std::ops::{Deref, DerefMut};

pub(crate) struct RetainedGraph<T>(Option<T>);

impl<T> RetainedGraph<T> {
    pub(crate) fn new(owner: T) -> Self { Self(Some(owner)) }

    /// Transfer to another retained owner, or explicitly retire after checked
    /// native completion. Ordinary scope exit never releases the graph.
    pub(crate) fn transfer(mut self) -> T { self.0.take().expect("retained graph moved once") }
}

impl<T> Deref for RetainedGraph<T> {
    type Target = T;
    fn deref(&self) -> &T { self.0.as_ref().expect("retained graph is live") }
}
impl<T> DerefMut for RetainedGraph<T> {
    fn deref_mut(&mut self) -> &mut T { self.0.as_mut().expect("retained graph is live") }
}
impl<T> Drop for RetainedGraph<T> {
    fn drop(&mut self) {
        if let Some(owner) = self.0.take() { std::mem::forget(owner); }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, atomic::{AtomicUsize, Ordering}};
    struct Observe(Arc<AtomicUsize>);
    impl Drop for Observe { fn drop(&mut self) { self.0.fetch_add(1, Ordering::SeqCst); } }

    #[test]
    fn panic_after_publication_keeps_native_graph_original_control_and_pool_owner() {
        let drops = Arc::new(AtomicUsize::new(0));
        let driver = Arc::new(Observe(Arc::clone(&drops)));
        let original = Arc::new(Observe(Arc::clone(&drops)));
        let binding = Arc::new(Observe(Arc::clone(&drops)));
        let graph = Arc::new((Arc::clone(&driver), Arc::clone(&original), Arc::clone(&binding)));
        let weak = Arc::downgrade(&graph);
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
            let _published_owner = RetainedGraph::new(Arc::clone(&graph));
            drop(graph);
            panic!("injected panic after native graph publication");
        }));
        assert!(result.is_err());
        drop((driver, original, binding));
        assert!(weak.upgrade().is_some(), "the full native owner graph survives unwinding");
        assert_eq!(drops.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn explicit_completed_transfer_can_release_the_full_graph() {
        let drops = Arc::new(AtomicUsize::new(0));
        let owner = RetainedGraph::new((Observe(Arc::clone(&drops)), Observe(Arc::clone(&drops))));
        assert_eq!(drops.load(Ordering::SeqCst), 0);
        drop(owner.transfer());
        assert_eq!(drops.load(Ordering::SeqCst), 2);
    }
}
