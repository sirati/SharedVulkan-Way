//! The keeper (waycompose-design.md §108.10): one SYSTEM process per guest that
//! creates every guest-RAM region and holds it until its provider says the
//! host has let go.
//!
//! A large-page section lives as long as any handle or view of it. The
//! provider (the GPU process, the compositor) maps a region by name, so while
//! it runs the memory has two holders. When it crashes, the keeper still holds
//! the memory, so the host's writes, which go on until it has noticed and
//! unmapped (§108.5), land in pages Windows has not given to anyone else.
//! The keeper frees a region on one event only: its provider's `free`, which a
//! provider sends only after the peer's `UNMAPPED` for that region's epoch. A
//! provider that dies leaves its regions as **orphans** of its pool. The next
//! provider of that pool adopts them, waits for `UNMAPPED`, and frees them.
//!
//! The keeper never maps the bootstrap device (the ivshmem driver allows one
//! mapping), never reads the region table, and never decides on its own that
//! a region is free. It cannot free a region early, and a region it cannot
//! free costs memory, never correctness.
//!
//! **Sections are unnamed.** No other process can open one; the keeper hands
//! each to the process that asked for it with `DuplicateHandle`, and a pool is
//! the pair (the caller's user SID, its pool name), so a process of another
//! Windows user can neither get a region nor adopt one (§108.10, R43). The
//! first build named them `Global\waycompose-ram-*` with an SDDL admitting
//! every interactive user, which would have let user B map user A's memory.
//!
//! Protocol: lines on `\\.\pipe\waycompose-ram`, one request and one answer:
//!
//! ```text
//! hello POOL            -> ok RUN [ID:LEN:HANDLE ...] orphans of (SID, POOL), now this client's;
//!                                                     HANDLE: the section, duplicated into the client
//! alloc BYTES           -> ok ID HANDLE NONCE32HEX US the section is created, stamped, held, duplicated
//!                       |  err WHY
//! free ID               -> ok | err WHY               only after UNMAPPED (the caller's duty)
//! stats                 -> ok HELD BYTES ORPHANED ORPHAN-BYTES AVAIL TOTAL
//! ```
//!
//! This is the stand-in until the shared broker crate's Windows service hosts
//! the same four operations (§108.10).

pub const PIPE: &str = r"\\.\pipe\waycompose-ram";

/// Who may *talk to* the keeper: SYSTEM, administrators and every authenticated
/// user (a batch or service logon is a Windows user too).
/// The sections themselves have no name and no one but their owner's process
/// ever gets a handle.
pub const SDDL: &str = "D:P(A;;GA;;;SY)(A;;GA;;;BA)(A;;GA;;;AU)";

pub fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

pub fn unhex16(s: &str) -> Option<[u8; 16]> {
    if s.len() != 32 {
        return None;
    }
    let mut out = [0u8; 16];
    for (i, o) in out.iter_mut().enumerate() {
        *o = u8::from_str_radix(&s[2 * i..2 * i + 2], 16).ok()?;
    }
    Some(out)
}

#[cfg(windows)]
pub use imp::*;

#[cfg(windows)]
mod imp {
    use super::*;
    use crate::winsys;
    use std::collections::BTreeMap;
    use std::io::{BufRead, BufReader, Write};
    use std::sync::{Arc, Mutex};
    use std::time::Instant;
    use sv_transport::winram::{LargeSection, enable_lock_memory, memory_status};

    struct Held {
        section: LargeSection,
        /// `SID|pool`: a pool is per Windows user.
        pool: String,
        /// The connection that owns it; `None`: an orphan.
        owner: Option<u64>,
    }

    #[derive(Default)]
    struct State {
        next_id: u32,
        held: BTreeMap<u32, Held>,
    }

    /// Run the keeper until the process is killed. Logs to stderr.
    pub fn serve() -> Result<(), String> {
        match enable_lock_memory() {
            Ok(true) => {}
            Ok(false) => return Err("this account does not hold SeLockMemoryPrivilege (run the keeper as SYSTEM)".into()),
            Err(e) => return Err(format!("enabling SeLockMemoryPrivilege: {e}")),
        }
        let run = u64::from_le_bytes(winsys::random::<8>());
        let state = Arc::new(Mutex::new(State { next_id: 1, ..Default::default() }));
        eprintln!("[keeper] run {run:016x}: serving {PIPE}");
        let mut conn = 0u64;
        loop {
            let pipe = winsys::pipe_accept(PIPE, SDDL).map_err(|e| format!("pipe: {e}"))?;
            conn += 1;
            let (st, id) = (state.clone(), conn);
            std::thread::spawn(move || {
                let r = client(pipe, run, id, &st);
                // The client is gone, whatever ended it: its regions are orphans.
                let mut s = st.lock().unwrap();
                let mut n = 0;
                for h in s.held.values_mut() {
                    if h.owner == Some(id) {
                        h.owner = None;
                        n += 1;
                    }
                }
                eprintln!("[keeper] client {id} gone ({r:?}); {n} region(s) now orphans");
            });
        }
    }

    fn client(pipe: std::fs::File, run: u64, me: u64, st: &Mutex<State>) -> std::io::Result<()> {
        // Who is asking: the process at the pipe's other end, and its user.
        let pid = winsys::pipe_client_pid(&pipe)?;
        let sid = winsys::process_sid(pid)?;
        eprintln!("[keeper] client {me}: pid {pid}, user {sid}");
        let mut out = pipe.try_clone()?;
        let mut rd = BufReader::new(pipe);
        let mut pool = String::new();
        let mut line = String::new();
        loop {
            line.clear();
            if rd.read_line(&mut line)? == 0 {
                return Ok(());
            }
            let w: Vec<&str> = line.split_whitespace().collect();
            let ans = match w.as_slice() {
                ["hello", p] => {
                    pool = format!("{sid}|{p}");
                    let mut s = st.lock().unwrap();
                    let mut list = String::new();
                    for (id, h) in s.held.iter_mut() {
                        if h.owner.is_none() && h.pool == pool {
                            match winsys::duplicate_into(pid, h.section.raw_handle()) {
                                Ok(hd) => {
                                    h.owner = Some(me);
                                    list.push_str(&format!(" {id}:{}:{hd}", h.section.len()));
                                }
                                Err(e) => eprintln!("[keeper] orphan {id} could not be handed to pid {pid}: {e}"),
                            }
                        }
                    }
                    format!("ok {run:016x}{list}")
                }
                ["alloc", bytes] if !pool.is_empty() => match bytes.parse::<u64>() {
                    Ok(len) => alloc(me, pid, &pool, len, st),
                    Err(e) => format!("err bad length: {e}"),
                },
                ["free", id] => match id.parse::<u32>() {
                    Ok(id) => {
                        let mut s = st.lock().unwrap();
                        match s.held.get(&id) {
                            Some(h) if h.owner == Some(me) => {
                                s.held.remove(&id);
                                "ok".to_string()
                            }
                            Some(_) => "err not yours".to_string(),
                            None => "err no such region".to_string(),
                        }
                    }
                    Err(e) => format!("err bad id: {e}"),
                },
                ["stats"] => {
                    let s = st.lock().unwrap();
                    let (mut n, mut b, mut on, mut ob) = (0, 0u64, 0, 0u64);
                    for h in s.held.values() {
                        n += 1;
                        b += h.section.len();
                        if h.owner.is_none() {
                            on += 1;
                            ob += h.section.len();
                        }
                    }
                    let (total, avail) = memory_status();
                    format!("ok {n} {b} {on} {ob} {avail} {total}")
                }
                _ => "err unknown request (hello first)".to_string(),
            };
            out.write_all(ans.as_bytes())?;
            out.write_all(b"\n")?;
            out.flush()?;
        }
    }

    /// Windows keeps this much physical memory for itself whatever we ask
    /// (§108.6): max(1 GiB, 15% of RAM), or `WC_RAM_RESERVE_MIB`.
    fn reserve(total: u64) -> u64 {
        if let Some(m) = std::env::var("WC_RAM_RESERVE_MIB").ok().and_then(|s| s.parse::<u64>().ok()) {
            return m << 20;
        }
        (1u64 << 30).max(total / 100 * 15)
    }

    fn alloc(me: u64, pid: u32, pool: &str, len: u64, st: &Mutex<State>) -> String {
        let (total, avail) = memory_status();
        if avail < len.saturating_add(reserve(total)) {
            return format!("err budget: {} MiB asked, {} MiB available, {} MiB kept for Windows", len >> 20, avail >> 20, reserve(total) >> 20);
        }
        let id = {
            let mut s = st.lock().unwrap();
            let id = s.next_id;
            s.next_id += 1;
            id
        };
        let nonce = winsys::random::<16>();
        let t = Instant::now();
        // Unnamed: the only way in is the handle duplicated below.
        let section = match LargeSection::create(None, len, None) {
            Ok(s) => s,
            Err(e) => return format!("err {e}"),
        };
        // Stamp through a short-lived view; the keeper holds only the handle.
        match section.map() {
            Ok(v) => v.stamp_units(&nonce),
            Err(e) => return format!("err stamping: {e}"),
        }
        let handle = match winsys::duplicate_into(pid, section.raw_handle()) {
            Ok(h) => h,
            Err(e) => return format!("err handing the section to pid {pid}: {e}"),
        };
        let us = t.elapsed().as_micros();
        st.lock().unwrap().held.insert(id, Held { section, pool: pool.to_string(), owner: Some(me) });
        format!("ok {id} {handle} {} {us}", hex(&nonce))
    }

    /// The provider's side of the keeper.
    pub struct KeeperClient {
        rd: BufReader<std::fs::File>,
        wr: std::fs::File,
        pub run: u64,
    }

    /// One region the keeper made for us.
    pub struct Granted {
        pub id: u32,
        /// The section, duplicated into this process by the keeper.
        pub handle: isize,
        pub nonce: [u8; 16],
        pub keeper_us: u64,
    }

    impl KeeperClient {
        /// Connect and say which pool we are. Returns the orphans of that pool
        /// a previous provider of this user left: `(id, len, handle)`, each
        /// section duplicated into this process.
        pub fn connect(pool: &str) -> Result<(KeeperClient, Vec<(u32, u64, isize)>), String> {
            let f = winsys::pipe_connect(PIPE, 5000).map_err(|e| format!("keeper: {e}"))?;
            let wr = f.try_clone().map_err(|e| e.to_string())?;
            let mut c = KeeperClient { rd: BufReader::new(f), wr, run: 0 };
            let a = c.ask(&format!("hello {pool}"))?;
            let mut w = a.split_whitespace();
            c.run = u64::from_str_radix(w.next().unwrap_or("0"), 16).map_err(|e| e.to_string())?;
            let orphans = w
                .filter_map(|x| {
                    let mut p = x.splitn(3, ':');
                    Some((p.next()?.parse().ok()?, p.next()?.parse().ok()?, p.next()?.parse().ok()?))
                })
                .collect();
            Ok((c, orphans))
        }

        fn ask(&mut self, req: &str) -> Result<String, String> {
            self.wr.write_all(format!("{req}\n").as_bytes()).map_err(|e| e.to_string())?;
            let mut l = String::new();
            if self.rd.read_line(&mut l).map_err(|e| e.to_string())? == 0 {
                return Err("keeper closed the pipe".into());
            }
            let l = l.trim_end();
            match l.strip_prefix("ok") {
                Some(rest) => Ok(rest.trim_start().to_string()),
                None => Err(l.strip_prefix("err ").unwrap_or(l).to_string()),
            }
        }

        pub fn alloc(&mut self, len: u64) -> Result<Granted, String> {
            let a = self.ask(&format!("alloc {len}"))?;
            let w: Vec<&str> = a.split_whitespace().collect();
            if w.len() != 4 {
                return Err(format!("keeper answered {a:?}"));
            }
            Ok(Granted {
                id: w[0].parse().map_err(|_| "bad id")?,
                handle: w[1].parse().map_err(|_| "bad handle")?,
                nonce: unhex16(w[2]).ok_or("bad nonce")?,
                keeper_us: w[3].parse().map_err(|_| "bad time")?,
            })
        }

        /// Only after the peer's `UNMAPPED` for this region (§108.4 rule 3).
        pub fn free(&mut self, id: u32) -> Result<(), String> {
            self.ask(&format!("free {id}")).map(|_| ())
        }

        pub fn stats(&mut self) -> Result<String, String> {
            self.ask("stats")
        }
    }
}
