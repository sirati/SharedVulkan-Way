//! Kernel identity for a Linux Unix-domain service endpoint.
//!
//! SO_PEERPIDFD retains the exact peer process, independently of numeric PID
//! reuse. Socket close and protocol acknowledgements never prove process death
//! or resource completion. The operator must also enforce the privileged
//! service's process boundary. Matching a uid or executable alone cannot stop
//! an arbitrary same-uid process from injecting into an unprotected service.
use std::fs::File;
use std::io;
use std::mem::{MaybeUninit, size_of};
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd};
use std::os::unix::fs::MetadataExt;
use std::os::unix::net::UnixStream;

const SO_PEERPIDFD: libc::c_int = 77;
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Credentials {
    pub pid: u32,
    pub uid: u32,
    pub gid: u32,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Executable {
    pub device: u64,
    pub inode: u64,
}
impl Executable {
    /// Pin an operator-selected, retained executable inode, not a basename.
    pub fn from_file(file: &File) -> io::Result<Self> {
        let metadata = file.metadata()?;
        if !metadata.is_file() {
            return Err(invalid("service executable is not a regular file"));
        }
        Ok(Self {
            device: metadata.dev(),
            inode: metadata.ino(),
        })
    }
}
#[derive(Clone, Copy, Debug)]
pub struct Policy {
    pub uid: u32,
    pub gid: Option<u32>,
    pub executable: Executable,
}

/// An endpoint identity, not an allocation-release capability.
pub struct UnixPeer {
    credentials: Credentials,
    process: OwnedFd,
    executable: File,
    identity: Executable,
}
fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::PermissionDenied, message)
}
fn option<T>(socket: &UnixStream, name: libc::c_int) -> io::Result<T> {
    let mut value = MaybeUninit::<T>::uninit();
    let mut length = size_of::<T>() as libc::socklen_t;
    // SAFETY: kernel gets a complete output object and its exact capacity.
    let result = unsafe {
        libc::getsockopt(
            socket.as_raw_fd(),
            libc::SOL_SOCKET,
            name,
            value.as_mut_ptr().cast(),
            &mut length,
        )
    };
    if result != 0 {
        return Err(io::Error::last_os_error());
    }
    if length as usize != size_of::<T>() {
        return Err(invalid("kernel peer identity length differs"));
    }
    // SAFETY: successful exact-length getsockopt initialized the whole object.
    Ok(unsafe { value.assume_init() })
}
impl UnixPeer {
    /// Capture the socket's actual peer process. No pidfd_open by claimed PID
    /// and no fallback when the kernel lacks SO_PEERPIDFD.
    pub fn capture(socket: &UnixStream) -> io::Result<Self> {
        let credentials: libc::ucred = option(socket, libc::SO_PEERCRED)?;
        if credentials.pid <= 0 {
            return Err(invalid("socket peer has no process identity"));
        }
        let raw: libc::c_int = option(socket, SO_PEERPIDFD)?;
        if raw < 0 {
            return Err(invalid("kernel returned an invalid peer pidfd"));
        }
        // SAFETY: SO_PEERPIDFD transferred a newly owned descriptor.
        let process = unsafe { OwnedFd::from_raw_fd(raw) };
        // SAFETY: retained owned fd and the valid close-on-exec flag.
        if unsafe { libc::fcntl(process.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) } < 0 {
            return Err(io::Error::last_os_error());
        }
        let executable = File::open(format!("/proc/{}/exe", credentials.pid))?;
        let identity = Executable::from_file(&executable)?;
        let peer = Self {
            credentials: Credentials {
                pid: credentials.pid as u32,
                uid: credentials.uid,
                gid: credentials.gid,
            },
            process,
            executable,
            identity,
        };
        // A process that exited during /proc inspection cannot be confused
        // with a new process which inherited its numeric PID.
        if peer.exited()? {
            return Err(invalid("service peer exited during identity capture"));
        }
        Ok(peer)
    }
    pub fn credentials(&self) -> Credentials {
        self.credentials
    }
    pub fn executable_identity(&self) -> Executable {
        self.identity
    }
    pub fn pidfd(&self) -> BorrowedFd<'_> {
        self.process.as_fd()
    }
    pub fn executable(&self) -> &File {
        &self.executable
    }
    /// Check operator policy. Deployment isolation remains a prerequisite;
    /// this does not certify that a same-uid process cannot ptrace the peer.
    pub fn require(&self, policy: Policy) -> io::Result<()> {
        if self.credentials.uid != policy.uid
            || policy.gid.is_some_and(|gid| gid != self.credentials.gid)
            || self.identity != policy.executable
        {
            return Err(invalid(
                "service peer credentials or executable inode differ",
            ));
        }
        if self.exited()? {
            return Err(invalid("service peer already exited"));
        }
        Ok(())
    }
    /// Kernel-confirmed exit of this retained process only. A true result may
    /// support retiring its producer references; it never releases another
    /// process's consumer references or undelivered pending transfers.
    pub fn exited(&self) -> io::Result<bool> {
        let mut descriptor = libc::pollfd {
            fd: self.process.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: one complete pollfd and a zero timeout, no retained borrow.
        let result = unsafe { libc::poll(&mut descriptor, 1, 0) };
        if result < 0 {
            return Err(io::Error::last_os_error());
        }
        if descriptor.revents & (libc::POLLNVAL | libc::POLLERR) != 0 {
            return Err(invalid("retained service pidfd became invalid"));
        }
        Ok(descriptor.revents & libc::POLLIN != 0)
    }
}

// SAFETY: the only liveness query polls the original SO_PEERPIDFD descriptor.
// No message field, socket EOF or reopened numeric PID participates.
unsafe impl wc_broker::service_auth::ServiceProcess for UnixPeer {
    fn exited(&self) -> io::Result<bool> {
        UnixPeer::exited(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::os::unix::fs::DirBuilderExt;
    use std::os::unix::net::UnixListener;
    use std::process::{Child, Command, Stdio};
    struct ChildGuard(Child);
    impl Drop for ChildGuard {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    struct Directory(std::path::PathBuf);
    impl Drop for Directory {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    #[test]
    fn child_endpoint_fixture() {
        let Some(path) = std::env::var_os("WC_UNIX_PEER_CHILD_SOCKET") else {
            return;
        };
        let mut stream = UnixStream::connect(path).unwrap();
        stream
            .set_read_timeout(Some(std::time::Duration::from_secs(10)))
            .unwrap();
        stream.write_all(b"R").unwrap();
        let mut command = [0];
        stream.read_exact(&mut command).unwrap();
        assert_eq!(command, [b'C']);
        drop(stream);
        // The endpoint is closed while the actual process deliberately stays
        // alive. The parent must not infer completion from the socket EOF.
        std::io::stdin().read_exact(&mut command).unwrap();
        assert_eq!(command, [b'Q']);
    }
    #[test]
    fn kernel_pidfd_identity_survives_endpoint_close_until_actual_exit() {
        let directory = Directory(std::path::PathBuf::from(format!(
            "/mnt/datasets/builds/scratch/wc-unix-peer-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        )));
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&directory.0)
            .unwrap();
        let socket = directory.0.join("service");
        let listener = UnixListener::bind(&socket).unwrap();
        listener.set_nonblocking(true).unwrap();
        let executable = std::env::current_exe().unwrap();
        let mut child = ChildGuard(
            Command::new(&executable)
                .args([
                    "--exact",
                    "unix_peer::tests::child_endpoint_fixture",
                    "--nocapture",
                ])
                .env("WC_UNIX_PEER_CHILD_SOCKET", &socket)
                .stdin(Stdio::piped())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .unwrap(),
        );
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let mut stream = loop {
            match listener.accept() {
                Ok((stream, _)) => break stream,
                Err(error)
                    if error.kind() == io::ErrorKind::WouldBlock
                        && std::time::Instant::now() < deadline =>
                {
                    std::thread::sleep(std::time::Duration::from_millis(2))
                }
                Err(error) => panic!("child endpoint did not connect: {error}"),
            }
        };
        stream
            .set_read_timeout(Some(std::time::Duration::from_secs(10)))
            .unwrap();
        let peer = UnixPeer::capture(&stream).unwrap();
        let service_process: &dyn wc_broker::service_auth::ServiceProcess = &peer;
        assert_eq!(peer.credentials().pid, child.0.id());
        let expected = Executable::from_file(&File::open(executable).unwrap()).unwrap();
        let policy = Policy {
            uid: peer.credentials().uid,
            gid: Some(peer.credentials().gid),
            executable: expected,
        };
        peer.require(policy).unwrap();
        assert!(
            peer.require(Policy {
                executable: Executable {
                    device: 0,
                    inode: 0
                },
                ..policy
            })
            .is_err()
        );
        let mut ready = [0];
        stream.read_exact(&mut ready).unwrap();
        assert_eq!(ready, [b'R']);
        stream.write_all(b"C").unwrap();
        assert_eq!(stream.read(&mut ready).unwrap(), 0);
        assert!(
            !service_process.exited().unwrap(),
            "endpoint close is not kernel process death"
        );
        child.0.stdin.as_mut().unwrap().write_all(b"Q").unwrap();
        assert!(child.0.wait().unwrap().success());
        assert!(service_process.exited().unwrap());
        assert!(peer.require(policy).is_err());
    }
}
