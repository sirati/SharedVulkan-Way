//! What a compositor, GPU process, launcher or proxy calls (§111.4).
//!
//! One connection per process, kept open for the process's life: the broker
//! watches the process itself, and the connection is also what a lease
//! belongs to. Every call is one line out and one line back; lines the broker
//! pushes (`!…`) are put aside for [`Client::next_event`].

#[cfg(unix)]
use crate::proto;
#[cfg(unix)]
use std::io;

/// Where the broker listens unless `WC_BROKER` says otherwise.
#[cfg(target_os = "linux")]
pub const DEFAULT_PATH: &str = "/run/waycompose/broker.sock";
#[cfg(target_os = "macos")]
pub const DEFAULT_PATH: &str = "/var/run/waycompose/broker.sock";
#[cfg(windows)]
pub const DEFAULT_PATH: &str = r"\\.\pipe\waycompose-broker";

/// The broker's address: `WC_BROKER`, else [`DEFAULT_PATH`].
#[cfg(any(unix, windows))]
pub fn path() -> String {
    std::env::var("WC_BROKER").unwrap_or_else(|_| DEFAULT_PATH.to_string())
}

/// What `hello` answers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hello {
    pub run: String,
    pub user: String,
    pub pid: u32,
}

#[cfg(unix)]
fn refused(line: &str) -> io::Error {
    io::Error::new(io::ErrorKind::PermissionDenied, proto::reply_error(line))
}

/// An `ok signed SEQ MAC IDENTITY...` reply, for any platform's client.
pub fn parse_signed(line: &str, install: &str) -> Result<(wc_handle::macboot::Auth, wc_handle::macboot::Identity), String> {
    let rest = line.strip_prefix("ok signed ").ok_or_else(|| crate::proto::reply_error(line))?;
    let mut w = rest.splitn(3, ' ');
    let seq: u64 = w.next().and_then(|s| s.parse().ok()).ok_or("seq")?;
    let hex = w.next().ok_or("mac")?;
    let identity = wc_handle::macboot::Identity::parse(w.next().ok_or("identity")?).ok_or("identity")?;
    if hex.len() != 64 {
        return Err("mac".into());
    }
    let mut mac = [0u8; 32];
    for (i, b) in mac.iter_mut().enumerate() {
        *b = u8::from_str_radix(&hex[2 * i..2 * i + 2], 16).map_err(|_| "mac")?;
    }
    Ok((wc_handle::macboot::Auth { installation: install.to_string(), seq, mac }, identity))
}

#[cfg(unix)]
pub use unix_client::Client;
#[cfg(unix)]
pub use process::{close_region, find_region, install_hooks, open_path, open_region, own_connection, region_spec};

/// §111.10: what a session's process on the client role's host uses in place
/// of opening the raw devices itself. A path `broker:ID` is region `ID`, which
/// the daemon published for this process's user (the BAR file today); a hub
/// spec `broker:NAME` is a fresh connection to the hub's service `NAME`. One
/// connection per process, kept for the process's life: it is what the lease
/// on a region belongs to, and how the broker sees the process exit.
#[cfg(unix)]
mod process {
    use super::*;
    use std::collections::BTreeMap;
    use std::os::fd::OwnedFd;
    use std::sync::Mutex;

    struct Shared {
        client: Option<Client>,
        regions: BTreeMap<u64, OwnedFd>,
    }

    static SHARED: Mutex<Shared> = Mutex::new(Shared { client: None, regions: BTreeMap::new() });

    fn role() -> String {
        std::env::args()
            .next()
            .and_then(|a| std::path::Path::new(&a).file_name().map(|f| f.to_string_lossy().into_owned()))
            .filter(|r| crate::proto::Request::parse(&format!("hello {r}")).is_ok())
            .unwrap_or_else(|| "client".into())
    }

    fn with_client<T>(f: impl FnOnce(&mut Client) -> io::Result<T>) -> io::Result<T> {
        let mut g = SHARED.lock().unwrap_or_else(|e| e.into_inner());
        if g.client.is_none() {
            let mut c = Client::connect(&super::path())?;
            c.hello(&role())?;
            g.client = Some(c);
        }
        let r = f(g.client.as_mut().expect("just set"));
        if r.as_ref().is_err_and(|e| e.kind() == io::ErrorKind::UnexpectedEof || e.kind() == io::ErrorKind::BrokenPipe) {
            // The daemon was restarted: the next call connects again.
            g.client = None;
        }
        r
    }

    /// The region id of a `broker:ID` path.
    pub fn region_spec(path: &std::path::Path) -> Option<u64> {
        path.to_str()?.strip_prefix(sv_transport::brokered::PREFIX)?.parse().ok()
    }

    /// Open `path` read-write: region `ID` of the broker for `broker:ID`
    /// (its fd, asked for once and duplicated after), else the file.
    pub fn open_path(path: &std::path::Path) -> io::Result<std::fs::File> {
        let Some(id) = region_spec(path) else {
            return std::fs::OpenOptions::new().read(true).write(true).open(path);
        };
        {
            let g = SHARED.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(fd) = g.regions.get(&id) {
                return Ok(std::fs::File::from(fd.try_clone()?));
            }
        }
        let (r, mut fds) = with_client(|c| c.open(id)).map_err(|e| {
            io::Error::new(e.kind(), format!("region {id} from the broker at {}: {e}", super::path()))
        })?;
        if r.pieces.len() != 1 || r.pieces[0].offset != 0 {
            return Err(io::Error::other(format!("region {id} is {} piece(s); a file-like region is one", r.pieces.len())));
        }
        let fd = fds.remove(0);
        let out = std::fs::File::from(fd.try_clone()?);
        SHARED.lock().unwrap_or_else(|e| e.into_inner()).regions.insert(id, fd);
        Ok(out)
    }

    fn connect_service(name: &str) -> io::Result<std::os::unix::net::UnixStream> {
        let fd = with_client(|c| c.connect_service(name))?;
        Ok(std::os::unix::net::UnixStream::from(fd))
    }

    /// Let `sv_transport` reach `broker:NAME` hub specs through this process's
    /// broker connection. Harmless to call more than once.
    pub fn install_hooks() {
        sv_transport::brokered::install(connect_service);
    }

    /// A lease on the live region of this user's pool `pool` (`find`, e.g.
    /// `gpu.<k>`), on this process's one connection. It ends with
    /// [`close_region`], or with the process.
    pub fn find_region(pool: &str) -> io::Result<(crate::proto::RegionReply, Vec<OwnedFd>)> {
        with_client(|c| c.find(pool))
    }

    /// A lease on region `id`, on this process's one connection.
    pub fn open_region(id: u64) -> io::Result<(crate::proto::RegionReply, Vec<OwnedFd>)> {
        with_client(|c| c.open(id))
    }

    /// One lease of this process's connection on region `id` ends.
    pub fn close_region(id: u64) -> io::Result<()> {
        with_client(|c| c.close(id))
    }

    /// A connection of its own, for leases held on behalf of one of several
    /// clients this process serves (the proxy's Wayland clients, each a
    /// sandboxed process that reaches only the Wayland socket, §112): every
    /// lease taken on it ends when it is dropped, whatever else this process
    /// holds. Leases for the process itself go on the shared connection
    /// ([`find_region`], [`open_region`], [`open_path`]); the broker watches
    /// the process through both.
    pub fn own_connection(role: &str) -> io::Result<Client> {
        let mut c = Client::connect(&super::path())?;
        c.hello(role)?;
        Ok(c)
    }
}

#[cfg(unix)]
mod unix_client {
    use super::*;
    use crate::proto::{RegionReply, Request};
    use crate::sys::unix::{recv_line, send_with_fds};
    use std::collections::VecDeque;
    use std::os::fd::OwnedFd;
    use std::os::unix::net::UnixStream;

    pub struct Client {
        s: UnixStream,
        buf: Vec<u8>,
        fds: Vec<OwnedFd>,
        events: VecDeque<String>,
    }

    impl Client {
        pub fn connect(path: &str) -> io::Result<Client> {
            let s = UnixStream::connect(path).map_err(|e| io::Error::new(e.kind(), format!("the broker at {path}: {e}")))?;
            Ok(Client { s, buf: Vec::new(), fds: Vec::new(), events: VecDeque::new() })
        }

        /// One request; its reply line and the fds that came with it.
        fn call(&mut self, req: &Request) -> io::Result<(String, Vec<OwnedFd>)> {
            let mut l = req.line();
            l.push('\n');
            send_with_fds(&self.s, l.as_bytes(), &[])?;
            loop {
                let Some(line) = recv_line(&self.s, &mut self.buf, &mut self.fds)? else {
                    return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "the broker closed the connection"));
                };
                if line.starts_with('!') {
                    self.events.push_back(line);
                    continue;
                }
                let fds = std::mem::take(&mut self.fds);
                if line.starts_with("err") {
                    return Err(refused(&line));
                }
                return Ok((line, fds));
            }
        }

        pub fn hello(&mut self, role: &str) -> io::Result<Hello> {
            let (l, _) = self.call(&Request::Hello { role: role.into() })?;
            match l.split(' ').collect::<Vec<_>>().as_slice() {
                ["ok", "hello", run, user, pid] => {
                    Ok(Hello { run: run.to_string(), user: user.to_string(), pid: pid.parse().unwrap_or(0) })
                }
                _ => Err(io::Error::other(proto::reply_error(&l))),
            }
        }

        /// The device, moved into this process: its fd is now the only one.
        pub fn device(&mut self, name: &str) -> io::Result<OwnedFd> {
            let (l, mut fds) = self.call(&Request::Device { name: name.into() })?;
            if !l.starts_with("ok device") || fds.len() != 1 {
                return Err(io::Error::other(format!("device {name}: {l} with {} fd(s)", fds.len())));
            }
            Ok(fds.remove(0))
        }

        fn region(&mut self, req: &Request) -> io::Result<(RegionReply, Vec<OwnedFd>)> {
            let (l, fds) = self.call(req)?;
            let r = proto::parse_region(&l).map_err(io::Error::other)?;
            if r.pieces.len() != fds.len() {
                return Err(io::Error::other(format!("{} piece(s) and {} fd(s)", r.pieces.len(), fds.len())));
            }
            Ok((r, fds))
        }

        /// A new region of `pool`, held by the broker and owned by this
        /// connection; free it once its pool has let go of it.
        pub fn alloc(&mut self, pool: &str, len: u64, large: bool) -> io::Result<(RegionReply, Vec<OwnedFd>)> {
            self.region(&Request::Alloc { pool: pool.into(), len, large })
        }

        /// A lease on region `id`, and its fds.
        pub fn open(&mut self, id: u64) -> io::Result<(RegionReply, Vec<OwnedFd>)> {
            self.region(&Request::Open { id })
        }

        /// A lease on the live region of `pool` (a published place such as
        /// `gpu.<k>`), and its fds.
        pub fn find(&mut self, pool: &str) -> io::Result<(RegionReply, Vec<OwnedFd>)> {
            self.region(&Request::Find { pool: pool.into() })
        }

        pub fn close(&mut self, id: u64) -> io::Result<()> {
            self.call(&Request::Close { id }).map(|_| ())
        }

        pub fn free(&mut self, id: u64) -> io::Result<()> {
            self.call(&Request::Free { id }).map(|_| ())
        }

        /// The orphans this user left in `pool`, now this connection's.
        pub fn adopt(&mut self, pool: &str) -> io::Result<Vec<(u64, u64)>> {
            let (l, _) = self.call(&Request::Adopt { pool: pool.into() })?;
            Ok(l.split(' ')
                .skip(2)
                .filter_map(|w| {
                    let (a, b) = w.split_once(':')?;
                    Some((a.parse().ok()?, b.parse().ok()?))
                })
                .collect())
        }

        /// A fresh connection to local service `name`, made by the broker.
        pub fn connect_service(&mut self, name: &str) -> io::Result<OwnedFd> {
            let (l, mut fds) = self.call(&Request::Connect { name: name.into() })?;
            if !l.starts_with("ok connect") || fds.len() != 1 {
                return Err(io::Error::other(format!("connect {name}: {l} with {} fd(s)", fds.len())));
            }
            Ok(fds.remove(0))
        }

        /// The broker's signature over a boot record, and the identity it
        /// signed it for (§111.6).
        pub fn sign(&mut self, req: &crate::proto::SignReq) -> io::Result<(wc_handle::macboot::Auth, wc_handle::macboot::Identity)> {
            let (l, _) = self.call(&Request::Sign(req.clone()))?;
            super::parse_signed(&l, &req.install).map_err(io::Error::other)
        }

        pub fn subscribe(&mut self) -> io::Result<()> {
            self.call(&Request::Subscribe).map(|_| ())
        }

        pub fn stats(&mut self) -> io::Result<String> {
            self.call(&Request::Stats).map(|(l, _)| l)
        }

        /// The next pushed line, waiting for one if none is queued.
        pub fn next_event(&mut self) -> io::Result<String> {
            if let Some(e) = self.events.pop_front() {
                return Ok(e);
            }
            loop {
                let Some(line) = recv_line(&self.s, &mut self.buf, &mut self.fds)? else {
                    return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "the broker closed the connection"));
                };
                if line.starts_with('!') {
                    return Ok(line);
                }
            }
        }
    }
}
