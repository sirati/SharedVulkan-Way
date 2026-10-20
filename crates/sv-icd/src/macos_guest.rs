//! Intel macOS application attachment to a GPU-owner PCI pool. All leases,
//! mappings, and doorbells go through the root broker; no raw IOKit handles
//! reach the application. The shared mailbox/offer format is unchanged.
use super::*;
use std::collections::BTreeMap;
use std::sync::{atomic::AtomicU64, OnceLock};
use sv_transport::poolcold::{self, ColdEvent, Liveness, Mailboxes};
use sv_transport::vdoorbell::{BellTarget, ControlSet};

struct Pool {
    region: Arc<SharedRegion>,
    mailboxes: Arc<Mailboxes>,
    live: Arc<Liveness>,
    cold: Arc<ColdEvent>,
    controls: Arc<ControlSet>,
}
struct Wake(std::sync::mpsc::SyncSender<()>);
impl sv_transport::park::CrossWake for Wake {
    fn poke(&self, _token: u8) {
        let _ = self.0.try_send(());
    }
}
fn open(name: &str) -> Result<Arc<Pool>, String> {
    static POOLS: OnceLock<Mutex<BTreeMap<String, Arc<Pool>>>> = OnceLock::new();
    let mut pools = POOLS
        .get_or_init(Default::default)
        .lock()
        .map_err(|_| "PCI pool registry poisoned")?;
    if let Some(pool) = pools.get(name) {
        return Ok(Arc::clone(pool));
    }
    let bell = std::env::var(crate::BELL_ENV)
        .map_err(|_| "macOS PCI attachment requires SV_BELL=pci:PEER:VECTOR")?;
    let (peer, vector) = bell
        .strip_prefix("pci:")
        .and_then(|s| s.split_once(':'))
        .ok_or("expected SV_BELL=pci:PEER:VECTOR")?;
    let peer = peer.parse::<u16>().map_err(|_| "invalid PCI peer")?;
    let vector = vector.parse::<u16>().map_err(|_| "invalid PCI vector")?;
    let mut client = wc_broker::client::Client::connect(&wc_broker::client::path())
        .map_err(|e| e.to_string())?;
    client.hello("vulkan-client").map_err(|e| e.to_string())?;
    let owner = Arc::new(client.find_mach(name).map_err(|e| e.to_string())?);
    let mut seen = owner
        .wait_irq(0)
        .map_err(|e| format!("PCI IRQ unavailable: {e}"))?;
    owner
        .ring(peer, vector)
        .map_err(|e| format!("PCI bell refused: {e}"))?;
    let len = owner.len();
    // SAFETY: every region and slice retains the Mach mapping and the ordinary
    // broker connection/lease, independently of session destruction order.
    let region =
        Arc::new(unsafe { SharedRegion::from_raw_owned(owner.as_ptr(), len, owner.clone()) });
    let mailboxes = Arc::new(Mailboxes::new(Arc::clone(&region)).map_err(|e| e.to_string())?);
    let cold = ColdEvent::new();
    let controls = ControlSet::new();
    let (sender, receiver) = std::sync::mpsc::sync_channel(1);
    let weak = Arc::downgrade(&owner);
    std::thread::Builder::new()
        .name("sv-pci-ring".into())
        .spawn(move || {
            while receiver.recv().is_ok() {
                let Some(owner) = weak.upgrade() else { break };
                if owner.ring(peer, vector).is_err() {
                    break;
                }
            }
        })
        .map_err(|e| e.to_string())?;
    let weak = Arc::downgrade(&owner);
    let target = Arc::clone(&controls);
    let cold_event = Arc::clone(&cold);
    cold.reopened();
    std::thread::Builder::new()
        .name("sv-pci-irq".into())
        .spawn(move || {
            let woken = AtomicU64::new(0);
            loop {
                let Some(owner) = weak.upgrade() else { break };
                let Ok(next) = owner.wait_irq(seen) else {
                    break;
                };
                if next != seen {
                    seen = next;
                    target.rescan(&woken);
                    cold_event.ring();
                }
            }
            cold_event.closed();
        })
        .map_err(|e| e.to_string())?;
    sv_transport::park::install_cross_wake_for(
        owner.as_ptr() as usize,
        len,
        Arc::new(Wake(sender)),
    );
    let live = Liveness::new();
    poolcold::spawn_liveness(Arc::clone(&mailboxes), Arc::clone(&live))
        .map_err(|e| e.to_string())?;
    let pool = Arc::new(Pool {
        region,
        mailboxes,
        live,
        cold,
        controls,
    });
    pools.insert(name.into(), Arc::clone(&pool));
    Ok(pool)
}

pub(super) fn connect(name: &str) -> Result<Attached, String> {
    if name.is_empty() {
        return Err("broker-pci requires a published pool name".into());
    }
    let pool = open(name)?;
    let base = unsafe { pool.region.base_ptr() } as usize;
    let nonce = (std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|e| e.to_string())?
        .as_nanos() as u64
        ^ u64::from(std::process::id()))
        | 1;
    let attachment = poolcold::host_attach(
        &pool.mailboxes,
        &pool.live,
        &pool.cold,
        &|| poolcold::poke_cross_at(base),
        nonce,
        poolcold::HostTiming::default(),
        |hello| sv_transport::poolattach::check(&pool.region, hello, crate::protocol_ids()),
        |hello| {
            let sv_transport::cold::Locator::IvshmemPool { control_offset, .. } =
                hello.extent.locator
            else {
                return false;
            };
            pool.region
                .slice(control_offset, hello.geometry.control_size)
                .ok()
                .is_some_and(|r| Control::attach(Arc::new(r), crate::protocol_ids()).is_ok())
        },
    )?;
    let snapshot = sv_transport::poolhdr::PoolHeader::new(&pool.region)
        .map_err(|e| e.to_string())?
        .snapshot();
    let sv_transport::cold::Locator::IvshmemPool { pool_nonce, .. } =
        attachment.hello.extent.locator
    else {
        unreachable!()
    };
    if !snapshot.valid || snapshot.nonce != pool_nonce {
        return Err("the GPU pool owner restarted during attach".into());
    }
    // The owner has replaced the checked stamps with a published Control. Its
    // header must still match the accepted offer, including the generation.
    let control = Control::attach(Arc::clone(&attachment.value), crate::protocol_ids())
        .map_err(|e| e.to_string())?;
    if control.layout().geometry != attachment.hello.geometry
        || control.generation() != attachment.hello.generation
    {
        return Err("published control differs from the accepted offer".into());
    }
    let mut attached = RingTransport::finish_attach(
        attachment.value,
        Arc::clone(&pool.region),
        pool.region.len() as u64,
    )?;
    pool.controls.add(&attached.transport.control);
    attached.enter_macos_bar_pool()?;
    Ok(attached)
}

impl Attached {
    fn enter_macos_bar_pool(&mut self) -> Result<(), String> {
        let t = &mut self.transport;
        let info = t
            .arena_info(&format!("macos pid {}", std::process::id()))
            .ok_or("the GPU owner did not answer arena mode")?;
        if info.mode != sv_proto::sv::ARENA_MODE_LEASED
            || info.capacity > t.arena.len() as u64
            || info.max_capacity > t.arena.len() as u64
            || info.lease_granule == 0
            || !info.lease_granule.is_power_of_two()
            || info.session_cap == 0
            || info.session_cap > info.capacity
        {
            return Err("the GPU pool must be leased and fit entirely in this BAR; grown host regions are unsupported".into());
        }
        t.pool = Some(PoolView {
            source: PoolSource::None,
            mapped: Mutex::new(vec![(0, t.arena.len() as u64)]),
            ram: Mutex::new(BTreeMap::new()),
            granule: info.lease_granule,
            session_cap: info.session_cap,
            capacity: info.capacity,
        });
        let (off, len, _) = t
            .lease_arena(sv_arena::pool::BLOB_LEASE, sv_proto::sv::LEASE_BLOBS)
            .map_err(|e| format!("leasing the command-blob reserve: {e:?}"))?;
        *t.blobs.lock().unwrap_or_else(|e| e.into_inner()) = Blobs::new(off, len);
        self.arena_len = t.arena.len() as u64;
        self.slab_size = self.arena_len;
        Ok(())
    }
}
