//! A Windows application consuming a GPU-owner pool across the VM boundary.
//! `SV_SOCKET=ivshmem:0x18` selects this path explicitly. The local named-pipe
//! path remains separate. One process maps a given BAR once and shares its
//! mapping and virtio-serial reader between all of its Vulkan sessions.
use super::*;
use std::collections::BTreeMap;
use std::sync::OnceLock;
use sv_transport::poolcold::{self, ColdEvent, Liveness, Mailboxes};
use sv_transport::vdoorbell::{ControlSet, Doorman, PortHandle};

struct Pool {
    region: Arc<SharedRegion>,
    mailboxes: Arc<Mailboxes>,
    live: Arc<Liveness>,
    cold: Arc<ColdEvent>,
    controls: Arc<ControlSet>,
    _doorman: Option<Doorman>,
}

fn open(slot: u8) -> Result<Arc<Pool>, String> {
    static POOLS: OnceLock<Mutex<BTreeMap<u8, Arc<Pool>>>> = OnceLock::new();
    let mut pools = POOLS.get_or_init(Default::default).lock().map_err(|_| "ivshmem pool registry poisoned")?;
    if let Some(pool) = pools.get(&slot) { return Ok(Arc::clone(pool)); }
    // Never bypass the privileged broker on this application path. Its SID
    // ownership and device drain rules apply before raw BAR access is lent.
    let name = wc_broker::client::lend::ivshmem_name(slot).ok_or("the broker has no ivshmem device configured at this slot")?;
    let device = wc_broker::client::lend::ivshmem(&name, "vulkan-client")
        .ok_or("the broker refused ivshmem access for this Windows Vulkan client")?;
    let (view, size, _) = wc_handle::map_ivshmem_device(device).map_err(|e| format!("mapping the lent BAR: {e}"))?;
    let len = usize::try_from(size).map_err(|_| "ivshmem size does not fit the process")?;
    let owner = Arc::new(view);
    // SAFETY: MappedView owns the device and mapping. Each region slice retains
    // this owner, independently of registry/session destruction order.
    let region = Arc::new(unsafe { SharedRegion::from_raw_owned(owner.ptr().cast_mut(), len, owner.clone()) });
    let mailboxes = Arc::new(Mailboxes::new(Arc::clone(&region)).map_err(|e| e.to_string())?);
    let cold = ColdEvent::new();
    let controls = ControlSet::new();
    let doorman = match std::env::var(crate::BELL_ENV) {
        Ok(spec) => {
            let pipe = wc_broker::client::lend::port(&spec, "vulkan-client")
                .ok_or("the broker refused the Windows Vulkan client's doorbell port")?;
            let port = PortHandle::Pipe(Arc::new(pipe));
            Some(poolcold::attach_guest_bell_for(port, Arc::clone(&controls), Arc::clone(&cold),
                unsafe { region.base_ptr() } as usize, len).map_err(|e| format!("attaching the guest doorbell: {e}"))?)
        }
        Err(_) => None,
    };
    let live = Liveness::new();
    poolcold::spawn_liveness(Arc::clone(&mailboxes), Arc::clone(&live)).map_err(|e| e.to_string())?;
    let pool = Arc::new(Pool { region, mailboxes, live, cold, controls, _doorman: doorman });
    pools.insert(slot, Arc::clone(&pool));
    Ok(pool)
}

pub(super) fn connect(slot: &str) -> Result<Attached, String> {
    let slot = match slot.strip_prefix("0x") {
        Some(hex) => u8::from_str_radix(hex, 16),
        None => slot.parse(),
    }.map_err(|_| "ivshmem client slot must be a decimal or 0x-prefixed PCI slot")?;
    let pool = open(slot)?;
    let base = unsafe { pool.region.base_ptr() } as usize;
    let nonce = (std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)
        .map_err(|e| e.to_string())?.as_nanos() as u64 ^ u64::from(std::process::id())) | 1;
    let attachment = poolcold::host_attach(&pool.mailboxes, &pool.live, &pool.cold,
        &|| poolcold::poke_cross_at(base), nonce, poolcold::HostTiming::default(),
        |hello| sv_transport::poolattach::check(&pool.region, hello, crate::protocol_ids()),
        |hello| {
            let sv_transport::cold::Locator::IvshmemPool { control_offset, .. } = hello.extent.locator else { return false };
            pool.region.slice(control_offset, hello.geometry.control_size).ok()
                .is_some_and(|r| Control::attach(Arc::new(r), crate::protocol_ids()).is_ok())
        })?;
    let snapshot = sv_transport::poolhdr::PoolHeader::new(&pool.region).map_err(|e| e.to_string())?.snapshot();
    let sv_transport::cold::Locator::IvshmemPool { pool_nonce, .. } = attachment.hello.extent.locator else { unreachable!() };
    if !snapshot.valid || snapshot.nonce != pool_nonce {
        return Err("the GPU pool owner restarted during attach".into());
    }
    // The owner has replaced the checked stamps with a published Control. Its
    // header must still match the accepted offer, including the generation.
    let control = Control::attach(Arc::clone(&attachment.value), crate::protocol_ids()).map_err(|e| e.to_string())?;
    if control.layout().geometry != attachment.hello.geometry || control.generation() != attachment.hello.generation {
        return Err("published control differs from the accepted offer".into());
    }
    let mut attached = RingTransport::finish_attach(attachment.value, Arc::clone(&pool.region), pool.region.len() as u64)?;
    pool.controls.add(&attached.transport.control);
    attached.enter_bar_pool()?;
    Ok(attached)
}

impl Attached {
    fn enter_bar_pool(&mut self) -> Result<(), String> {
        let t = &mut self.transport;
        let info = t.arena_info(&format!("windows pid {}", std::process::id())).ok_or("the GPU owner did not answer arena mode")?;
        if info.mode != sv_proto::sv::ARENA_MODE_LEASED || info.capacity > t.arena.len() as u64
            || info.lease_granule == 0 || !info.lease_granule.is_power_of_two()
            || info.session_cap == 0 || info.session_cap > info.capacity {
            return Err("the GPU pool must be leased and fit entirely in this BAR; grown host regions are unsupported".into());
        }
        t.pool = Some(PoolView {
            source: PoolSource::None,
            mapped: Mutex::new(vec![(0, t.arena.len() as u64)]),
            ram: Mutex::new(BTreeMap::new()),
            granule: info.lease_granule, session_cap: info.session_cap, capacity: info.capacity,
        });
        let (off, len, _) = t.lease_arena(sv_arena::pool::BLOB_LEASE, sv_proto::sv::LEASE_BLOBS)
            .map_err(|e| format!("leasing the command-blob reserve: {e:?}"))?;
        *t.blobs.lock().unwrap_or_else(|e| e.into_inner()) = Blobs::new(off, len);
        self.arena_len = t.arena.len() as u64;
        self.slab_size = self.arena_len;
        Ok(())
    }
}
