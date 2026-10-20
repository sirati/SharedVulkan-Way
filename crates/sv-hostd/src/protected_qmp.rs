//! Unactivated service-owned memfd launch and exact QMP BAR enrollment.
//! No caller-supplied original file, named-file substitution or proc-fd reopen.
use crate::qemu::Qmp;
use serde_json::{Value, json};
use std::fs::File;
use std::io::{self, Read};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::sync::{Arc, Mutex};
use sv_ram::linux_payload::{LiveOriginalOwner, SealedBacking};
use wc_broker::bar_registry::PhysicalBacking;
use wc_broker::service_grants::BackingGeometry;
fn denied(s: &str) -> io::Error {
    io::Error::new(io::ErrorKind::PermissionDenied, s)
}
fn reply(q: &mut Qmp, command: Value) -> io::Result<Value> {
    let text = q.command(&command.to_string())?;
    let value: Value = serde_json::from_str(&text)?;
    value
        .get("return")
        .cloned()
        .ok_or_else(|| denied("QMP command failed or omitted result"))
}
/// Created only inside the protected service. Its original descriptor remains
/// private and is inherited only by the independently trusted QEMU command.
pub struct PendingOriginal {
    file: File,
    bytes: u64,
}
impl PendingOriginal {
    /// # Safety
    /// Caller is the protected canonical source launcher. This is a genuinely new
    /// physical backing, not a replacement registry for a prior live namespace.
    pub unsafe fn create(bytes: u64) -> io::Result<Self> {
        if bytes < 16384 || !bytes.is_power_of_two() || bytes > i64::MAX as u64 {
            return Err(denied("unsupported protected BAR size"));
        }
        let name = c"wc-protected-original";
        let fd = unsafe {
            libc::memfd_create(name.as_ptr(), libc::MFD_ALLOW_SEALING | libc::MFD_CLOEXEC)
        };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        let file = unsafe { File::from_raw_fd(fd) };
        file.set_len(bytes)?;
        let seals = libc::F_SEAL_SHRINK | libc::F_SEAL_GROW | libc::F_SEAL_SEAL;
        if unsafe { libc::fcntl(fd, libc::F_ADD_SEALS, seals) } < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self { file, bytes })
    }
    /// Launch with this exact original, a fixed single-entry fdset and BAR backend.
    /// No desktop command or arguments may reach this boundary.
    ///
    /// # Safety
    /// Entire command/executable, QMP path and ancestor directory are protected
    /// launcher configuration. The command must directly execute trusted QEMU,
    /// not a wrapper that substitutes its own executable/arguments. Hotplug and
    /// QMP/backend mutation must remain exclusively under this owner afterwards.
    /// No external SIGCHLD handler/reaper may reap this owned child. Entire
    /// command, environment and any pre_exec callbacks must be immutable trusted
    /// configuration. This API rejects reserved topology/daemonization options
    /// and requires a direct canonical ELF, but does not authenticate arbitrary
    /// code or validate every QEMU option. Ancestors and parent namespace must
    /// remain protected against other writers throughout launch/enrollment.
    pub unsafe fn launch(
        self,
        mut command: Command,
        qmp_path: &Path,
    ) -> io::Result<LaunchedOriginal> {
        validate_command(&command)?;
        let program = Path::new(command.get_program());
        if !program.is_absolute() || std::fs::canonicalize(program)? != program {
            return Err(denied("QEMU must be an absolute canonical executable path"));
        }
        let mut executable_file = File::open(program)?;
        let executable = executable_file.metadata()?;
        let mut magic = [0; 4];
        executable_file.read_exact(&mut magic)?;
        if !executable.is_file() || magic != *b"\x7fELF" || executable.mode() & 0o022 != 0 {
            return Err(denied(
                "QEMU must be the immutable direct ELF executable, not a wrapper",
            ));
        }
        let qmp_parent = protected_qmp_parent(qmp_path)?;
        // The child's own pre-exec code captures its pidfd and sends the exact
        // object over a private socketpair. No parent-side PID lookup race.
        let mut pair = [-1; 2];
        if unsafe {
            libc::socketpair(
                libc::AF_UNIX,
                libc::SOCK_DGRAM | libc::SOCK_CLOEXEC,
                0,
                pair.as_mut_ptr(),
            )
        } < 0
        {
            return Err(io::Error::last_os_error());
        }
        let receiver = unsafe { OwnedFd::from_raw_fd(pair[0]) };
        let sender = unsafe { OwnedFd::from_raw_fd(pair[1]) };
        let fd = self.file.as_raw_fd();
        let qmp_arg = format!(
            "unix:{},server=on,wait=off",
            qmp_path
                .to_str()
                .ok_or_else(|| denied("non-UTF8 QMP path"))?
        );
        if qmp_arg.contains(',') && qmp_path.as_os_str().as_encoded_bytes().contains(&b',') {
            return Err(denied("QMP path contains argument separator"));
        }
        command.args(["-add-fd",&format!("fd={fd},set=37,opaque=wc-protected-original"),"-object",&format!("memory-backend-file,id=wc-protected-memory,mem-path=/dev/fdset/37,size={},share=on",self.bytes),"-device","ivshmem-plain,id=wc-protected-bar,memdev=wc-protected-memory,addr=1c","-qmp",&qmp_arg]);
        unsafe {
            command.pre_exec(move || {
                let native = libc::syscall(libc::SYS_pidfd_open, libc::getpid(), 0) as i32;
                if native < 0 {
                    return Err(io::Error::last_os_error());
                }
                let sent = send_native(sender.as_raw_fd(), native);
                libc::close(native);
                sent?;
                let flags = libc::fcntl(fd, libc::F_GETFD);
                if flags < 0 {
                    return Err(io::Error::last_os_error());
                }
                if libc::fcntl(fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC) < 0 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let mut child = command.spawn()?;
        let pidfd = match receive_native(&receiver) {
            Ok(pidfd) => pidfd,
            Err(error) => {
                // This is still our unreaped Child; no external child reaper
                // may own it under the launch contract. Never detach a QEMU
                // with the inherited original on a native-object query error.
                let _ = child.kill();
                let _ = child.wait();
                return Err(error);
            }
        };
        Ok(LaunchedOriginal {
            file: self.file,
            bytes: self.bytes,
            child: Some(child),
            pidfd,
            executable: (executable.dev(), executable.ino()),
            qmp_path: qmp_path.to_owned(),
            qmp_parent,
            _executable: executable_file,
        })
    }
}
fn validate_command(command: &Command) -> io::Result<()> {
    for argument in command.get_args() {
        let text = argument
            .to_str()
            .ok_or_else(|| denied("non-UTF8 protected QEMU argument"))?;
        let option = text.trim_start_matches('-').split('=').next().unwrap_or("");
        if text.starts_with('-')
            && matches!(
                option,
                "qmp"
                    | "add-fd"
                    | "device"
                    | "object"
                    | "readconfig"
                    | "writeconfig"
                    | "config"
                    | "mon"
                    | "monitor"
                    | "daemonize"
                    | "run-with"
                    | "runas"
            )
        {
            return Err(denied(
                "QEMU command overrides protected launch topology or child ownership",
            ));
        }
    }
    Ok(())
}
fn protected_parent(path: &Path) -> io::Result<File> {
    let directory = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)?;
    let m = directory.metadata()?;
    if !m.is_dir() || m.uid() != unsafe { libc::geteuid() } || m.mode() & 0o077 != 0 {
        return Err(denied(
            "QMP parent is not a private service-owned directory",
        ));
    }
    Ok(directory)
}
fn protected_qmp_parent(path: &Path) -> io::Result<File> {
    if path.file_name().is_none() {
        return Err(denied("QMP path has no socket filename"));
    }
    let directory = protected_parent(path.parent().ok_or_else(|| denied("QMP parent missing"))?)?;
    match std::fs::symlink_metadata(path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(directory),
        Err(error) => Err(error),
        Ok(_) => Err(denied(
            "QMP endpoint already exists before protected launch",
        )),
    }
}
// Called only in pre_exec. Stack buffers and async-signal-safe syscalls only.
fn send_native(socket: i32, native: i32) -> io::Result<()> {
    let byte = 1u8;
    let mut iov = libc::iovec {
        iov_base: (&byte as *const u8).cast_mut().cast(),
        iov_len: 1,
    };
    let mut control = [0usize; 4];
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = control.as_mut_ptr().cast();
    msg.msg_controllen = unsafe { libc::CMSG_SPACE(std::mem::size_of::<i32>() as u32) } as usize;
    unsafe {
        let header = libc::CMSG_FIRSTHDR(&msg);
        (*header).cmsg_level = libc::SOL_SOCKET;
        (*header).cmsg_type = libc::SCM_RIGHTS;
        (*header).cmsg_len = libc::CMSG_LEN(std::mem::size_of::<i32>() as u32) as usize;
        std::ptr::write_unaligned(libc::CMSG_DATA(header).cast::<i32>(), native);
        if libc::sendmsg(socket, &msg, libc::MSG_NOSIGNAL) != 1 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}
fn receive_native(socket: &OwnedFd) -> io::Result<OwnedFd> {
    let mut byte = 0u8;
    let mut iov = libc::iovec {
        iov_base: (&mut byte as *mut u8).cast(),
        iov_len: 1,
    };
    let mut control = [0usize; 4];
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = control.as_mut_ptr().cast();
    msg.msg_controllen = std::mem::size_of_val(&control);
    if unsafe {
        libc::recvmsg(
            socket.as_raw_fd(),
            &mut msg,
            libc::MSG_CMSG_CLOEXEC | libc::MSG_DONTWAIT,
        )
    } != 1
    {
        return Err(io::Error::last_os_error());
    }
    let header = unsafe { libc::CMSG_FIRSTHDR(&msg) };
    if msg.msg_flags & (libc::MSG_TRUNC | libc::MSG_CTRUNC) != 0 || byte != 1 || header.is_null() {
        return Err(denied("missing exact native child object"));
    }
    let header = unsafe { &*header };
    if header.cmsg_level != libc::SOL_SOCKET
        || header.cmsg_type != libc::SCM_RIGHTS
        || header.cmsg_len != unsafe { libc::CMSG_LEN(std::mem::size_of::<i32>() as u32) } as usize
    {
        return Err(denied("invalid native child object message"));
    }
    let fd = unsafe { std::ptr::read_unaligned(libc::CMSG_DATA(header).cast::<i32>()) };
    if fd < 0 {
        return Err(denied("invalid native child descriptor"));
    }
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}
/// Owns only its newly launched child. Failed enrollment/drop kills and waits
/// this exact child, never a PID found by name or a preexisting VM.
pub struct LaunchedOriginal {
    file: File,
    bytes: u64,
    child: Option<Child>,
    pidfd: OwnedFd,
    executable: (u64, u64),
    qmp_path: PathBuf,
    qmp_parent: File,
    _executable: File,
}
impl Drop for LaunchedOriginal {
    fn drop(&mut self) {
        if let Some(mut c) = self.child.take() {
            // Signal the exact captured native object, never a reusable PID.
            unsafe {
                libc::syscall(
                    libc::SYS_pidfd_send_signal,
                    self.pidfd.as_raw_fd(),
                    libc::SIGKILL,
                    std::ptr::null::<libc::siginfo_t>(),
                    0,
                );
            }
            let _ = c.wait();
        }
    }
}
impl LaunchedOriginal {
    /// Caller waits for its private QMP socket, then enrolls this exact child.
    pub fn enroll(mut self, arena_base: u64) -> io::Result<EnrolledOriginal> {
        let parent = protected_parent(
            self.qmp_path
                .parent()
                .ok_or_else(|| denied("QMP parent missing"))?,
        )?;
        let expected = self.qmp_parent.metadata()?;
        let actual = parent.metadata()?;
        if expected.dev() != actual.dev() || expected.ino() != actual.ino() {
            return Err(denied("protected QMP parent changed after launch"));
        }
        let mut qmp = Qmp::connect(&self.qmp_path)?;
        let pid = qmp.peer_pid()?;
        let child = self
            .child
            .as_mut()
            .ok_or_else(|| denied("missing original QEMU child"))?;
        if pid != child.id() || child.try_wait()?.is_some() {
            return Err(denied("QMP peer is not exact live launched QEMU child"));
        }
        let exe = std::fs::metadata(format!("/proc/{pid}/exe"))?;
        if (exe.dev(), exe.ino()) != self.executable {
            return Err(denied("QMP peer executable differs from trusted launch"));
        }
        live(&self.pidfd)?;
        let start = std::fs::read_to_string(format!("/proc/{pid}/stat"))?;
        let start = start
            .rsplit_once(')')
            .and_then(|(_, s)| s.split_whitespace().nth(19))
            .and_then(|s| s.parse::<u64>().ok())
            .filter(|n| *n > 0)
            .ok_or_else(|| denied("QEMU native start identity unavailable"))?;
        let m = self.file.metadata()?;
        let physical = PhysicalBacking {
            host_device: m.dev(),
            host_inode: m.ino(),
            bar_bytes: self.bytes,
        };
        validate(&mut qmp, pid, physical)?;
        validate(&mut qmp, pid, physical)?;
        live(&self.pidfd)?;
        let original = self.file.try_clone()?;
        let owner = Arc::new(QmpOwner {
            launched: Mutex::new(self),
            qmp: Mutex::new(qmp),
            physical,
            pid,
        });
        let backing = unsafe {
            SealedBacking::from_live_original(
                original,
                BackingGeometry {
                    physical,
                    arena_base,
                },
                owner.clone(),
            )
        }?;
        Ok(EnrolledOriginal {
            backing: Arc::new(backing),
            owner,
            pid,
            start,
            physical,
        })
    }
}
fn live(fd: &OwnedFd) -> io::Result<()> {
    let mut p = libc::pollfd {
        fd: fd.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    if unsafe { libc::poll(&mut p, 1, 0) } < 0 {
        return Err(io::Error::last_os_error());
    }
    if p.revents != 0 {
        return Err(denied(
            "exact QEMU process object exited or cannot be queried",
        ));
    }
    Ok(())
}
fn validate(q: &mut Qmp, pid: u32, physical: PhysicalBacking) -> io::Result<()> {
    let status = reply(q, json!({"execute":"query-status"}))?;
    if status["running"].as_bool() != Some(true) {
        return Err(denied("protected QEMU is not running"));
    }
    let path = "/machine/peripheral/wc-protected-bar";
    let backend = reply(
        q,
        json!({"execute":"qom-get","arguments":{"path":path,"property":"memdev"}}),
    )?;
    if backend.as_str() != Some("/objects/wc-protected-memory") {
        return Err(denied("QMP BAR backend substitution"));
    }
    let mempath = reply(
        q,
        json!({"execute":"qom-get","arguments":{"path":"/objects/wc-protected-memory","property":"mem-path"}}),
    )?;
    if mempath.as_str() != Some("/dev/fdset/37") {
        return Err(denied("QMP backend is not original protected fdset"));
    }
    let offset = reply(
        q,
        json!({"execute":"qom-get","arguments":{"path":"/objects/wc-protected-memory","property":"offset"}}),
    )?;
    if offset.as_u64() != Some(0) {
        return Err(denied("QMP backend is a subfile alias"));
    }
    let memdevs = reply(q, json!({"execute":"query-memdev"}))?;
    let entries: Vec<_> = memdevs
        .as_array()
        .ok_or_else(|| denied("missing QMP backends"))?
        .iter()
        .filter(|v| v["id"].as_str() == Some("wc-protected-memory"))
        .collect();
    if entries.len() != 1
        || entries[0]["size"].as_u64() != Some(physical.bar_bytes)
        || entries[0]["share"].as_bool() != Some(true)
    {
        return Err(denied("QMP backend geometry/share differs"));
    }
    let sets = reply(q, json!({"execute":"query-fdsets"}))?;
    let sets: Vec<_> = sets
        .as_array()
        .ok_or_else(|| denied("missing QMP fdsets"))?
        .iter()
        .filter(|s| s["fdset-id"].as_u64() == Some(37))
        .collect();
    if sets.len() != 1 {
        return Err(denied("QMP protected fdset absent or ambiguous"));
    }
    let fds = sets[0]["fds"]
        .as_array()
        .ok_or_else(|| denied("missing QMP fdset entries"))?;
    if fds.len() != 1 || fds[0]["opaque"].as_str() != Some("wc-protected-original") {
        return Err(denied("QMP protected fdset alias/substitution"));
    }
    let fd = fds[0]["fd"]
        .as_u64()
        .filter(|n| *n <= i32::MAX as u64)
        .ok_or_else(|| denied("invalid QMP original FD"))?;
    // Metadata only. Never reopen any proc FD or bless an unrelated whole BAR.
    let m = std::fs::metadata(format!("/proc/{pid}/fd/{fd}"))?;
    if !m.is_file()
        || m.dev() != physical.host_device
        || m.ino() != physical.host_inode
        || m.len() != physical.bar_bytes
    {
        return Err(denied("QMP fdset does not retain exact original object"));
    }
    let pci = reply(q, json!({"execute":"query-pci"}))?;
    crate::qemu_bar::protected_device_size(&pci, "wc-protected-bar", 28, physical.bar_bytes)?;
    Ok(())
}
struct QmpOwner {
    launched: Mutex<LaunchedOriginal>,
    qmp: Mutex<Qmp>,
    physical: PhysicalBacking,
    pid: u32,
}
// SAFETY: independently retained exact child/pidfd and QMP objects, no PID
// lookup replacement or client-selected source. All mutable source geometry
// is checked again; death/query ambiguity always refuses and never releases.
unsafe impl LiveOriginalOwner for QmpOwner {
    fn ensure_live(&self) -> io::Result<()> {
        let owner = self
            .launched
            .lock()
            .map_err(|_| denied("QEMU child owner poisoned"))?;
        live(&owner.pidfd)?;
        let mut qmp = self
            .qmp
            .lock()
            .map_err(|_| denied("QMP original owner poisoned"))?;
        validate(&mut qmp, self.pid, self.physical)?;
        live(&owner.pidfd)
    }
}
/// Retains exact original, child, pidfd and QMP connection through every grant.
pub struct EnrolledOriginal {
    backing: Arc<SealedBacking>,
    owner: Arc<QmpOwner>,
    pid: u32,
    start: u64,
    physical: PhysicalBacking,
}
impl EnrolledOriginal {
    pub fn pid(&self) -> u32 {
        self.pid
    }
    pub fn start(&self) -> u64 {
        self.start
    }
    pub fn physical(&self) -> PhysicalBacking {
        self.physical
    }
    pub fn backing(&self) -> Arc<SealedBacking> {
        self.backing.clone()
    }
    pub fn ensure_live(&self) -> io::Result<()> {
        self.owner.ensure_live()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
    #[test]
    fn command_cannot_override_private_topology_or_detach_child() {
        for argument in [
            "-qmp",
            "--qmp=unix:foreign",
            "-add-fd",
            "-device",
            "-object",
            "-readconfig",
            "-mon",
            "-daemonize",
            "-run-with",
        ] {
            let mut command = Command::new("/unused-trusted-qemu");
            command.arg(argument);
            assert!(validate_command(&command).is_err(), "{argument}");
        }
        let mut command = Command::new("/unused-trusted-qemu");
        command.args([
            "-machine",
            "q35",
            "-accel",
            "tcg,thread=single",
            "-nodefaults",
        ]);
        validate_command(&command).unwrap();
    }
    #[test]
    fn existing_endpoint_and_nonprivate_parent_refuse_before_connect() {
        let name = format!(
            "wc-qmp-guard-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let directory = std::env::temp_dir().join(name);
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&directory)
            .unwrap();
        let endpoint = directory.join("qmp.sock");
        assert!(protected_qmp_parent(&endpoint).is_ok());
        std::fs::write(&endpoint, b"foreign endpoint must not be touched").unwrap();
        assert!(protected_qmp_parent(&endpoint).is_err());
        assert_eq!(
            std::fs::read(&endpoint).unwrap(),
            b"foreign endpoint must not be touched"
        );
        std::fs::remove_file(&endpoint).unwrap();
        std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o750)).unwrap();
        assert!(protected_qmp_parent(&endpoint).is_err());
        std::fs::remove_dir(&directory).unwrap();
    }
}
