//! Same-kernel GPU endpoint for Linux ELF clients inside hl. Bootstrap is the
//! existing ColdMessage protocol; graphics data and queues use shared backing.
//! One-byte doorbells wake the existing ring waiters in each host process.
use crate::{
    arena,
    driver::{DeviceChoice, Driver},
    server::Shared,
    session,
    table::Objects,
};
use std::fs::{File, OpenOptions};
use std::io;
use std::os::fd::{AsFd, AsRawFd};
use std::os::unix::{
    fs::{MetadataExt, OpenOptionsExt},
    net::{UnixListener, UnixStream},
};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use sv_transport::{
    cold::{Attachment, ColdMessage, recv, send},
    control::Control,
    park::CrossWake,
    region::SharedRegion,
    vdoorbell::ControlSet,
};

type Error = Box<dyn std::error::Error + Send + Sync>;

#[derive(Default)]
struct Bells(Mutex<Vec<UnixStream>>);
impl CrossWake for Bells {
    fn poke(&self, _token: u8) {
        // A queued rescan covers all publications, including a later send
        // coalesced because the socket buffer is already full.
        let token = sv_transport::park::RESCAN_TOKEN;
        if let Ok(mut peers) = self.0.lock() {
            peers.retain_mut(|p| {
                #[cfg(target_os = "linux")]
                let flags = libc::MSG_DONTWAIT | libc::MSG_NOSIGNAL;
                #[cfg(target_os = "macos")]
                let flags = libc::MSG_DONTWAIT;
                let n =
                    unsafe { libc::send(p.as_raw_fd(), (&token as *const u8).cast(), 1, flags) };
                if n == 1 {
                    return true;
                }
                matches!(
                    io::Error::last_os_error().kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                )
            });
        }
    }
}
fn same_user(s: &UnixStream) -> io::Result<()> {
    #[cfg(target_os = "macos")]
    let uid = {
        let (mut uid, mut gid) = (0, 0);
        if unsafe { libc::getpeereid(s.as_raw_fd(), &mut uid, &mut gid) } != 0 {
            return Err(io::Error::last_os_error());
        }
        uid
    };
    #[cfg(target_os = "linux")]
    let uid = {
        let mut cred = std::mem::MaybeUninit::<libc::ucred>::uninit();
        let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
        if unsafe {
            libc::getsockopt(
                s.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_PEERCRED,
                cred.as_mut_ptr().cast(),
                &mut len,
            )
        } != 0
        {
            return Err(io::Error::last_os_error());
        }
        unsafe { cred.assume_init().uid }
    };
    if uid != unsafe { libc::geteuid() } {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "GPU endpoint peer uid differs",
        ));
    }
    Ok(())
}
fn private_dir(path: &Path) -> io::Result<()> {
    let st = std::fs::symlink_metadata(path)?;
    if !st.is_dir() || st.uid() != unsafe { libc::geteuid() } || st.mode() & 0o077 != 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "runtime must be a private, owned directory",
        ));
    }
    Ok(())
}
struct Endpoint {
    listener: UnixListener,
    path: PathBuf,
    dev: u64,
    inode: u64,
}
impl Endpoint {
    fn incoming(&self) -> std::os::unix::net::Incoming<'_> {
        self.listener.incoming()
    }
    fn bind(path: &str) -> io::Result<Self> {
        let listener = UnixListener::bind(path)?;
        let meta = std::fs::symlink_metadata(path)?;
        Ok(Self {
            listener,
            path: path.into(),
            dev: meta.dev(),
            inode: meta.ino(),
        })
    }
}
impl Drop for Endpoint {
    fn drop(&mut self) {
        if std::fs::symlink_metadata(&self.path)
            .is_ok_and(|m| m.dev() == self.dev && m.ino() == self.inode)
        {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

struct Named {
    path: PathBuf,
    file: File,
}
impl Named {
    fn create(runtime: &Path, sequence: u64, len: u64) -> io::Result<Self> {
        let path = runtime.join(format!("gpu-{}-{sequence}.shared", std::process::id()));
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)?;
        let owner = Self { path, file };
        owner.file.set_len(len)?;
        Ok(owner)
    }
}
impl Drop for Named {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Accepts indefinitely. Its compositor process owns shutdown. Never removes a
/// live socket or replaces another process's endpoint during startup.
pub(super) fn run(args: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    let arg = |key: &str| {
        args.iter()
            .position(|a| a == key)
            .and_then(|i| args.get(i + 1))
            .cloned()
    };
    let runtime = PathBuf::from(arg("--runtime").ok_or("--runtime is required")?);
    private_dir(&runtime)?;
    let socket = arg("--socket").ok_or("--socket is required")?;
    let bell = arg("--bell").ok_or("--bell is required")?;
    if Path::new(&socket).parent() != Some(runtime.as_path())
        || Path::new(&bell).parent() != Some(runtime.as_path())
    {
        return Err("GPU sockets must be directly inside the private runtime directory".into());
    }
    let bytes = arg("--arena").map_or(Ok(64 << 20), |s| s.parse::<u64>())?;
    let geometry = super::geometry_for(bytes);
    let total = geometry
        .control_size
        .checked_add(geometry.arena_size)
        .ok_or("arena size overflow")?;
    let listener = Endpoint::bind(&socket)?;
    let bells = Endpoint::bind(&bell)?;
    // Bind every endpoint before starting workers so name conflicts unwind.
    let relay = Endpoint::bind(&format!("{socket}.relay"))?;
    let controls = ControlSet::new();
    let writers = Arc::new(Bells::default());
    sv_transport::park::install_cross_wake(Some(writers.clone()));
    let set = controls.clone();
    let bell_writers = writers.clone();
    std::thread::Builder::new()
        .name("hl-gpu-bells".into())
        .spawn(move || {
            for incoming in bells.incoming() {
                let Ok(reader) = incoming else { break };
                let _ = attach_bell(reader, &set, &bell_writers);
            }
        })?;
    let (accepted, clients) = std::sync::mpsc::channel();
    let direct = accepted.clone();
    std::thread::Builder::new()
        .name("hl-gpu-accept".into())
        .spawn(move || {
            for incoming in listener.incoming() {
                let Ok(sock) = incoming else { break };
                if direct.send(sock).is_err() {
                    break;
                }
            }
        })?;
    let relay_set = controls.clone();
    std::thread::Builder::new()
        .name("hl-gpu-relay".into())
        .spawn(move || {
            use std::io::Write;
            for incoming in relay.incoming() {
                let Ok(mut request) = incoming else { break };
                if same_user(&request).is_err() {
                    continue;
                }
                let _ = request.set_read_timeout(Some(Duration::from_secs(5)));
                let Ok((tag, fd)) = wc_hyperlinux::local_fd::receive_tag(&request) else {
                    continue;
                };
                let sock = UnixStream::from(fd);
                if same_user(&sock).is_err() {
                    continue;
                }
                let mut kind: libc::c_int = 0;
                let mut len = std::mem::size_of_val(&kind) as libc::socklen_t;
                if unsafe {
                    libc::getsockopt(
                        sock.as_raw_fd(),
                        libc::SOL_SOCKET,
                        libc::SO_TYPE,
                        (&mut kind as *mut libc::c_int).cast(),
                        &mut len,
                    )
                } != 0
                    || kind != libc::SOCK_STREAM
                {
                    continue;
                }
                let accepted = match tag {
                    1 => accepted.send(sock).is_ok(),
                    2 => attach_bell(sock, &relay_set, &writers).is_ok(),
                    _ => false,
                };
                let _ = request.write_all(&[if accepted { 0 } else { 1 }]);
            }
        })?;
    for (sequence, sock) in clients.iter().enumerate() {
        if same_user(&sock).is_err() {
            continue;
        }
        let runtime = runtime.clone();
        let controls = controls.clone();
        std::thread::Builder::new()
            .name("hl-gpu-session".into())
            .spawn(move || {
                if let Err(e) =
                    serve_one(sock, &runtime, sequence as u64, total, geometry, controls)
                {
                    eprintln!("[gpu] hl session {sequence}: {e}");
                }
            })?;
    }
    Ok(())
}

fn serve_one(
    sock: UnixStream,
    runtime: &Path,
    sequence: u64,
    total: u64,
    geometry: sv_transport::control::Geometry,
    controls: Arc<ControlSet>,
) -> Result<(), Error> {
    sock.set_read_timeout(Some(Duration::from_secs(30)))?;
    let by_fd = match recv(&sock)? {
        ColdMessage::AttachRequest => false,
        ColdMessage::AttachRequestFds => true,
        _ => return Err("expected AttachRequest".into()),
    };
    let backing = Named::create(runtime, sequence, total)?;
    let control_region = Arc::new(SharedRegion::map_fd(
        backing.file.as_fd(),
        0,
        geometry.control_size as usize,
    )?);
    let arena_region = Arc::new(SharedRegion::map_fd(
        backing.file.as_fd(),
        geometry.control_size,
        geometry.arena_size as usize,
    )?);
    #[cfg(target_os = "macos")]
    let choice = DeviceChoice::Preferred;
    #[cfg(target_os = "linux")]
    let choice = DeviceChoice::Index(0);
    let control = Arc::new(Control::create(
        control_region,
        geometry,
        crate::protocol_ids(),
        1,
    )?);
    control.stamp_epoch()?;
    let driver = Driver::open_choice(choice)?;
    let arena = arena::import_whole(&driver, arena_region)?;
    let mut objects = Objects::new();
    objects.set_arena(arena.memory, arena.len);
    let fns = Arc::new(driver.load_fns());
    let shared = Arc::new(Shared {
        driver: Arc::new(driver),
        arena,
        fns,
        objects: Mutex::new(objects),
        queue_lock: Arc::new(Mutex::new(())),
        pool_lock: Arc::new(Mutex::new(())),
        video_stable: Default::default(),
        poisoned: Default::default(),
    });
    let result = (|| -> Result<(), Error> {
        controls.add(&control);
        let path = backing
            .path
            .to_str()
            .ok_or("runtime path is not UTF-8")?
            .to_owned();
        let attach = ColdMessage::Attach(Attachment {
            object: path.clone(),
            arena_object: path,
            control_offset: 0,
            control_len: geometry.control_size,
            arena_offset: geometry.control_size,
            arena_len: geometry.arena_size,
            generation: 1,
            run_count: 0,
        });
        if by_fd {
            let mut bytes = Vec::new();
            attach.encode(&mut bytes)?;
            #[cfg(target_os = "linux")]
            sv_transport::localfd::send_with_fds(&sock, &bytes, &[backing.file.as_fd()])?;
            #[cfg(target_os = "macos")]
            send_native_fd(&sock, &bytes, backing.file.as_fd())?;
        } else {
            send(&sock, &attach)?;
        }
        match recv(&sock)? {
            ColdMessage::Accept { generation: 1 } => {}
            _ => return Err("client did not acknowledge its shared mappings".into()),
        }
        // Both processes now own mappings; names are no longer needed.
        std::fs::remove_file(&backing.path)?;
        session::serve(
            control.clone(),
            shared.clone(),
            geometry.ring_slot_count,
            Duration::from_secs(30),
        )
        .map_err(|e| e.into())
        .map(|_| ())
    })();
    let _ = control.set_flag(sv_transport::control::Flags::SHUTDOWN);
    for slot in 0..geometry.ring_slot_count {
        if let Ok(ring) = sv_transport::ring::RingCtrl::open(&control, slot) {
            let _ = ring.set_fatal(1, 0, "native GPU session disconnected");
            for lane in [
                sv_transport::ring::Lane::Command,
                sv_transport::ring::Lane::Reply,
            ] {
                if let Ok(bell) = ring.bell(lane) {
                    bell.ring();
                }
            }
        }
    }
    // A live worker, failed drain or unknown object destructor cannot permit
    // host/GPU writes into a freed mapping. Retain that complete session.
    let shared = match Arc::try_unwrap(shared) {
        Ok(s) => s,
        Err(s) => {
            std::mem::forget(s);
            return Err("live GPU workers retained session backing".into());
        }
    };
    let drained = unsafe { shared.driver.device.device_wait_idle() }.is_ok();
    let clean = drained
        && shared.objects.lock().is_ok_and(|mut o| unsafe {
            o.destroy_leftovers(&shared.driver.device, shared.arena.memory)
                .is_ok()
        });
    if !clean {
        std::mem::forget(shared);
        return Err("undrained GPU session backing retained".into());
    }
    unsafe {
        shared.driver.device.free_memory(shared.arena.memory, None);
    }
    result
}

#[cfg(target_os = "macos")]
fn send_native_fd(
    sock: &UnixStream,
    payload: &[u8],
    fd: std::os::fd::BorrowedFd<'_>,
) -> io::Result<()> {
    use std::io::Write;
    let length =
        u32::try_from(payload.len()).map_err(|_| io::Error::from_raw_os_error(libc::E2BIG))?;
    let mut framed = Vec::with_capacity(payload.len() + 4);
    framed.extend_from_slice(&length.to_le_bytes());
    framed.extend_from_slice(payload);
    let mut iov = libc::iovec {
        iov_base: framed.as_mut_ptr().cast(),
        iov_len: framed.len(),
    };
    let mut control = [0usize; 8];
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = control.as_mut_ptr().cast();
    msg.msg_controllen = unsafe { libc::CMSG_SPACE(4) };
    unsafe {
        let c = libc::CMSG_FIRSTHDR(&msg);
        (*c).cmsg_level = libc::SOL_SOCKET;
        (*c).cmsg_type = libc::SCM_RIGHTS;
        (*c).cmsg_len = libc::CMSG_LEN(4);
        std::ptr::write_unaligned(libc::CMSG_DATA(c).cast::<i32>(), fd.as_raw_fd());
    }
    let n = unsafe { libc::sendmsg(sock.as_raw_fd(), &msg, 0) };
    if n < 0 {
        return Err(io::Error::last_os_error());
    }
    // Ancillary ownership transfers with the first byte; a short payload can
    // continue through ordinary writes without retransmitting its descriptor.
    let mut writer = sock;
    writer.write_all(&framed[n as usize..])
}

fn attach_bell(
    reader: UnixStream,
    controls: &Arc<ControlSet>,
    writers: &Arc<Bells>,
) -> io::Result<()> {
    same_user(&reader)?;
    let writer = reader.try_clone()?;
    #[cfg(target_os = "macos")]
    {
        let yes: libc::c_int = 1;
        if unsafe {
            libc::setsockopt(
                writer.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_NOSIGPIPE,
                (&yes as *const libc::c_int).cast(),
                std::mem::size_of_val(&yes) as libc::socklen_t,
            )
        } != 0
        {
            return Err(io::Error::last_os_error());
        }
    }
    writers
        .0
        .lock()
        .map_err(|_| io::Error::from_raw_os_error(libc::EIO))?
        .push(writer);
    writers.poke(sv_transport::park::RESCAN_TOKEN);
    let target = controls.clone();
    std::thread::Builder::new()
        .name("hl-gpu-doorbell".into())
        .spawn(move || {
            let stop = std::sync::atomic::AtomicBool::new(false);
            let woken = std::sync::atomic::AtomicU64::new(0);
            let _ = sv_transport::vdoorbell::serve_port(reader, &*target, &stop, &woken);
        })?;
    Ok(())
}
