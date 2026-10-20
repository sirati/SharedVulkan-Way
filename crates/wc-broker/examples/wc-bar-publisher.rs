//! Isolated single-UID Linux product fixture, NOT protected multi-user grants.
//! Retains original resource2 and publishes a full-offset-zero broker lease.
//! Never initializes headers, allocates BAR bytes, signs, or reads install keys.
#[cfg(target_os = "linux")]
fn main() -> std::io::Result<()> {
    use std::{io::{self,Write},os::{fd::{AsRawFd,OwnedFd},unix::fs::{MetadataExt,OpenOptionsExt,FileTypeExt}},path::PathBuf};
    use wc_broker::{Allow,DeviceSpec,Release,User,server::{Broker,Config},sys::unix::{Unix,DevicePath,Object}};
    fn bad(s:&str)->io::Error {io::Error::new(io::ErrorKind::InvalidInput,s)}
    let mut args=std::collections::BTreeMap::new();let mut input=std::env::args().skip(1);
    while let Some(key)=input.next(){if !["--socket","--bar","--uid","--pool","--run","--bell","--receipt"].contains(&key.as_str()){return Err(bad("unknown option"));}let value=input.next().ok_or_else(||bad("option requires value"))?;if args.insert(key,value).is_some(){return Err(bad("duplicate option"));}}
    let get=|key:&str|args.get(key).cloned().ok_or_else(||bad("required options: --socket --bar --uid --pool --run --bell --receipt"));
    if unsafe{libc::geteuid()}!=0{return Err(bad("fixture publisher requires root"));}
    let socket=PathBuf::from(get("--socket")?);let bar=PathBuf::from(get("--bar")?);let bell=PathBuf::from(get("--bell")?);
    let receipt=PathBuf::from(get("--receipt")?);let pool=get("--pool")?;
    let uid:u32=get("--uid")?.parse().map_err(|_|bad("invalid UID"))?;
    let run:u64=get("--run")?.parse().map_err(|_|bad("invalid run"))?;
    if uid==0||run==0||wc_broker::proto::Request::parse(&format!("find {pool}")).is_err(){return Err(bad("invalid non-root fixture UID/pool/run"));}
    let parent=socket.parent().ok_or_else(||bad("socket has no parent"))?;
    let m=std::fs::symlink_metadata(parent)?;
    if !m.is_dir()||m.uid()!=0||m.mode()&0o022!=0||socket.exists(){return Err(bad("socket parent must be root-owned, not writable by others; socket must be absent"));}
    // Fixed sysfs PCI resource, no arbitrary regular file/raw-device alias.
    let canonical=std::fs::canonicalize(&bar)?;
    if canonical.file_name()!=Some(std::ffi::OsStr::new("resource2"))||!canonical.starts_with("/sys/devices"){return Err(bad("bar must resolve to original sysfs PCI resource2"));}
    let device=canonical.parent().ok_or_else(||bad("PCI parent absent"))?;
    if std::fs::read_to_string(device.join("vendor"))?.trim()!="0x1af4"||std::fs::read_to_string(device.join("device"))?.trim()!="0x1110"{return Err(bad("unexpected PCI device identity"));}
    let file=std::fs::OpenOptions::new().read(true).write(true).custom_flags(libc::O_NOFOLLOW|libc::O_CLOEXEC).open(&canonical)?;
    let metadata=file.metadata()?;const BYTES:u64=128<<20;
    if metadata.uid()!=0||metadata.mode()&0o077!=0||metadata.len()!=BYTES{return Err(bad("original BAR must be root-only 128MiB"));}
    // Read actual mapped MMIO header, never pread and never write the BAR.
    let map=unsafe{libc::mmap(std::ptr::null_mut(),4096,libc::PROT_READ,libc::MAP_SHARED,file.as_raw_fd(),0)};
    if map==libc::MAP_FAILED{return Err(io::Error::last_os_error());}
    let mut bytes=[0u8;wc_handle::ivshmem::ENCODED_LEN];
    for(i,b)in bytes.iter_mut().enumerate(){*b=unsafe{std::ptr::read_volatile(map.cast::<u8>().add(i))};}
    std::sync::atomic::fence(std::sync::atomic::Ordering::Acquire);
    let mut again=[0u8;wc_handle::ivshmem::ENCODED_LEN];
    for(i,b)in again.iter_mut().enumerate(){*b=unsafe{std::ptr::read_volatile(map.cast::<u8>().add(i))};}
    unsafe{libc::munmap(map,4096);}
    if bytes!=again{return Err(bad("header changed during snapshot"));}
    let header=wc_handle::ivshmem::Header::decode(&bytes)?;
    if header.run_id!=run||header.region_len!=BYTES||header.arena_id!=0||header.generation==0||header.rings.1==0{return Err(bad("header run/size/arena/generation/rings differs"));}
    let bell_actual=std::fs::canonicalize(&bell)?;let bm=std::fs::metadata(&bell_actual)?;
    if bell!=PathBuf::from("/dev/virtio-ports/org.sirati.wcbell.0")||!bell_actual.starts_with("/dev")||!bm.file_type().is_char_device()||bm.uid()!=0||bm.mode()&0o077!=0{return Err(bad("wake port must be original root-only /dev object"));}
    // Atomically refuse replacing an existing receipt before opening socket.
    let mut output=std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&receipt)?;
    let platform=Unix::bind(&socket,vec![DevicePath{name:"wcbell".into(),path:bell_actual}])?;
    let broker=Broker::new(platform,Config{devices:vec![DeviceSpec{name:"wcbell".into(),allow:Allow::Users(vec![User::uid(uid)])}],services:vec![],keys:None,release:Release::OwnerFree,budget:Some(0),run});
    let id=broker.publish(User::uid(uid),&pool,header.generation,vec![(Object::Device(OwnedFd::from(file)),0,BYTES)]);
    let line=format!("{{\"region\":{id},\"broker_run\":{run},\"epoch\":{},\"pieces\":[[0,{BYTES}]],\"host_run\":{run},\"arena_id\":0,\"uid\":{uid},\"physical_dev\":{},\"physical_inode\":{}}}\n",header.generation,metadata.dev(),metadata.ino());
    output.write_all(line.as_bytes())?;output.sync_all()?;print!("{line}");io::stdout().flush()?;
    broker.serve()
}
#[cfg(not(target_os = "linux"))]
fn main(){eprintln!("wc-bar-publisher is a Linux isolated fixture only");std::process::exit(2);}
