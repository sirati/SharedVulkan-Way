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

    struct Fixture {
        boot: Arc<sv_transport::region::SharedRegion>,
        server: Arc<crate::pool::PoolServer>,
        binding: crate::pool::PoolBinding,
        control_region: Arc<sv_transport::region::SharedRegion>,
        held_free: u64,
    }
    fn fixture() -> Fixture {
        use sv_transport::region::SharedRegion;
        use sv_arena::pool::{PoolConfig, Purpose};
        let boot = Arc::new(SharedRegion::heap(64 << 20).unwrap());
        boot.write_bytes((64 << 20) - 8, b"ORIGINAL").unwrap();
        let server = crate::pool::PoolServer::new(PoolConfig::for_capacity(64 << 20),
            Arc::clone(&boot), crate::pool::Importer::HostPointer).unwrap();
        let binding = server.open_session("retained graph fixture");
        let lease = server.lease_raw(binding.session, crate::pool_guest::geometry().control_size, Purpose::Control).unwrap();
        let (_, offset) = sv_arena::pool::split(lease.offset);
        // SAFETY: this view is the actual leased window; the retained binding
        // keeps the original boot mapping alive throughout this test.
        let control_region = Arc::new(unsafe {
            SharedRegion::from_raw(boot.base_ptr().add(offset as usize), lease.len as usize)
        });
        let held_free = server.capacity_and_free().1;
        Fixture { boot, server, binding, control_region, held_free }
    }

    #[test]
    fn setup_error_after_control_create_keeps_original_mapping_and_lease_owner() {
        let fixture = fixture();
        let original = Arc::downgrade(&fixture.boot);
        let pool = Arc::downgrade(&fixture.server);
        let mapping = Arc::downgrade(&fixture.control_region);
        let expected_free = fixture.held_free;
        let drops = Arc::new(AtomicUsize::new(0));
        let control_record = std::sync::Mutex::new(None);
        let result = (|| -> Result<(), &'static str> {
            let Fixture { boot, server, binding, control_region, held_free: _ } = fixture;
            let region_owner = RetainedGraph::new(control_region);
            let _binding_owner = RetainedGraph::new(binding);
            drop(server);
            let _driver = RetainedGraph::new(Arc::new(Observe(Arc::clone(&drops))));
            let _ordinary_arena = crate::arena::map_lazy(boot);
            let control = RetainedGraph::new(Arc::new(sv_transport::control::Control::create(
                Arc::clone(&region_owner), crate::pool_guest::geometry(), crate::protocol_ids(), 1).unwrap()));
            *control_record.lock().unwrap() = Some(Arc::downgrade(&control));
            Err("injected setup error after Control::create")
        })();
        assert!(result.is_err());
        assert!(control_record.lock().unwrap().as_ref().unwrap().upgrade().is_some());
        assert!(mapping.upgrade().is_some());
        let original = original.upgrade().expect("ordinary Arena drop must not unmap original pages");
        let mut bytes = [0; 8];
        original.read_bytes((64 << 20) - 8, &mut bytes).unwrap();
        assert_eq!(&bytes, b"ORIGINAL");
        assert_eq!(pool.upgrade().unwrap().capacity_and_free().1, expected_free);
        assert_eq!(drops.load(Ordering::SeqCst), 0, "native driver destructor must not run");
    }

    #[test]
    fn panic_after_publication_keeps_driver_shared_control_and_original_pool_graph() {
        struct NativeShared {
            _driver: Arc<Observe>,
            _arena: crate::arena::Arena,
            _binding: crate::pool::PoolBinding,
            drops: Arc<AtomicUsize>,
        }
        impl Drop for NativeShared { fn drop(&mut self) { self.drops.fetch_add(1, Ordering::SeqCst); } }
        let fixture = fixture();
        let original = Arc::downgrade(&fixture.boot);
        let pool = Arc::downgrade(&fixture.server);
        let mapping = Arc::downgrade(&fixture.control_region);
        let expected_free = fixture.held_free;
        let driver_drops = Arc::new(AtomicUsize::new(0));
        let shared_drops = Arc::new(AtomicUsize::new(0));
        let driver_count = Arc::clone(&driver_drops);
        let shared_count = Arc::clone(&shared_drops);
        let published = AtomicUsize::new(0);
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let Fixture { boot, server, binding, control_region, held_free: _ } = fixture;
            let region = RetainedGraph::new(control_region);
            let _binding = RetainedGraph::new(binding.clone());
            drop(server);
            let driver = RetainedGraph::new(Arc::new(Observe(driver_count)));
            let _control = RetainedGraph::new(Arc::new(sv_transport::control::Control::create(
                Arc::clone(&region), crate::pool_guest::geometry(), crate::protocol_ids(), 1).unwrap()));
            let shared = Arc::new(NativeShared { _driver: Arc::clone(&driver),
                _arena: crate::arena::map_lazy(boot), _binding: binding, drops: shared_count });
            let _native_owner = RetainedGraph::new(Arc::clone(&shared));
            published.store(1, Ordering::SeqCst);
            panic!("injected panic after native graph publication");
        }));
        assert!(result.is_err());
        assert_eq!(published.load(Ordering::SeqCst), 1);
        assert!(original.upgrade().is_some());
        assert!(mapping.upgrade().is_some());
        assert_eq!(pool.upgrade().unwrap().capacity_and_free().1, expected_free);
        assert_eq!(driver_drops.load(Ordering::SeqCst), 0);
        assert_eq!(shared_drops.load(Ordering::SeqCst), 0);
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
