//! The holder (§111.8): a second SYSTEM process that holds a copy of every
//! region the broker made, and nothing else.
//!
//! A section lives as long as any handle or view of it. Its provider (the GPU
//! process, the compositor) holds one, and the broker holds one, so that a
//! provider's crash leaves the region alive for the host, which may still be
//! writing into it (§108.10, the keeper rule). Without the holder, a crash of
//! the broker itself while a provider is down would free memory the host
//! still maps. With it, a region is lost only if the broker, the holder and
//! every provider die together.
//!
//! * The broker starts the holder (`sv-guest broker holder`) if it is not
//!   running, connects to its pipe, and puts a copy of each new section into
//!   it with `DuplicateHandle` (`add`), and takes it back out (`del`) when the
//!   region is freed.
//! * A restarted broker asks for the list (`list`), duplicates each section
//!   back out of the holder, and serves it as an orphan of its user's pool
//!   (`Core::restore_orphan`).
//! * A holder that died is started again and given a copy of every region the
//!   broker holds.
//!
//! The holder never maps a section, never reads one, and answers only on a
//! pipe that admits SYSTEM (and the account that made it, for tests).
//!
//! ```text
//! add ID LEN POOL USER HANDLE   -> ok      HANDLE is already in the holder
//! del ID                        -> ok
//! list                          -> ok N, then N lines ID LEN POOL USER HANDLE
//! quit                          -> ok only when empty; otherwise err retained
//! ```

use super::ident::{self, Owned};
use super::pipe::{Listener, ServerPipe};
use crate::core::{ObjId, RegionInfo, User};
use std::collections::BTreeMap;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::os::windows::io::{FromRawHandle, RawHandle};
use std::sync::{Arc, Mutex};
use windows_sys::Win32::Foundation::{
    DUPLICATE_CLOSE_SOURCE, DUPLICATE_SAME_ACCESS, DuplicateHandle, GENERIC_READ, GENERIC_WRITE, HANDLE,
    INVALID_HANDLE_VALUE,
};
use windows_sys::Win32::Storage::FileSystem::{CreateFileW, OPEN_EXISTING};
use windows_sys::Win32::System::Pipes::GetNamedPipeServerProcessId;
use windows_sys::Win32::System::Threading::{GetCurrentProcess, INFINITE, WaitForSingleObject};

pub const HOLDER_PIPE: &str = r"\\.\pipe\waycompose-broker-holder";
/// SYSTEM, elevated Administrators for checked uninstall, and the pipe's
/// owner for tests. A filtered/disabled Administrators SID grants no access.
/// Administrators already control the service and its protected data.
pub const HOLDER_SDDL: &str = "D:P(A;;GA;;;SY)(A;;GA;;;BA)(A;;GA;;;OW)";

// ------------------------------------------------------------ the holder

struct Kept {
    len: u64,
    pool: String,
    user: String,
    handle: Owned,
}

/// `sv-guest broker holder`: serve the broker until `quit`.
pub fn serve(pipe: &str, sddl: &str) -> io::Result<()> {
    let listener = Listener::new(pipe, sddl)?;
    let mut kept: BTreeMap<u64, Kept> = BTreeMap::new();
    eprintln!("[wc-broker holder] serving {pipe}");
    loop {
        let p = listener.accept()?;
        // Only our own account (SYSTEM for the service) may talk to it; the
        // descriptor admits no one else, and this says who came.
        let who = ident::pipe_client(p.handle).map(|x| x.1).unwrap_or_default();
        match conversation(&p, &mut kept) {
            Ok(true) => {
                eprintln!("[wc-broker holder] empty holder quit");
                return Ok(());
            }
            Ok(false) | Err(_) => {
                eprintln!("[wc-broker holder] the broker ({who}) went away; holding {} region(s)", kept.len());
            }
        }
    }
}

/// One broker's connection. Ok(true): `quit`.
fn conversation(p: &ServerPipe, kept: &mut BTreeMap<u64, Kept>) -> io::Result<bool> {
    let mut rd = BufReader::new(Reader(p));
    let mut line = String::new();
    loop {
        line.clear();
        if rd.read_line(&mut line)? == 0 {
            return Ok(false);
        }
        let w: Vec<&str> = line.trim_end().split(' ').collect();
        let mut out = String::from("ok\n");
        match w.as_slice() {
            ["add", id, len, pool, user, h] => {
                let (Ok(id), Ok(len), Some(h)) = (id.parse(), len.parse(), parse_handle(h)) else {
                    out = "err bad add\n".into();
                    write(p, &out)?;
                    continue;
                };
                kept.insert(id, Kept { len, pool: pool.to_string(), user: user.to_string(), handle: Owned(h) });
            }
            ["del", id] => {
                if let Ok(id) = id.parse::<u64>() {
                    kept.remove(&id);
                }
            }
            ["list"] => {
                out = format!("ok {}\n", kept.len());
                for (id, k) in kept.iter() {
                    out.push_str(&format!("{id} {} {} {} {:#x}\n", k.len, k.pool, k.user, k.handle.0 as usize));
                }
            }
            ["quit"] => {
                // Requests are serialized on this connection. No add or del
                // can race the empty check and successful shutdown reply.
                if !kept.is_empty() {
                    write(p, &format!("err retained {} region(s); normal release required\n", kept.len()))?;
                    continue;
                }
                write(p, "ok\n")?;
                return Ok(true);
            }
            _ => out = "err unknown\n".into(),
        }
        write(p, &out)?;
    }
}

struct Reader<'a>(&'a ServerPipe);

impl Read for Reader<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        (&*self.0).read(buf)
    }
}

fn write(p: &ServerPipe, s: &str) -> io::Result<()> {
    (&*p).write_all(s.as_bytes())
}

fn parse_handle(s: &str) -> Option<HANDLE> {
    let v = usize::from_str_radix(s.strip_prefix("0x")?, 16).ok().filter(|v| *v != 0)?;
    Some(v as HANDLE)
}

// ------------------------------------------------------- the broker's side

/// How to start a holder: a program and its arguments.
#[derive(Clone, Debug)]
pub struct Spawn {
    pub exe: std::path::PathBuf,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
}

struct Conn {
    rd: BufReader<std::fs::File>,
    wr: std::fs::File,
    process: Arc<Owned>,
}

/// The broker's link to its holder.
pub struct Link {
    pipe: String,
    spawn: Spawn,
    conn: Mutex<Option<Conn>>,
    /// What the holder must hold: the broker's own handle on each region
    /// (valid until `del`), so a new holder can be refilled.
    regions: Mutex<BTreeMap<ObjId, (RegionInfo, User, usize)>>,
}

fn open_pipe(pipe: &str) -> io::Result<std::fs::File> {
    let w = super::pipe::wide(pipe);
    // SAFETY: a live name; no attributes, no template.
    let h = unsafe { CreateFileW(w.as_ptr(), GENERIC_READ | GENERIC_WRITE, 0, std::ptr::null(), OPEN_EXISTING, 0, std::ptr::null_mut()) };
    if h == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: just opened, owned from here.
    Ok(unsafe { std::fs::File::from_raw_handle(h as RawHandle) })
}

impl Link {
    /// Connect to the holder on `pipe`, starting it with `spawn` if it is not
    /// running.
    pub fn start(pipe: &str, spawn: Spawn) -> io::Result<Arc<Link>> {
        let l = Arc::new(Link { pipe: pipe.to_string(), spawn, conn: Mutex::new(None), regions: Mutex::new(BTreeMap::new()) });
        l.connect()?;
        let me = Arc::clone(&l);
        std::thread::Builder::new().name("wc-broker-holder".into()).spawn(move || me.watch())?;
        Ok(l)
    }

    fn connect(&self) -> io::Result<()> {
        let first = {
            let t = std::time::Instant::now();
            loop {
                match open_pipe(&self.pipe) {
                    // Busy: the holder still serves a broker that just died.
                    Err(e) if e.raw_os_error() == Some(231) && t.elapsed() < std::time::Duration::from_secs(5) => {
                        std::thread::sleep(std::time::Duration::from_millis(20));
                    }
                    r => break r,
                }
            }
        };
        let f = match first {
            Ok(f) => f,
            Err(e) if e.raw_os_error() != Some(2) => return Err(e),
            Err(_) => {
                let mut c = std::process::Command::new(&self.spawn.exe);
                c.args(&self.spawn.args).stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null());
                for (k, v) in &self.spawn.env {
                    c.env(k, v);
                }
                {
                    use std::os::windows::process::CommandExt;
                    // CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP: no console,
                    // and no Ctrl-C of the broker's console reaches it.
                    c.creation_flags(0x0800_0000 | 0x0000_0200);
                }
                let child = c.spawn()?;
                eprintln!("[wc-broker] holder started, process {}", child.id());
                let t = std::time::Instant::now();
                loop {
                    match open_pipe(&self.pipe) {
                        Ok(f) => break f,
                        Err(e) if t.elapsed() > std::time::Duration::from_secs(10) => {
                            return Err(io::Error::new(e.kind(), format!("the holder's pipe {} never appeared: {e}", self.pipe)));
                        }
                        Err(_) => std::thread::sleep(std::time::Duration::from_millis(10)),
                    }
                }
            }
        };
        let mut pid = 0u32;
        // SAFETY: a live client end of a pipe.
        if unsafe { GetNamedPipeServerProcessId(f.as_raw() as HANDLE, &mut pid) } == 0 {
            return Err(io::Error::last_os_error());
        }
        // The holder must be our own account's process: a pipe of that name
        // made by anyone else (who could not get the SDDL past the first
        // instance anyway) is refused here.
        let me = ident::current_user()?;
        let process = Arc::new(ident::open_process_checked(pid, &me)?);
        let wr = f.try_clone()?;
        *self.conn.lock().unwrap_or_else(|e| e.into_inner()) = Some(Conn { rd: BufReader::new(f), wr, process });
        Ok(())
    }

    fn ask(&self, req: &str) -> io::Result<Vec<String>> {
        let mut g = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        let c = g.as_mut().ok_or_else(|| io::Error::new(io::ErrorKind::NotConnected, "no holder"))?;
        c.wr.write_all(format!("{req}\n").as_bytes())?;
        let mut l = String::new();
        if c.rd.read_line(&mut l)? == 0 {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "the holder closed its pipe"));
        }
        let l = l.trim_end().to_string();
        let n = match l.strip_prefix("ok ") {
            Some(n) if req == "list" => n.parse::<usize>().unwrap_or(0),
            _ if l == "ok" => 0,
            _ => return Err(io::Error::other(format!("holder: {l}"))),
        };
        let mut out = Vec::with_capacity(n);
        for _ in 0..n {
            let mut x = String::new();
            c.rd.read_line(&mut x)?;
            out.push(x.trim_end().to_string());
        }
        Ok(out)
    }

    fn process(&self) -> Option<Arc<Owned>> {
        self.conn.lock().unwrap_or_else(|e| e.into_inner()).as_ref().map(|c| Arc::clone(&c.process))
    }

    /// Put a copy of region `info`'s section `h` into the holder.
    pub fn add(&self, info: &RegionInfo, user: &User, h: HANDLE) -> io::Result<()> {
        self.regions.lock().unwrap_or_else(|e| e.into_inner()).insert(info.id, (info.clone(), user.clone(), h as usize));
        self.give(info, user, h)
    }

    fn give(&self, info: &RegionInfo, user: &User, h: HANDLE) -> io::Result<()> {
        let p = self.process().ok_or_else(|| io::Error::new(io::ErrorKind::NotConnected, "no holder"))?;
        let mut theirs: HANDLE = std::ptr::null_mut();
        // SAFETY: a live handle of this process and a live process handle
        // with PROCESS_DUP_HANDLE.
        if unsafe { DuplicateHandle(GetCurrentProcess(), h, p.0, &mut theirs, 0, 0, DUPLICATE_SAME_ACCESS) } == 0 {
            return Err(io::Error::last_os_error());
        }
        let r = self.ask(&format!("add {} {} {} {} {:#x}", info.id, info.len, info.pool, user.0, theirs as usize));
        if r.is_err() {
            // SAFETY: the copy just made in the holder, which never learned of it.
            unsafe { DuplicateHandle(p.0, theirs, std::ptr::null_mut(), std::ptr::null_mut(), 0, 0, DUPLICATE_CLOSE_SOURCE) };
        }
        r.map(|_| ())
    }

    pub fn del(&self, id: ObjId) {
        if self.regions.lock().unwrap_or_else(|e| e.into_inner()).remove(&id).is_some()
            && let Err(e) = self.ask(&format!("del {id}"))
        {
            eprintln!("[wc-broker] holder: del {id}: {e}");
        }
    }

    /// The regions a previous broker left with the holder, each with a
    /// section handle of this process's own.
    pub fn restore(&self) -> Vec<(RegionInfo, User, HANDLE)> {
        let Some(p) = self.process() else { return Vec::new() };
        let lines = match self.ask("list") {
            Ok(l) => l,
            Err(e) => {
                eprintln!("[wc-broker] holder: list: {e}");
                return Vec::new();
            }
        };
        let mut out = Vec::new();
        for l in lines {
            let w: Vec<&str> = l.split(' ').collect();
            let [id, len, pool, user, h] = w.as_slice() else { continue };
            let (Ok(id), Ok(len), Some(h)) = (id.parse::<u64>(), len.parse::<u64>(), parse_handle(h)) else { continue };
            let mut mine: HANDLE = std::ptr::null_mut();
            // SAFETY: a handle value in the holder's process, which this
            // process may duplicate from (PROCESS_DUP_HANDLE).
            if unsafe { DuplicateHandle(p.0, h, GetCurrentProcess(), &mut mine, 0, 0, DUPLICATE_SAME_ACCESS) } == 0 {
                eprintln!("[wc-broker] holder: region {id}: {}", io::Error::last_os_error());
                continue;
            }
            let info = RegionInfo { id, pool: pool.to_string(), len, epoch: 1 };
            let user = User(user.to_string());
            self.regions.lock().unwrap_or_else(|e| e.into_inner()).insert(id, (info.clone(), user.clone(), mine as usize));
            out.push((info, user, mine));
        }
        out
    }

    /// Wait for the holder's exit; start a new one and give it every region.
    fn watch(&self) {
        loop {
            let Some(p) = self.process() else { return };
            // SAFETY: a live process handle with SYNCHRONIZE.
            unsafe { WaitForSingleObject(p.0, INFINITE) };
            eprintln!("[wc-broker] the holder exited; starting another and giving it every region");
            *self.conn.lock().unwrap_or_else(|e| e.into_inner()) = None;
            let mut tries = 0;
            while let Err(e) = self.connect() {
                tries += 1;
                eprintln!("[wc-broker] holder: {e}");
                if tries > 50 {
                    return;
                }
                std::thread::sleep(std::time::Duration::from_millis(200));
            }
            // Keep the registry locked while duplicating its borrowed raw
            // handles. region_destroyed removes an entry before closing the
            // broker's handle; it must not close or reuse a value from a
            // refill snapshot before we have duplicated it.
            let all = self.regions.lock().unwrap_or_else(|e| e.into_inner());
            for (info, user, h) in all.values() {
                if let Err(e) = self.give(info, user, *h as HANDLE) {
                    eprintln!("[wc-broker] holder: refilling region {}: {e}", info.id);
                }
            }
            eprintln!("[wc-broker] holder refilled with {} region(s)", all.len());
        }
    }

    /// The holder's process id (for tests and logs).
    pub fn holder_pid(&self) -> Option<u32> {
        let p = self.process()?;
        // SAFETY: a live process handle.
        Some(unsafe { windows_sys::Win32::System::Threading::GetProcessId(p.0) })
    }
}

trait AsRaw {
    fn as_raw(&self) -> RawHandle;
}

impl AsRaw for std::fs::File {
    fn as_raw(&self) -> RawHandle {
        use std::os::windows::io::AsRawHandle;
        self.as_raw_handle()
    }
}

/// Ask an empty holder to exit. A retained region is never released by
/// shutdown, timeout or uninstall; only the normal release path removes it.
pub fn quit(pipe: &str) -> io::Result<()> {
    // The holder serves one broker at a time: a broker that just exited may
    // still be its client for a moment.
    let t = std::time::Instant::now();
    let mut f = loop {
        match open_pipe(pipe) {
            Ok(f) => break f,
            Err(e) if e.raw_os_error() == Some(231) && t.elapsed() < std::time::Duration::from_secs(5) => {
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            Err(e) => return Err(e),
        }
    };
    f.write_all(b"quit\n")?;
    let mut s = String::new();
    BufReader::new(f).read_line(&mut s)?;
    if s != "ok\n" && s != "ok\r\n" {
        return Err(io::Error::other(format!("holder refused quit: {}", s.trim_end())));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows_sys::Win32::System::Memory::{CreateFileMappingW, FILE_MAP_ALL_ACCESS, MapViewOfFile, PAGE_READWRITE, UnmapViewOfFile};

    #[test]
    fn retained_section_refuses_quit_and_preserves_bytes_until_del() {
        let pipe = format!(r"\\.\pipe\wc-holder-quit-test-{}", std::process::id());
        let endpoint = pipe.clone();
        let worker = std::thread::spawn(move || serve(&endpoint, HOLDER_SDDL).unwrap());
        let until = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let mut f = loop {
            match open_pipe(&pipe) {
                Ok(f) => break f,
                Err(_) if std::time::Instant::now() < until => std::thread::sleep(std::time::Duration::from_millis(10)),
                Err(e) => panic!("holder did not start: {e}"),
            }
        };
        // SAFETY: an unnamed private section; the holder takes sole ownership
        // of the handle through add below, as a transferred duplicate would.
        let h = unsafe { CreateFileMappingW(INVALID_HANDLE_VALUE, std::ptr::null(), PAGE_READWRITE, 0, 4096, std::ptr::null()) };
        assert!(!h.is_null());
        let view = unsafe { MapViewOfFile(h, FILE_MAP_ALL_ACCESS, 0, 0, 4096) };
        assert!(!view.Value.is_null());
        unsafe { std::ptr::write_volatile(view.Value.cast::<u8>(), 0x6d); UnmapViewOfFile(view); }
        writeln!(f, "add 1 4096 test sid:S-1-5-18 {:#x}", h as usize).unwrap();
        let mut rd = BufReader::new(f);
        let mut reply = String::new();
        rd.read_line(&mut reply).unwrap();
        assert_eq!(reply, "ok\n");
        drop(rd);
        assert!(quit(&pipe).unwrap_err().to_string().contains("retained 1 region"));
        assert!(!worker.is_finished());
        // The temporary view was unmapped before quit. Only the retained
        // handle now keeps the section and its bytes alive.
        let view = unsafe { MapViewOfFile(h, FILE_MAP_ALL_ACCESS, 0, 0, 4096) };
        assert!(!view.Value.is_null());
        assert_eq!(unsafe { std::ptr::read_volatile(view.Value.cast::<u8>()) }, 0x6d);
        unsafe { UnmapViewOfFile(view); }
        let mut f = loop {
            match open_pipe(&pipe) {
                Ok(f) => break f,
                Err(e) if e.raw_os_error() == Some(231) && std::time::Instant::now() < until =>
                    std::thread::sleep(std::time::Duration::from_millis(10)),
                Err(e) => panic!("holder unavailable after refused quit: {e}"),
            }
        };
        f.write_all(b"del 1\n").unwrap();
        let mut rd = BufReader::new(f);
        reply.clear();
        rd.read_line(&mut reply).unwrap();
        assert_eq!(reply, "ok\n");
        drop(rd);
        quit(&pipe).unwrap();
        worker.join().unwrap();
    }
}
