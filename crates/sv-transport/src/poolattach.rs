//! Validate an offer from the GPU pool owner, regardless of which OS owns it.
//! The claimant must finish these checks before sending Accept. Liveness and
//! atomic mailbox claiming remain the common `poolcold::host_attach` protocol.
use crate::cold::{Granularity, Hello, Locator};
use crate::control::{Layout, ProtocolIds};
use crate::poolhdr::{HEADER_LEN, PoolHeader};
use crate::region::{PAGE, SharedRegion};
use std::sync::Arc;

/// Validate the nonce, protocol, geometry, range and every control-page stamp.
/// Return a slice retaining the full pool mapping and its external owner.
pub fn check(pool: &Arc<SharedRegion>, hello: &Hello, ids: ProtocolIds) -> Result<Arc<SharedRegion>, String> {
    hello.check_against(ids).map_err(|e| e.to_string())?;
    let Locator::IvshmemPool { control_offset, pool_nonce } = hello.extent.locator else {
        return Err("the GPU owner did not offer a pooled ivshmem session".into());
    };
    let header = PoolHeader::new(pool).map_err(|e| e.to_string())?.snapshot();
    if !header.valid || header.nonce != pool_nonce || pool_nonce == 0 {
        return Err("the offer belongs to another or invalid pool nonce".into());
    }
    let len = hello.extent.control_pages.checked_mul(PAGE as u64).ok_or("control length overflows")?;
    if len == 0 || len > u64::from(u32::MAX) || hello.extent.run_id == 0
        || hello.extent.granularity != Granularity::Small4K
        || hello.extent.arena_pages != 0 || control_offset < HEADER_LEN {
        return Err("a pool offer must have a control region outside the header and no private arena".into());
    }
    if hello.generation == 0 || hello.geometry.control_size != len {
        return Err("the offer's control geometry or generation is invalid".into());
    }
    // Bound the multiplicative layout inputs before Layout::compute. This
    // avoids overflow on an untrusted offer with maximal u32 ring fields.
    let ring_bytes = u64::from(hello.geometry.ring_slot_count)
        .checked_mul(u64::from(hello.geometry.ring_data_bytes)).and_then(|n| n.checked_mul(2));
    if ring_bytes.is_none_or(|n| n > len)
        || u64::from(hello.geometry.ring_slot_count) > len / PAGE as u64
        || u64::from(hello.geometry.timeline_count) > len / 64 {
        return Err("the offer's ring or timeline geometry exceeds its control region".into());
    }
    Layout::compute(hello.geometry).map_err(|e| e.to_string())?;
    let control = Arc::new(pool.slice(control_offset, len).map_err(|e| e.to_string())?);
    for i in 0..hello.extent.control_pages {
        let mut stamp = [0u8; 20];
        control.read_bytes(i * PAGE as u64, &mut stamp).map_err(|e| e.to_string())?;
        if &stamp[..8] != arena_record::MAGIC
            || u64::from_le_bytes(stamp[8..16].try_into().unwrap()) != hello.extent.run_id
            || u64::from(u32::from_le_bytes(stamp[16..20].try_into().unwrap())) != i
        {
            return Err(format!("control page {i} does not carry the offered run and index"));
        }
    }
    // Detect a pool-owner restart during validation as well as before it.
    let after = PoolHeader::new(pool).map_err(|e| e.to_string())?.snapshot();
    if !after.valid || after.nonce != header.nonce {
        return Err("the GPU pool owner restarted while validating the offer".into());
    }
    Ok(control)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cold::{ExtentDesc, Granularity};
    use crate::control::{Control, ENDIAN_MARKER, Geometry};
    use crate::{coldpage, poolcold};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    fn ids() -> ProtocolIds {
        ProtocolIds::new(1, 7)
    }

    fn fixture() -> (Arc<SharedRegion>, Hello) {
        let pool = Arc::new(SharedRegion::heap(8 << 20).unwrap());
        PoolHeader::new(&pool).unwrap().publish(123);
        let hello = Hello {
            ids: ids(), generation: 1,
            extent: ExtentDesc { run_id: 456, granularity: Granularity::Small4K,
                control_pages: (1 << 20) / PAGE as u64, arena_pages: 0,
                locator: Locator::IvshmemPool { control_offset: HEADER_LEN, pool_nonce: 123 } },
            geometry: Geometry::small(), endianness: ENDIAN_MARKER, pointer_width: 64,
        };
        let control = pool.slice(HEADER_LEN, 1 << 20).unwrap();
        for i in 0..hello.extent.control_pages {
            control.write_bytes(i * PAGE as u64, arena_record::MAGIC).unwrap();
            control.write_bytes(i * PAGE as u64 + 8, &456u64.to_le_bytes()).unwrap();
            control.write_bytes(i * PAGE as u64 + 16, &(i as u32).to_le_bytes()).unwrap();
        }
        (pool, hello)
    }

    #[test]
    fn validates_nonce_geometry_bounds_and_every_stamp() {
        let (pool, hello) = fixture();
        assert!(check(&pool, &hello, ids()).is_ok());
        for bad in 0..7 {
            let mut h = hello.clone();
            match bad {
                0 => h.extent.locator = Locator::IvshmemPool { control_offset: HEADER_LEN, pool_nonce: 124 },
                1 => h.extent.locator = Locator::IvshmemPool { control_offset: 0, pool_nonce: 123 },
                2 => h.extent.locator = Locator::IvshmemPool { control_offset: 8 << 20, pool_nonce: 123 },
                3 => h.geometry.control_size += PAGE as u64,
                4 => h.extent.control_pages = u64::MAX,
                5 => h.extent.arena_pages = 1,
                6 => h.ids.wire_version += 1,
                _ => unreachable!(),
            }
            assert!(check(&pool, &h, ids()).is_err(), "bad case {bad}");
        }
        pool.write_bytes(HEADER_LEN + (255 * PAGE as u64) + 16, &0u32.to_le_bytes()).unwrap();
        assert!(check(&pool, &hello, ids()).err().unwrap().contains("page 255"));
    }

    #[test]
    fn a_slice_retains_the_external_device_owner_after_parent_drop() {
        struct Owner { region: SharedRegion, drops: Arc<AtomicUsize> }
        impl Drop for Owner { fn drop(&mut self) { self.drops.fetch_add(1, Ordering::SeqCst); } }
        let drops = Arc::new(AtomicUsize::new(0));
        let owner = Arc::new(Owner { region: SharedRegion::heap(2 * PAGE).unwrap(), drops: Arc::clone(&drops) });
        let parent = Arc::new(unsafe { SharedRegion::from_raw_owned(owner.region.base_ptr(), 2 * PAGE, owner.clone()) });
        let slice = parent.slice(PAGE as u64, PAGE as u64).unwrap();
        drop(parent);
        drop(owner);
        slice.write_bytes(0, b"still owned").unwrap();
        assert!(slice.equals(0, b"still owned").unwrap());
        assert_eq!(drops.load(Ordering::SeqCst), 0);
        drop(slice);
        assert_eq!(drops.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn claimant_validates_before_accept_and_attaches_published_control() {
        let (pool, hello) = fixture();
        let mb = Arc::new(poolcold::Mailboxes::new(Arc::clone(&pool)).unwrap());
        coldpage::offer(mb.page(0), "host-gpu", &hello).unwrap();
        let owner_mb = Arc::clone(&mb);
        let owner_pool = Arc::clone(&pool);
        let g = hello.geometry;
        let owner = std::thread::spawn(move || {
            loop {
                coldpage::beat(owner_mb.page(0)).unwrap();
                if let Some(reply) = coldpage::poll_reply(owner_mb.page(0)).unwrap() {
                    assert!(matches!(reply, crate::cold::ColdMessage::Accept { generation: 1 }));
                    let region = Arc::new(owner_pool.slice(HEADER_LEN, g.control_size).unwrap());
                    let control = Control::create(region, g, ids(), 1).unwrap();
                    assert!(coldpage::transition(owner_mb.page(0), coldpage::STATE_REPLIED, poolcold::STATE_PUBLISHED).unwrap());
                    return control;
                }
                std::thread::sleep(Duration::from_millis(1));
            }
        });
        let attached = poolcold::host_attach(&mb, &poolcold::Liveness::new(), &poolcold::ColdEvent::new(), &|| {}, 789,
            poolcold::HostTiming::default(), |h| check(&pool, h, ids()),
            |_| Control::attach(Arc::new(pool.slice(HEADER_LEN, g.control_size).unwrap()), ids()).is_ok()).unwrap();
        assert_eq!(Control::attach(attached.value, ids()).unwrap().layout().geometry, g);
        owner.join().unwrap();
    }
}
