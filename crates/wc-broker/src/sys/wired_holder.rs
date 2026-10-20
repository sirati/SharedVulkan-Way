//! An independent retention process for already allocated wired regions.
//!
//! It has no public socket, allocator or request authorization. Only the
//! broker's inherited private socket can add/remove its kernel objects.
//! A successful add means its separate mmap/mlock completed before the
//! broker may grant a client handle. Broker death closes that socket but
//! never releases the holder's mappings. Thereafter the ordinary Core pool
//! release policy, with no deadline, controls reclamation.

use super::unix::{recv_line, send_with_fds, Region};
use crate::core::{Core, Notice, RegionInfo, User, HOST_VISIBLE_RELEASE};
use std::collections::BTreeMap;
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

pub struct WiredHolder {
    socket: UnixStream,
    child: Child,
}

impl WiredHolder {
    pub fn start(executable: &std::path::Path) -> io::Result<Self> {
        use std::os::unix::process::CommandExt;
        let (socket, child_socket) = UnixStream::pair()?;
        let mut command = Command::new(executable);
        command.arg("__hold-wired").stdin(Stdio::from(OwnedFd::from(child_socket))).stdout(Stdio::null());
        // SAFETY: setsid is async-signal-safe; the fresh child is not a
        // process-group leader. launchd also must abandon the parent's group.
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() < 0 { return Err(io::Error::last_os_error()); }
                Ok(())
            });
        }
        let child = command.spawn()?;
        Ok(Self { socket, child })
    }

    pub fn retain(&mut self, info: &RegionInfo, user: &User, region: &Region) -> io::Result<()> {
        if self.child.try_wait()?.is_some() {
            return Err(io::Error::other("independent wired holder exited; refusing the grant"));
        }
        // pool names and numeric uid identity are validated by the core.
        let line = format!("add {} {} {} {} {}\n", info.id, info.epoch, info.len, user.0, info.pool);
        send_with_fds(&self.socket, line.as_bytes(), &[region.fd.as_raw_fd()])?;
        let mut reply = [0u8; 1];
        self.socket.read_exact(&mut reply)?;
        if reply != [b'+'] { return Err(io::Error::other("independent holder could not wire the region")); }
        Ok(())
    }

    pub fn remove(&mut self, id: u64) {
        // If the channel failed, do not recover by killing the holder.
        // It must keep every existing mapping until the host releases it.
        if writeln!(self.socket, "remove {id}").is_ok() {
            // Serialize every command through its reply. Otherwise a stream
            // recvmsg could attach the next add's fd to a preceding remove.
            let mut reply = [0u8; 1];
            let _ = self.socket.read_exact(&mut reply);
        }
    }
}

// Dropping Child does not terminate it. Dropping the private socket switches
// it to host-release probes. Never wait/kill it in Drop, even on startup errors.

pub fn run() -> io::Result<()> {
    // The only control channel was installed as stdin by our parent. No
    // pathname or new caller can connect. Signals cannot shorten retention.
    unsafe {
        libc::signal(libc::SIGTERM, libc::SIG_IGN);
        libc::signal(libc::SIGINT, libc::SIG_IGN);
        libc::signal(libc::SIGHUP, libc::SIG_IGN);
        libc::signal(libc::SIGPIPE, libc::SIG_IGN);
    }
    let socket = unsafe { UnixStream::from_raw_fd(0) };
    let mut core = Core::new(Vec::new());
    let mut regions = BTreeMap::<u64, Region>::new();
    let mut buf = Vec::new();
    let mut fds = Vec::new();
    loop {
        let line = match recv_line(&socket, &mut buf, &mut fds) {
            Ok(Some(line)) => line,
            // A malformed/broken private channel is loss of the controller,
            // never permission to unwind mappings or exit.
            Ok(None) | Err(_) => break,
        };
        let words: Vec<_> = line.split_whitespace().collect();
        match words.as_slice() {
            ["add", id, epoch, len, user, pool] if fds.len() == 1 => {
                let parsed = id.parse::<u64>().ok().zip(epoch.parse::<u64>().ok()).zip(len.parse::<u64>().ok());
                let Some(((id, epoch), len)) = parsed else { break };
                if regions.contains_key(&id) || crate::proto::Request::parse(&format!("find {pool}")).is_err() { break; }
                let fd = fds.pop().unwrap();
                let region = match Region::wired(fd, len, None) {
                    Ok(region) => region,
                    Err(_) => { let _ = (&socket).write_all(b"-"); continue; }
                };
                core.restore_orphan(&RegionInfo { id, epoch, len, pool: (*pool).into() }, User((*user).into()),
                    HOST_VISIBLE_RELEASE, Instant::now());
                regions.insert(id, region);
                if (&socket).write_all(b"+").is_err() { break; }
            }
            ["remove", id] if fds.is_empty() => {
                let Ok(id) = id.parse() else { break };
                // This command comes only from the authoritative broker core
                // after its release decision, including an owner's free.
                core.alloc_failed(id);
                regions.remove(&id);
                if (&socket).write_all(b"+").is_err() { break; }
            }
            _ => break,
        }
    }
    // Make a failed protocol visible to the broker before probing. Keeping
    // a connected but unread channel would leave its next grant blocked.
    let _ = socket.shutdown(std::net::Shutdown::Both);
    // No controller remains. The same shared lifetime engine handles its
    // now-orphaned regions, with the exact host boot-page release proof.
    loop {
        for id in core.probe_candidates() {
            if regions.get(&id).is_some_and(Region::boot_released) { core.pool_released(id); }
        }
        core.tick(Instant::now());
        for notice in core.drain() {
            if let Notice::Destroy(id) = notice { regions.remove(&id); }
        }
        if regions.is_empty() { return Ok(()); }
        std::thread::sleep(Duration::from_millis(100));
    }
}
