//! Original toolkit allocations. The extension chooses pages before Cairo paints.
//! Production pages come from existing authenticated broker region leases.
use super::*;
use std::collections::{HashMap, HashSet};
use wc_native_shm::{Backing, Mapping, same_open_file};

fn verify_original(registered: &crate::pool::NativeBacking, mapping: &Mapping) -> io::Result<()> {
    if mapping.fd_offset() != registered.fd_offset { return Err(io::Error::other("native GTK original backing offset changed")); }
    let same = match &registered.description {
        Backing::Broker { .. } => same_open_file(&registered.original,mapping.fd())?,
        Backing::Fixture { .. } => wc_native_shm::file_metadata(&registered.original)? == wc_native_shm::file_metadata(mapping.fd())?,
    };
    if !same { return Err(io::Error::other("native GTK original registered backing changed")); }
    Ok(())
}

const GLOBAL: u32 = 0xfffe_d4b1;
const INTERFACE: &str = "wc_native_shm_v1";

#[derive(Default)]
pub(super) struct NativeShm {
    enabled: bool,
    shm_global: Option<u32>,
    managers: HashSet<u32>,
    allocations: HashMap<u64, Allocation>,
    pools: HashMap<u32, u64>,
    buffers: HashMap<u32, u64>,
    deleting: HashSet<u32>,
}
struct Allocation {
    region: Region,
    mapping: Mapping,
    fixture: bool,
    pool_created: bool,
    pool_destroyed: bool,
    cairo_released: bool,
    buffers: HashSet<u32>,
    held: HashSet<u32>,
    // A buffer destroyed before the compositor released it cannot be proved
    // drained by delete_id. Never recycle that allocation on a guessed drain.
    unproved_drain: bool,
}

impl NativeShm {
    pub(super) fn new() -> Self {
        Self { enabled: std::env::var_os("WC_NATIVE_SHM").is_some_and(|v| v == "1"), ..Self::default() }
    }
    pub(super) fn quarantined(&self) -> (usize, u64) {
        (self.allocations.len(), self.allocations.values().map(|a| a.region.size).sum())
    }
    pub(super) fn contains_buffer(&self, id: u32) -> bool { self.buffers.contains_key(&id) }
    pub(super) fn committed(&mut self, id: u32) {
        if let Some(a) = self.buffers.get(&id).and_then(|key| self.allocations.get_mut(key)) { a.held.insert(id); }
    }
}

impl Session {
    pub(super) fn native_registry(&mut self, interface: &str, object: u32, opcode: u16,
        body: &[u8], spans: &[Span], out: &mut Vec<u8>) {
        if !self.native_shm.enabled || interface != "wl_registry" || opcode != 0 { return; }
        let name = spans.first().and_then(|s| wire::word_at(body,s)).unwrap_or(0);
        let iface = spans.get(1).and_then(|s| wire::string_at(body,s)).unwrap_or_default();
        if name == GLOBAL { out.clear(); return; }
        if iface == "wl_shm" {
            self.native_shm.shm_global = Some(name);
            let mut b = GLOBAL.to_ne_bytes().to_vec();
            wire::push_string(&mut b, INTERFACE);
            b.extend_from_slice(&1u32.to_ne_bytes());
            out.extend_from_slice(&wire::message(object, 0, &b));
        }
    }

    pub(super) fn native_event(&mut self, object: u32, opcode: u16) -> bool {
        self.native_shm.managers.contains(&object) && opcode == 0
    }

    fn native_finish(&mut self, key: u64) {
        let done = self.native_shm.allocations.get(&key).is_some_and(|a| {
            a.cairo_released && (!a.pool_created || a.pool_destroyed) && a.buffers.is_empty()
                && a.held.is_empty() && !a.unproved_drain
        });
        if done {
            let a = self.native_shm.allocations.remove(&key).expect("checked allocation");
            // Unmap and end our own broker lease before returning the allocator
            // extent. GTK has independently unmapped and closed its own lease.
            drop(a.mapping);
            self.transport.destroy(&a.region);
        }
    }

    pub(super) fn native_released(&mut self, id: u32) {
        if let Some(key) = self.native_shm.buffers.get(&id).copied()
            && let Some(a) = self.native_shm.allocations.get_mut(&key) { a.held.remove(&id); }
    }
    pub(super) fn native_deleted(&mut self, id: u32) {
        if !self.native_shm.deleting.remove(&id) { return; }
        if let Some(key) = self.native_shm.buffers.remove(&id) {
            if let Some(a) = self.native_shm.allocations.get_mut(&key) {
                a.buffers.remove(&id);
                if a.held.remove(&id) { a.unproved_drain = true; }
            }
            self.native_finish(key);
        }
    }

    pub(super) fn native_request(&mut self, interface: &str, object: u32, opcode: u16,
        body: &[u8], spans: &[Span], fds: &mut std::collections::VecDeque<OwnedFd>) -> Result<Option<Vec<u8>>, Error> {
        if !self.native_shm.enabled { return Ok(None); }
        let word = |i: usize| spans.get(i).and_then(|s| wire::word_at(body,s)).unwrap_or(0);
        if interface == "wl_registry" && opcode == 0 && word(0) == GLOBAL {
            let (iface, version, id) = spans.get(1).and_then(|s| wire::untyped_new_id(body,s))
                .ok_or_else(|| Error::Walk("native shm bind has no new_id".into()))?;
            if iface != INTERFACE || version != 1 { return Err(Error::Walk("invalid native shm binding".into())); }
            let shm = self.native_shm.shm_global.ok_or_else(|| Error::Walk("native shm bound before wl_shm".into()))?;
            self.native_shm.managers.insert(id);
            // Preserve the client's id sequence upstream, as for dmabuf.
            let mut b = shm.to_ne_bytes().to_vec(); wire::push_string(&mut b, "wl_shm");
            b.extend_from_slice(&1u32.to_ne_bytes()); b.extend_from_slice(&id.to_ne_bytes());
            return Ok(Some(wire::message(object,0,&b)));
        }
        if interface == INTERFACE {
            let serial = word(0); let key = ((object as u64) << 32) | serial as u64;
            if opcode == 1 {
                let a = self.native_shm.allocations.get_mut(&key).ok_or_else(|| Error::Walk("unknown native shm release".into()))?;
                if a.cairo_released { return Err(Error::Walk("duplicate native shm release".into())); }
                a.cairo_released = true;
                self.native_finish(key);
                return Ok(Some(Vec::new()));
            }
            if opcode != 0 || serial == 0 || self.native_shm.allocations.contains_key(&key) {
                return Err(Error::Walk("invalid native shm allocation serial".into()));
            }
            let size = word(1) as u64;
            let allocation = (|| -> io::Result<_> {
                if size == 0 || size > i32::MAX as u64 { return Err(io::Error::other("invalid native shm size")); }
                let region = self.transport.create(size)?;
                let result = self.transport.native_backing(&region).and_then(|registered| {
                    let mapping = Mapping::open(&registered.description,size)?;
                    verify_original(&registered, &mapping)?;
                    Ok((registered.description,mapping))
                });
                match result {
                    Ok((backing,mapping)) => Ok((region,backing,mapping)),
                    Err(e) => { self.transport.destroy(&region); Err(e) }
                }
            })();
            match allocation {
                Ok((region,backing,mapping)) => {
                    let fixture = matches!(backing, Backing::Fixture { .. });
                    let (kind,id,run,epoch,offset,path) = match backing {
                        Backing::Broker { id, run, epoch, offset } => (1u32,id,run,epoch,offset,String::new()),
                        Backing::Fixture { path, offset } => (2u32,0,0,0,offset,path.to_string_lossy().into_owned()),
                    };
                    let words = [serial,kind,(id>>32) as u32,id as u32,(run>>32) as u32,run as u32,
                        (epoch>>32) as u32,epoch as u32,(offset>>32) as u32,offset as u32,size as u32];
                    let mut b: Vec<u8> = words.into_iter().flat_map(u32::to_ne_bytes).collect(); wire::push_string(&mut b,&path);
                    self.to_client.extend_from_slice(&wire::message(object,0,&b));
                    self.native_shm.allocations.insert(key,Allocation { region,mapping,fixture,pool_created:false,pool_destroyed:false,
                        cairo_released:false,buffers:HashSet::new(),held:HashSet::new(),unproved_drain:false });
                }
                Err(e) => {
                    let mut b = serial.to_ne_bytes().to_vec(); wire::push_string(&mut b,&e.to_string());
                    self.to_client.extend_from_slice(&wire::message(object,1,&b));
                }
            }
            return Ok(Some(Vec::new()));
        }
        if interface == "wl_shm" && opcode == op::SHM_CREATE_POOL && !self.native_shm.managers.is_empty() {
            let fd = fds.front().ok_or_else(|| Error::Walk("native shm pool has no fd".into()))?;
            let mut matched = None;
            for (key,a) in &self.native_shm.allocations {
                if a.pool_created || a.cairo_released { continue; }
                let same = if a.fixture {
                    // An explicit named-file fixture opens the same regular
                    // inode independently. Unlike anon-inode dma-bufs, a live
                    // regular inode identifies the file's original pages.
                    let file = std::fs::File::from(fd.try_clone()?);
                    file.metadata()?.file_type().is_file()
                        && wc_native_shm::file_metadata(a.mapping.fd())? == wc_native_shm::file_metadata(fd)?
                } else { same_open_file(a.mapping.fd(),fd)? };
                if same { matched=Some(*key); break; }
            }
            if let Some(key) = matched {
                let pool = word(0); let size = word(2) as u64;
                let a = self.native_shm.allocations.get_mut(&key).expect("matched allocation");
                if a.mapping.fd_offset().checked_add(a.region.size) != Some(size) {
                    return Err(Error::Walk("native shm pool size differs from its registered backing".into()));
                }
                a.pool_created = true; self.native_shm.pools.insert(pool,key);
                fds.pop_front();
                return Ok(Some(create_pool_message(object,pool,&a.region.descriptor,a.region.size as u32)));
            }
            return Err(Error::Walk("native GTK pool fd is not a registered original backing".into()));
        }
        if interface == "wl_shm_pool" && let Some(key) = self.native_shm.pools.get(&object).copied() {
            if opcode == op::POOL_DESTROY {
                self.native_shm.pools.remove(&object);
                self.native_shm.allocations.get_mut(&key).expect("live native pool").pool_destroyed = true;
                self.native_finish(key);
                return Ok(Some(wire::message(object,opcode,body)));
            }
            if opcode != op::POOL_CREATE_BUFFER { return Err(Error::Walk("native shm pool cannot resize or change its allocation".into())); }
            let id=word(0); let offset=word(1) as i32; let width=word(2) as i32; let height=word(3) as i32; let stride=word(4) as i32;
            let a=self.native_shm.allocations.get_mut(&key).expect("live native pool");
            let relative=(offset as i64).checked_sub(a.mapping.fd_offset() as i64).filter(|o|*o>=0)
                .ok_or_else(||Error::Walk("native shm buffer precedes its allocation".into()))? as u64;
            let bytes=(height as u64).checked_mul(stride as u64).filter(|_|height>0&&stride>0&&width>0&&(width as u64)*4<=stride as u64)
                .and_then(|n|relative.checked_add(n));
            if bytes.is_none_or(|end|end>a.region.size) || word(5)>1 { return Err(Error::Walk("native shm buffer exceeds its allocation or format".into())); }
            let mut b=body.to_vec(); b[4..8].copy_from_slice(&(relative as u32).to_ne_bytes());
            a.buffers.insert(id); self.native_shm.buffers.insert(id,key); self.foreign_buffers.insert(id);
            return Ok(Some(wire::message(object,opcode,&b)));
        }
        if interface=="wl_buffer" && opcode==op::BUFFER_DESTROY && self.native_shm.buffers.contains_key(&object) {
            self.native_shm.deleting.insert(object); self.foreign_buffers.remove(&object);
            return Ok(Some(wire::message(object,opcode,body)));
        }
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pool::{CopyScope, SharedFile};
    use std::collections::VecDeque;

    fn fixture(tag: &str) -> (Session,std::path::PathBuf) {
        let dir=std::env::temp_dir().join(format!("native-proxy-{tag}-{}",std::process::id()));
        std::fs::create_dir(&dir).unwrap();
        let transport=SharedFile::new_native_fixture(&dir,dir.to_str().unwrap()).unwrap();
        let mut s=Session::new(Spec::builtin().unwrap(),Box::new(transport),CopyScope::Damage,1);
        s.native_shm.enabled=true;
        s.objects.record(2,INTERFACE);
        s.native_shm.managers.insert(2);
        s.objects.record(3,"wl_shm");
        (s,dir)
    }
    fn request(s:&mut Session,object:u32,opcode:u16,words:&[u32],fds:&mut VecDeque<OwnedFd>)->Result<Vec<u8>,Error> {
        let b:Vec<u8>=words.iter().flat_map(|w|w.to_ne_bytes()).collect();
        s.client_message(Header{object,opcode,size:b.len()+8},&b,fds)
    }
    fn allocate(s:&mut Session)->u64 {
        request(s,2,0,&[1,4096],&mut VecDeque::new()).unwrap();
        assert_eq!(s.native_shm.allocations.len(),1);
        (2u64<<32)|1
    }
    fn create(s:&mut Session,key:u64)->Mapping {
        let a=&s.native_shm.allocations[&key];
        let path=match s.transport.native_backing(&a.region).unwrap().description { Backing::Fixture{path,..}=>path,_=>unreachable!() };
        let cairo=Mapping::open(&Backing::Fixture{path,offset:0},4096).unwrap();
        let mut fds=VecDeque::from([cairo.fd().try_clone().unwrap()]);
        let bytes=request(s,3,0,&[4,4096],&mut fds).unwrap();
        assert!(fds.is_empty());
        assert_eq!(Header::parse(&bytes).unwrap().object,3);
        request(s,4,0,&[5,0,16,16,64,0],&mut VecDeque::new()).unwrap();
        cairo
    }
    fn event(s:&mut Session,object:u32,opcode:u16,words:&[u32]) {
        let b:Vec<u8>=words.iter().flat_map(|w|w.to_ne_bytes()).collect();
        s.server_message(Header{object,opcode,size:b.len()+8},&b).unwrap();
    }
    #[test]
    fn allocation_metadata_cannot_replace_the_transports_original_file() {
        let (mut s, dir) = fixture("source-identity");
        let key = allocate(&mut s);
        let a = &s.native_shm.allocations[&key];
        let mut original = s.transport.native_backing(&a.region).unwrap();
        assert!(verify_original(&original, &a.mapping).is_ok());
        let foreign = std::fs::OpenOptions::new().read(true).write(true)
            .create_new(true).open(dir.join("foreign-original")).unwrap();
        foreign.set_len(4096).unwrap();
        original.original = foreign.into();
        assert!(verify_original(&original, &a.mapping).is_err());
        drop(s);
        std::fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn original_cairo_pages_reach_the_peer_and_are_recycled_only_after_both_lifetimes() {
        let(mut s,dir)=fixture("lifetime");let key=allocate(&mut s);let cairo=create(&mut s,key);
        unsafe{ std::ptr::write_volatile(cairo.data().cast::<u32>(),0xff12_3456); }
        let a=&s.native_shm.allocations[&key];
        assert_eq!(unsafe{std::ptr::read_volatile(a.mapping.data().cast::<u32>())},0xff12_3456);
        s.objects.record(6,"wl_surface");
        request(&mut s,6,1,&[5,0,0],&mut VecDeque::new()).unwrap();
        request(&mut s,6,6,&[],&mut VecDeque::new()).unwrap();
        assert_eq!(s.stats.copies,0);
        assert!(s.native_shm.allocations[&key].held.contains(&5));
        event(&mut s,5,0,&[]);
        request(&mut s,5,0,&[],&mut VecDeque::new()).unwrap();
        request(&mut s,4,1,&[],&mut VecDeque::new()).unwrap();
        drop(cairo);
        request(&mut s,2,1,&[1],&mut VecDeque::new()).unwrap();
        assert!(s.transport.published()>0,"Cairo release was incorrectly treated as compositor deletion");
        event(&mut s,1,1,&[5]);
        assert_eq!(s.transport.published(),0);
        assert!(s.native_shm.allocations.is_empty());
        drop(s);std::fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn a_claimed_pool_with_a_foreign_fd_does_not_acquire_the_registered_allocation() {
        use std::os::fd::FromRawFd;
        let(mut s,dir)=fixture("foreign");let key=allocate(&mut s);
        let raw=unsafe{libc::memfd_create(c"foreign-toolkit".as_ptr(),libc::MFD_CLOEXEC)};assert!(raw>=0);
        let fd=unsafe{OwnedFd::from_raw_fd(raw)};assert_eq!(unsafe{libc::ftruncate(fd.as_raw_fd(),4096)},0);
        let mut fds=VecDeque::from([fd]);
        assert!(request(&mut s,3,0,&[4,4096],&mut fds).is_err());
        assert!(!s.native_shm.allocations[&key].pool_created);
        assert!(!s.native_shm.pools.contains_key(&4));
        request(&mut s,2,1,&[1],&mut VecDeque::new()).unwrap();
        drop(s);std::fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn destroy_before_compositor_release_never_guesses_that_the_original_pages_are_drained() {
        let(mut s,dir)=fixture("drain");let key=allocate(&mut s);let cairo=create(&mut s,key);
        s.native_shm.committed(5);
        request(&mut s,5,0,&[],&mut VecDeque::new()).unwrap();
        request(&mut s,4,1,&[],&mut VecDeque::new()).unwrap();
        drop(cairo);request(&mut s,2,1,&[1],&mut VecDeque::new()).unwrap();
        event(&mut s,1,1,&[5]);
        assert!(s.native_shm.allocations[&key].unproved_drain);
        assert!(s.transport.published()>0);
        drop(s);std::fs::remove_dir_all(dir).unwrap();
    }
}
