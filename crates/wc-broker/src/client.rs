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

        /// End this process when this user's pool `pool` is released (§111.9:
        /// a macOS session's GPU server, once the host has stopped serving it).
        pub fn tie(&mut self, pool: &str) -> io::Result<()> {
            self.call(&Request::Tie { pool: pool.into() }).map(|_| ())
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
