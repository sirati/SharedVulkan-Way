//! §105: the Linux end of host integration. The proxy claims the
//! `OWNER_HOSTINT` ring slot when the session has rings, as it claims the
//! control and sound slots (`macsvc`), and serves it on threads of its own:
//!
//! * **paths**: the host's share map. Kept for `wc-host-open`, and its Linux
//!   roots written to `DIR/hostint/roots`, which `wc-session open` hands to
//!   `wc-proxy desktop --file-roots`: a `file:///` URL from the host opens only
//!   if it is inside a share (the host is the less trusted side for Linux's
//!   files).
//! * **handlers**: the host's applications per type, written as desktop
//!   entries (`wc-host-<token>.desktop`, `wc-host-browser.desktop`) under
//!   `DIR/hostint/xdg-data/applications` and a `mimeapps.list` under
//!   `DIR/hostint/xdg-config`, which `wc-session` puts first in the
//!   applications' `XDG_DATA_DIRS` and `XDG_CONFIG_DIRS`. Never the user's
//!   own `~/.config` or `~/.local`.
//! * **audio**: the host's devices, as one PipeWire node each, hosted by one
//!   child `pipewire -c DIR/hostint/audio/nodes.conf` that connects to the
//!   session's PipeWire daemon (`--hostint-pipewire`, never inherited from the
//!   environment). Defaults follow the host through `pw-metadata`.
//! * **requests**: `wc-host-open` connects to `DIR/hostint.sock` (0600, the
//!   peer's uid checked), sends one `req`, and gets the host's answer. The
//!   proxy numbers the request, forwards it on the ring and waits up to 5 s
//!   for `rep N`.
//!
//! Nothing here is a socket to the host. `hostint.sock` and the FIFOs are
//! local to Linux; everything that reaches the host crosses on the ring slot.
//!
//! # The sound ring has not landed
//!
//! Each node's samples pass through a FIFO (`wc_hostint::audio::fifo_path`)
//! and a bridge thread copies between that FIFO and a [`SoundRing`] stream.
//! The generic multi-stream sound ring is not in this tree yet, so the only
//! implementation is [`NullRing`]: an output's samples are drained and
//! dropped, an input gets silence at the device's rate. The session's default
//! output still reaches the host on the existing single `OWNER_AUDIO` channel
//! (`macsvc`), from the session sound server's monitor on `audio.sock`. When
//! the sound ring lands, it implements [`SoundRing`] and nothing else here
//! changes.

use std::collections::HashMap;
use std::ffi::CString;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use sv_transport::control::Control;
use sv_transport::wlbytes::{ByteChannel, ReadOutcome, OWNER_HOSTINT};
use wc_hostint::audio::{self, Dir, Node, Reconciler, State};
use wc_hostint::linux::{self, Given, Plan};
use wc_hostint::wire::{self, Handler, LineBuf, Msg, PathMap, Req, Target, VERSION};

/// How long `wc-host-open` waits for the host's answer.
pub const REPLY_TIMEOUT: Duration = Duration::from_secs(5);
/// The longest line on the local socket.
const MAX_LOCAL_LINE: usize = 64 * 1024;

/// What `wc-session proxy` tells this process.
#[derive(Clone, Debug, Default)]
pub struct Config {
    /// The session's run directory (`--hostint DIR`). Everything this service
    /// writes is under it.
    pub dir: PathBuf,
    /// The session's PipeWire socket (`--hostint-pipewire PATH`). Without it
    /// the host's devices are not made into nodes, and it says so.
    pub pipewire: Option<PathBuf>,
    /// Where `pipewire`, `pw-dump` and `pw-metadata` are (`--hostint-pw-bin`);
    /// `PATH` if unset.
    pub pw_bin: Option<PathBuf>,
    /// `wc-host-open`, for the desktop entries' `Exec=` (`--hostint-opener`);
    /// beside this executable if unset.
    pub opener: Option<PathBuf>,
}

impl Config {
    fn hdir(&self) -> PathBuf {
        self.dir.join("hostint")
    }
    pub fn socket(&self) -> PathBuf {
        self.dir.join("hostint.sock")
    }
    fn opener(&self) -> PathBuf {
        self.opener.clone().unwrap_or_else(|| {
            std::env::current_exe()
                .ok()
                .and_then(|e| e.parent().map(|p| p.join("wc-host-open")))
                .unwrap_or_else(|| PathBuf::from("wc-host-open"))
        })
    }
}

fn log(s: &str) {
    println!("[proxy] hostint: {s}");
}

/// The host's name for people ("Windows", "macOS") and for node names
/// ("win", "mac"), from its `hello`.
pub fn host_names(platform: Option<&str>) -> (&'static str, &'static str) {
    match platform {
        Some("windows") => ("Windows", "win"),
        Some("macos") => ("macOS", "mac"),
        _ => ("host", "host"),
    }
}

// ------------------------------------------------------------ ring wait --

/// Sleep until the ring may have bytes, for at most `t`.
///
/// Sleep on the lane's doorbell (§103.15), for at most one service slice:
/// the doorbell ends the sleep early, and without one the slice is short.
fn wait_ring(ch: &ByteChannel, t: Duration) {
    let _ = ch.wait_readable(t.min(crate::rings::service_slice()));
}

// --------------------------------------------------------------- shared --

#[derive(Default)]
struct Shared {
    platform: Option<String>,
    paths: PathMap,
    handlers: Vec<Handler>,
    pending: HashMap<u64, Sender<Result<(), String>>>,
    next: u64,
    /// The ring is gone: requests are answered at once.
    closed: bool,
}

/// A running service. Dropping it does not stop it; [`Handle::stop`] does.
pub struct Handle {
    stop: Arc<AtomicBool>,
    threads: Vec<std::thread::JoinHandle<()>>,
    socket: PathBuf,
}

impl Handle {
    /// End every thread, the PipeWire child with them, and remove the socket.
    pub fn stop(self) {
        self.stop.store(true, Ordering::SeqCst);
        // Wake the accept loop.
        let _ = UnixStream::connect(&self.socket);
        for t in self.threads {
            let _ = t.join();
        }
        let _ = std::fs::remove_file(&self.socket);
    }
}

/// Claim the slot and start the service. Never fatal: a session without host
/// integration is still a session.
pub fn start(control: &Arc<Control>, cfg: Config) -> Option<Handle> {
    match ByteChannel::claim(control, OWNER_HOSTINT) {
        Ok(ch) => {
            println!("[proxy] host integration on ring slot {} (no ssh)", ch.slot());
            match start_on(ch, cfg) {
                Ok(h) => Some(h),
                Err(e) => {
                    log(&format!("NOT STARTED: {e}"));
                    None
                }
            }
        }
        Err(e) => {
            println!("[proxy] no host-integration ring: {e}");
            None
        }
    }
}

/// The service on a channel already claimed.
pub fn start_on(ch: ByteChannel, cfg: Config) -> std::io::Result<Handle> {
    let h = cfg.hdir();
    for d in [h.clone(), h.join("xdg-data/applications"), h.join("xdg-config"), h.join("audio")] {
        std::fs::DirBuilder::new().recursive(true).mode(0o700).create(&d)?;
    }
    write_atomic(&h.join("roots"), "")?;
    let shared = Arc::new(Mutex::new(Shared::default()));
    let stop = Arc::new(AtomicBool::new(false));
    let (out_tx, out_rx) = mpsc::channel::<String>();
    let (audio_tx, audio_rx) = mpsc::channel::<AudioCmd>();

    let listener = bind_socket(&cfg)?;
    let mut threads = Vec::new();
    {
        let (cfg, shared, stop) = (cfg.clone(), Arc::clone(&shared), Arc::clone(&stop));
        threads.push(
            std::thread::Builder::new()
                .name("hostint-ring".into())
                .spawn(move || ring_loop(ch, &cfg, &shared, &out_rx, &audio_tx, &stop))?,
        );
    }
    {
        let (cfg, stop) = (cfg.clone(), Arc::clone(&stop));
        threads.push(
            std::thread::Builder::new()
                .name("hostint-audio".into())
                .spawn(move || audio_loop(&cfg, &audio_rx, &stop, &NullRing))?,
        );
    }
    {
        let (cfg, shared, stop) = (cfg.clone(), Arc::clone(&shared), Arc::clone(&stop));
        threads.push(
            std::thread::Builder::new()
                .name("hostint-socket".into())
                .spawn(move || accept_loop(listener, &cfg, &shared, &out_tx, &stop))?,
        );
    }
    Ok(Handle { stop, threads, socket: cfg.socket() })
}

// ----------------------------------------------------------------- ring --

fn write_line(ch: &mut ByteChannel, line: &str) -> bool {
    let mut b = Vec::with_capacity(line.len() + 1);
    b.extend_from_slice(line.as_bytes());
    b.push(b'\n');
    let mut rest = &b[..];
    let t0 = Instant::now();
    while !rest.is_empty() {
        match ch.write(rest) {
            Ok(n) if n > 0 => rest = &rest[n..],
            // A full ring: the host is slow. Two seconds, then it is gone.
            Ok(_) if t0.elapsed() < Duration::from_secs(2) => std::thread::sleep(Duration::from_millis(2)),
            _ => return false,
        }
    }
    true
}

fn ring_loop(
    mut ch: ByteChannel,
    cfg: &Config,
    shared: &Mutex<Shared>,
    out: &Receiver<String>,
    audio: &Sender<AudioCmd>,
    stop: &AtomicBool,
) {
    let mut lines = LineBuf::default();
    let mut tmp = [0u8; 8192];
    let alive = write_line(&mut ch, &Msg::Hello { version: VERSION, platform: None }.encode());
    if alive {
        log("hello sent; waiting for the host");
    }
    while alive && !stop.load(Ordering::SeqCst) {
        let mut busy = false;
        while let Ok(l) = out.try_recv() {
            busy = true;
            if !write_line(&mut ch, &l) {
                break;
            }
        }
        match ch.read(&mut tmp) {
            Ok(ReadOutcome::Bytes(n)) => {
                busy = true;
                lines.push(&tmp[..n]);
                while let Some(l) = lines.next_line() {
                    on_host_line(&l, cfg, shared, audio);
                }
            }
            Ok(ReadOutcome::Eof) | Err(_) => break,
            Ok(_) => {}
        }
        if !busy {
            wait_ring(&ch, Duration::from_millis(50));
        }
    }
    log("the channel ended");
    let mut s = shared.lock().unwrap_or_else(|e| e.into_inner());
    s.closed = true;
    for (_, tx) in s.pending.drain() {
        let _ = tx.send(Err("the host-integration channel ended".into()));
    }
    let _ = audio.send(AudioCmd::Stop);
}

fn on_host_line(line: &str, cfg: &Config, shared: &Mutex<Shared>, audio: &Sender<AudioCmd>) {
    let msg = match Msg::decode(line) {
        Ok(m) => m,
        Err(e) => {
            log(&format!("ignored from the host: {e}"));
            return;
        }
    };
    match msg {
        Msg::Hello { version, platform } => {
            log(&format!("host says hello {version} {}", platform.as_deref().unwrap_or("-")));
            let handlers = {
                let mut s = shared.lock().unwrap_or_else(|e| e.into_inner());
                s.platform = platform.clone();
                s.handlers.clone()
            };
            let _ = audio.send(AudioCmd::Prefix(host_names(platform.as_deref()).1.into()));
            if let Err(e) = write_handlers(cfg, &handlers, platform.as_deref()) {
                log(&format!("desktop entries: {e}"));
            }
        }
        Msg::Paths(p) => {
            let roots: Vec<&str> = p
                .shares
                .iter()
                .map(|(_, l)| l.as_str())
                .filter(|l| l.starts_with('/') && !l.contains(':') && !l.contains('\n'))
                .collect();
            if let Err(e) = write_atomic(&cfg.hdir().join("roots"), &format!("{}\n", roots.join("\n"))) {
                log(&format!("roots: {e}"));
            }
            log(&format!("{} share(s): {}", p.shares.len(), roots.join(", ")));
            shared.lock().unwrap_or_else(|e| e.into_inner()).paths = p;
        }
        Msg::Handlers(h) => {
            let platform = {
                let mut s = shared.lock().unwrap_or_else(|e| e.into_inner());
                s.handlers = h.clone();
                s.platform.clone()
            };
            match write_handlers(cfg, &h, platform.as_deref()) {
                Ok(()) => log(&format!("{} host application(s) as desktop entries", h.len())),
                Err(e) => log(&format!("desktop entries: {e}")),
            }
        }
        Msg::Audio(st) => {
            let _ = audio.send(AudioCmd::State(st));
        }
        Msg::Rep { n, result } => {
            let tx = shared.lock().unwrap_or_else(|e| e.into_inner()).pending.remove(&n);
            match tx {
                Some(tx) => {
                    let _ = tx.send(result);
                }
                None => log(&format!("a reply to no request ({n})")),
            }
        }
        // Requests from the host to Linux go on the control channel's `open`.
        Msg::Req { .. } => log("ignored: a request from the host on this channel"),
    }
}

// ------------------------------------------------------- desktop entries --

fn write_atomic(path: &Path, text: &str) -> std::io::Result<()> {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, text)?;
    std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
    std::fs::rename(&tmp, path)
}

/// The browser entry, one entry per host application, and `mimeapps.list`.
/// Entries of handlers the host no longer offers are removed.
pub fn write_handlers(cfg: &Config, handlers: &[Handler], platform: Option<&str>) -> std::io::Result<()> {
    let host = host_names(platform).0;
    let opener = cfg.opener();
    let opener = opener.to_string_lossy();
    let apps = cfg.hdir().join("xdg-data/applications");
    let mut keep = vec![linux::BROWSER_ENTRY.to_string()];
    write_atomic(&apps.join(linux::BROWSER_ENTRY), &linux::browser_entry(&opener, host))?;
    for h in handlers {
        let name = linux::handler_entry(&h.token);
        write_atomic(&apps.join(&name), &linux::handler_desktop_entry(h, &opener, host))?;
        keep.push(name);
    }
    for e in std::fs::read_dir(&apps)?.flatten() {
        let n = e.file_name().to_string_lossy().into_owned();
        if n.starts_with("wc-host-") && n.ends_with(".desktop") && !keep.contains(&n) {
            let _ = std::fs::remove_file(e.path());
        }
    }
    write_atomic(&cfg.hdir().join("xdg-config/mimeapps.list"), &linux::mimeapps(handlers))?;
    // The cache is an optimisation; without the tool the entries still work.
    if let Some(udd) = find_in_path("update-desktop-database") {
        let _ = Command::new(udd).arg("-q").arg(&apps).stdin(Stdio::null()).status();
    }
    Ok(())
}

fn find_in_path(name: &str) -> Option<PathBuf> {
    std::env::var_os("PATH")?
        .to_string_lossy()
        .split(':')
        .map(|d| Path::new(d).join(name))
        .find(|p| p.is_file())
}

// --------------------------------------------------------- local socket --

fn bind_socket(cfg: &Config) -> std::io::Result<UnixListener> {
    let p = cfg.socket();
    let _ = std::fs::remove_file(&p);
    let l = UnixListener::bind(&p)?;
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o600))?;
    // A proxy that runs as root (the guest-RAM path) gives the socket to the
    // session's user, who owns the run directory.
    let owner = std::fs::metadata(&cfg.dir)?.uid();
    // SAFETY: plain syscalls on a path we just made.
    unsafe {
        if libc::geteuid() == 0 && owner != 0 {
            let c = CString::new(p.as_os_str().as_encoded_bytes()).unwrap_or_default();
            libc::chown(c.as_ptr(), owner, u32::MAX);
        }
    }
    Ok(l)
}

fn peer_uid(s: &UnixStream) -> Option<u32> {
    let mut cred = libc::ucred { pid: 0, uid: 0, gid: 0 };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    // SAFETY: a live socket and a buffer of the size we pass.
    let r = unsafe {
        libc::getsockopt(
            s.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&mut cred as *mut libc::ucred).cast(),
            &mut len,
        )
    };
    (r == 0).then_some(cred.uid)
}

fn accept_loop(l: UnixListener, cfg: &Config, shared: &Arc<Mutex<Shared>>, out: &Sender<String>, stop: &Arc<AtomicBool>) {
    let owner = std::fs::metadata(&cfg.dir).map(|m| m.uid()).unwrap_or(u32::MAX);
    // SAFETY: no arguments.
    let me = unsafe { libc::geteuid() };
    for c in l.incoming() {
        if stop.load(Ordering::SeqCst) {
            break;
        }
        let Ok(c) = c else { continue };
        match peer_uid(&c) {
            Some(u) if u == owner || u == me => {}
            other => {
                log(&format!("refused a local connection from uid {other:?} (the session's is {owner})"));
                continue;
            }
        }
        let (shared, out) = (Arc::clone(shared), out.clone());
        let _ = std::thread::Builder::new().name("hostint-client".into()).spawn(move || serve_client(c, &shared, &out));
    }
}

/// One `wc-host-open`: `hello` gets `host PLATFORM` and `paths B64`; each
/// `req N …` gets `rep N ok` or `rep N err B64`.
fn serve_client(c: UnixStream, shared: &Mutex<Shared>, out: &Sender<String>) {
    let _ = c.set_read_timeout(Some(Duration::from_secs(30)));
    let Ok(mut w) = c.try_clone() else { return };
    let mut r = BufReader::new(c).take(1 << 20);
    let mut line = String::new();
    loop {
        line.clear();
        match r.read_line(&mut line) {
            Ok(0) | Err(_) => return,
            Ok(n) if n > MAX_LOCAL_LINE => return,
            Ok(_) => {}
        }
        let l = line.trim_end_matches('\n');
        let reply = if l == "hello" {
            let s = shared.lock().unwrap_or_else(|e| e.into_inner());
            format!(
                "host {}\n{}",
                s.platform.as_deref().unwrap_or("-"),
                Msg::Paths(s.paths.clone()).encode()
            )
        } else {
            match Msg::decode(l) {
                Ok(Msg::Req { n, req }) => Msg::Rep { n, result: forward(req, shared, out) }.encode(),
                _ => Msg::Rep { n: 0, result: Err("not a request".into()) }.encode(),
            }
        };
        if writeln!(w, "{reply}").is_err() {
            return;
        }
    }
}

/// Number a request, send it on the ring, and wait for its reply.
fn forward(req: Req, shared: &Mutex<Shared>, out: &Sender<String>) -> Result<(), String> {
    let (tx, rx) = mpsc::channel();
    let n = {
        let mut s = shared.lock().unwrap_or_else(|e| e.into_inner());
        if s.closed {
            return Err("the host-integration channel has ended".into());
        }
        s.next += 1;
        let n = s.next;
        s.pending.insert(n, tx);
        n
    };
    let line = Msg::Req { n, req }.encode();
    log(&format!("req {n}: {:?}", line.chars().take(120).collect::<String>()));
    if out.send(line).is_err() {
        shared.lock().unwrap_or_else(|e| e.into_inner()).pending.remove(&n);
        return Err("the host-integration channel has ended".into());
    }
    let r = rx.recv_timeout(REPLY_TIMEOUT).unwrap_or_else(|_| {
        Err(format!("no answer from the host in {} s", REPLY_TIMEOUT.as_secs()))
    });
    shared.lock().unwrap_or_else(|e| e.into_inner()).pending.remove(&n);
    log(&format!("rep {n}: {r:?}"));
    r
}

// ---------------------------------------------------------------- audio --

/// A node's stream on the sound ring, Linux to host (an output).
pub trait StreamSink: Send {
    fn play(&mut self, pcm: &[u8]);
}

/// A node's stream on the sound ring, host to Linux (an input). Fills `buf`
/// with the next samples and returns how many bytes it filled.
pub trait StreamSource: Send {
    fn capture(&mut self, buf: &mut [u8]) -> usize;
}

/// The sound ring's per-node streams. See the module documentation: only
/// [`NullRing`] exists until the multi-stream sound ring lands.
pub trait SoundRing: Sync {
    fn sink(&self, node: &Node) -> Box<dyn StreamSink>;
    fn source(&self, node: &Node) -> Box<dyn StreamSource>;
}

/// Drops what an output plays; gives an input silence.
pub struct NullRing;

struct NullSink;
impl StreamSink for NullSink {
    fn play(&mut self, _: &[u8]) {}
}
struct Silence;
impl StreamSource for Silence {
    fn capture(&mut self, buf: &mut [u8]) -> usize {
        buf.fill(0);
        buf.len()
    }
}

impl SoundRing for NullRing {
    fn sink(&self, _: &Node) -> Box<dyn StreamSink> {
        Box::new(NullSink)
    }
    fn source(&self, _: &Node) -> Box<dyn StreamSource> {
        Box::new(Silence)
    }
}

enum AudioCmd {
    Prefix(String),
    State(State),
    Stop,
}

/// A `pw-*` command aimed at exactly `remote`'s daemon: its socket, runtime
/// directory and `XDG_RUNTIME_DIR` are set from `remote`, never inherited, so
/// no command here can reach a PipeWire the session was not given.
pub fn pw_command(bin: Option<&Path>, name: &str, remote: &Path) -> Command {
    let prog = bin.map_or_else(|| PathBuf::from(name), |b| b.join(name));
    let mut c = Command::new(prog);
    let rt = remote.parent().unwrap_or(Path::new("/nonexistent"));
    c.env("PIPEWIRE_REMOTE", remote)
        .env("PIPEWIRE_RUNTIME_DIR", rt)
        .env("XDG_RUNTIME_DIR", rt)
        .env_remove("PULSE_SERVER")
        .env_remove("PIPEWIRE_CORE")
        .stdin(Stdio::null());
    c
}

/// The node names the daemon has, by `pw-dump`.
pub fn pw_nodes(bin: Option<&Path>, remote: &Path) -> std::collections::HashSet<String> {
    pw_command(bin, "pw-dump", remote)
        .stderr(Stdio::null())
        .output()
        .map(|o| audio::pw_dump_node_names(&String::from_utf8_lossy(&o.stdout)))
        .unwrap_or_default()
}

/// One node's FIFO, copied to or from its stream until `stop`.
struct Bridge {
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Drop for Bridge {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

fn open_fifo(path: &Path) -> std::io::Result<OwnedFd> {
    let c = CString::new(path.as_os_str().as_encoded_bytes()).map_err(std::io::Error::other)?;
    // O_RDWR: neither end ever sees EOF or blocks in open while the other end
    // restarts, which is the PipeWire child on every device change.
    // SAFETY: a valid C string; the fd is owned from here on.
    let fd = unsafe { libc::open(c.as_ptr(), libc::O_RDWR | libc::O_NONBLOCK | libc::O_CLOEXEC) };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: just opened, nobody else owns it.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

fn bridge(node: &Node, fifo: &Path, ring: &dyn SoundRing) -> std::io::Result<Bridge> {
    let fd = open_fifo(fifo)?;
    let stop = Arc::new(AtomicBool::new(false));
    let s2 = Arc::clone(&stop);
    let name = format!("hostint-{}", node.stream);
    let thread = match node.dir {
        Dir::Output => {
            let mut sink = ring.sink(node);
            std::thread::Builder::new().name(name).spawn(move || {
                let mut buf = vec![0u8; 16384];
                while !s2.load(Ordering::SeqCst) {
                    let mut p = libc::pollfd { fd: fd.as_raw_fd(), events: libc::POLLIN, revents: 0 };
                    // SAFETY: one live pollfd.
                    if unsafe { libc::poll(&mut p, 1, 100) } <= 0 {
                        continue;
                    }
                    // SAFETY: a live fd and a buffer of the length we pass.
                    let n = unsafe { libc::read(fd.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len()) };
                    if n > 0 {
                        sink.play(&buf[..n as usize]);
                    }
                }
            })?
        }
        Dir::Input => {
            let mut src = ring.source(node);
            // 10 ms of the device's format, s16le.
            let chunk = (node.rate as usize / 100) * node.channels as usize * 2;
            std::thread::Builder::new().name(name).spawn(move || {
                let mut buf = vec![0u8; chunk.max(4)];
                let mut next = Instant::now();
                while !s2.load(Ordering::SeqCst) {
                    let n = src.capture(&mut buf);
                    // A full FIFO is a source nobody reads: drop, as a
                    // microphone does.
                    // SAFETY: a live fd and a buffer of at least n bytes.
                    unsafe { libc::write(fd.as_raw_fd(), buf.as_ptr().cast(), n.min(buf.len())) };
                    next += Duration::from_millis(10);
                    let now = Instant::now();
                    if next > now {
                        std::thread::sleep(next - now);
                    } else {
                        next = now;
                    }
                }
            })?
        }
    };
    Ok(Bridge { stop, thread: Some(thread) })
}

fn mkfifo(path: &Path) -> std::io::Result<()> {
    let _ = std::fs::remove_file(path);
    let c = CString::new(path.as_os_str().as_encoded_bytes()).map_err(std::io::Error::other)?;
    // SAFETY: a valid C string.
    if unsafe { libc::mkfifo(c.as_ptr(), 0o600) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

/// The child PipeWire and the bridges of one device set.
struct Nodes {
    child: Option<Child>,
    _bridges: Vec<Bridge>,
    started: Instant,
}

impl Drop for Nodes {
    fn drop(&mut self) {
        if let Some(mut c) = self.child.take() {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}

struct Supervisor<'a> {
    cfg: &'a Config,
    remote: PathBuf,
    ring: &'a dyn SoundRing,
    recon: Reconciler,
    last: Option<State>,
    running: Option<Nodes>,
    /// The set to bring back if the child dies.
    want: Vec<Node>,
    retry_at: Option<Instant>,
}

impl Supervisor<'_> {
    fn update(&mut self, st: &State) {
        self.last = Some(st.clone());
        let d = self.recon.update(st);
        if let Some(nodes) = d.restart {
            log(&format!("device set: {} node(s); restarting the node host", nodes.len()));
            self.want = nodes;
            self.restart();
        } else if d.apply_defaults {
            self.apply_defaults();
        }
    }

    fn restart(&mut self) {
        self.running = None;
        self.retry_at = None;
        let adir = self.cfg.hdir().join("audio");
        let conf = adir.join("nodes.conf");
        if self.want.is_empty() {
            let _ = std::fs::remove_file(&conf);
            return;
        }
        match self.spawn(&adir, &conf) {
            Ok(n) => {
                self.running = Some(n);
                self.wait_nodes_then_defaults();
            }
            Err(e) => {
                log(&format!("the node host did not start: {e}"));
                self.retry_at = Some(Instant::now() + Duration::from_secs(2));
            }
        }
    }

    fn spawn(&self, adir: &Path, conf: &Path) -> std::io::Result<Nodes> {
        let fdir = adir.to_string_lossy().into_owned();
        let mut bridges = Vec::new();
        for n in &self.want {
            let f = PathBuf::from(audio::fifo_path(&fdir, n));
            mkfifo(&f)?;
            bridges.push(bridge(n, &f, self.ring)?);
        }
        write_atomic(conf, &audio::render_config(&self.want, &fdir))?;
        let logf = std::fs::File::create(adir.join("nodes.log"))?;
        let mut cmd = pw_command(self.cfg.pw_bin.as_deref(), "pipewire", &self.remote);
        cmd.arg("-c").arg(conf).stdout(logf.try_clone()?).stderr(logf);
        // SAFETY: async-signal-safe prctl between fork and exec: the node host
        // dies with this process, never outlives it holding nodes.
        unsafe {
            cmd.pre_exec(|| {
                libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM);
                Ok(())
            });
        }
        let child = cmd.spawn()?;
        log(&format!("node host pid {} ({})", child.id(), conf.display()));
        Ok(Nodes { child: Some(child), _bridges: bridges, started: Instant::now() })
    }

    /// The defaults are set once the nodes exist, at most 5 s later.
    fn wait_nodes_then_defaults(&mut self) {
        let want: Vec<String> = self.want.iter().map(|n| n.node_name.clone()).collect();
        let t0 = Instant::now();
        loop {
            let have = pw_nodes(self.cfg.pw_bin.as_deref(), &self.remote);
            if want.iter().all(|w| have.contains(w)) {
                log(&format!("{} node(s) up in {} ms", want.len(), t0.elapsed().as_millis()));
                break;
            }
            if t0.elapsed() > Duration::from_secs(5) {
                let missing: Vec<&String> = want.iter().filter(|w| !have.contains(*w)).collect();
                log(&format!("nodes missing after 5 s: {missing:?}"));
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        self.apply_defaults();
    }

    fn apply_defaults(&self) {
        let d = self.recon.defaults().clone();
        for (dir, name) in [(Dir::Output, d.sink), (Dir::Input, d.source)] {
            let Some(name) = name else { continue };
            let st = pw_command(self.cfg.pw_bin.as_deref(), "pw-metadata", &self.remote)
                .args(audio::metadata_args(dir, &name))
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
            match st {
                Ok(s) if s.success() => log(&format!("default {} -> {name}", dir.pw())),
                other => log(&format!("pw-metadata for {name}: {other:?}")),
            }
        }
    }

    fn check_child(&mut self) {
        if let Some(n) = &mut self.running
            && let Some(c) = &mut n.child
            && let Ok(Some(st)) = c.try_wait()
        {
            log(&format!(
                "the node host exited ({st}) after {} ms; again in 2 s",
                n.started.elapsed().as_millis()
            ));
            self.running = None;
            self.retry_at = Some(Instant::now() + Duration::from_secs(2));
        }
        if self.retry_at.is_some_and(|t| Instant::now() >= t) {
            self.restart();
        }
    }
}

fn audio_loop(cfg: &Config, rx: &Receiver<AudioCmd>, stop: &AtomicBool, ring: &dyn SoundRing) {
    let Some(remote) = cfg.pipewire.clone() else {
        // Drain, saying once why the devices are not nodes.
        let mut said = false;
        loop {
            match rx.recv_timeout(Duration::from_millis(200)) {
                Ok(AudioCmd::State(s)) if !said => {
                    said = true;
                    log(&format!(
                        "the host has {} audio device(s), and this session has no PipeWire of its own \
                         (WC_LAUNCH_AUDIO=pipewire or user-pipewire): they do not become nodes",
                        s.devices.len()
                    ));
                }
                Ok(AudioCmd::Stop) | Err(RecvTimeoutError::Disconnected) => return,
                _ if stop.load(Ordering::SeqCst) => return,
                _ => {}
            }
        }
    };
    let mut sup = Supervisor {
        cfg,
        remote,
        ring,
        recon: Reconciler::new("host"),
        last: None,
        running: None,
        want: Vec::new(),
        retry_at: None,
    };
    loop {
        match rx.recv_timeout(Duration::from_millis(200)) {
            Ok(AudioCmd::Prefix(p)) => {
                sup.recon = Reconciler::new(&p);
                if let Some(s) = sup.last.clone() {
                    sup.update(&s);
                }
            }
            Ok(AudioCmd::State(s)) => sup.update(&s),
            Ok(AudioCmd::Stop) | Err(RecvTimeoutError::Disconnected) => break,
            Err(RecvTimeoutError::Timeout) => {}
        }
        if stop.load(Ordering::SeqCst) {
            break;
        }
        sup.check_child();
    }
    sup.running = None;
}

// --------------------------------------------------------- wc-host-open --

fn random_name() -> String {
    let mut b = [0u8; 8];
    if std::fs::File::open("/dev/urandom").and_then(|mut f| f.read_exact(&mut b)).is_err() {
        let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
        b = (t.as_nanos() as u64 ^ u64::from(std::process::id())).to_le_bytes();
    }
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// Copy a Linux-only file into a fresh 0700 directory of the drop directory.
/// Returns the copy's Linux path. Done by `wc-host-open`, as the user who
/// asked: the proxy may run as root and must not copy what a user names.
pub fn copy_to_drop(from: &str, linux_dir: &str) -> Result<String, String> {
    let src = Path::new(from);
    if !src.is_file() {
        return Err(format!("{from}: only a regular file can be opened with a host application"));
    }
    let base = src.file_name().ok_or_else(|| format!("{from}: no file name"))?;
    let d = Path::new(linux_dir).join(random_name());
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&d)
        .map_err(|e| format!("{}: {e}", d.display()))?;
    let to = d.join(base);
    std::fs::copy(src, &to).map_err(|e| format!("copying {from} to {}: {e}", to.display()))?;
    Ok(to.to_string_lossy().into_owned())
}

/// A connection to the proxy, as `wc-host-open` holds it.
pub struct Client {
    r: BufReader<UnixStream>,
    w: UnixStream,
    pub platform: Option<String>,
    pub paths: PathMap,
}

impl Client {
    pub fn connect(sock: &Path) -> Result<Client, String> {
        let s = UnixStream::connect(sock).map_err(|e| format!("{}: {e}", sock.display()))?;
        let _ = s.set_read_timeout(Some(REPLY_TIMEOUT + Duration::from_secs(5)));
        let w = s.try_clone().map_err(|e| e.to_string())?;
        let mut c = Client { r: BufReader::new(s), w, platform: None, paths: PathMap::default() };
        writeln!(c.w, "hello").map_err(|e| e.to_string())?;
        let host = c.line()?;
        let p = host.strip_prefix("host ").ok_or("the proxy did not say hello")?;
        c.platform = (p != "-").then(|| p.to_string());
        match Msg::decode(&c.line()?) {
            Ok(Msg::Paths(m)) => c.paths = m,
            _ => return Err("the proxy sent no path map".into()),
        }
        Ok(c)
    }

    fn line(&mut self) -> Result<String, String> {
        let mut l = String::new();
        match self.r.read_line(&mut l) {
            Ok(0) => Err("the proxy closed the connection".into()),
            Ok(_) => Ok(l.trim_end_matches('\n').to_string()),
            Err(e) => Err(format!("waiting for the proxy: {e}")),
        }
    }

    /// Outer `Err`: no answer at all. Inner: the host's.
    pub fn request(&mut self, req: Req) -> Result<Result<(), String>, String> {
        writeln!(self.w, "{}", Msg::Req { n: 1, req }.encode()).map_err(|e| e.to_string())?;
        match Msg::decode(&self.line()?) {
            Ok(Msg::Rep { result, .. }) => Ok(result),
            _ => Err("the proxy's answer was not a reply".into()),
        }
    }
}

const OPEN_USAGE: &str = "usage: wc-host-open [--with TOKEN] TARGET...\n\
  Opens each URL or file on the host desktop: a URL in its browser, a file\n\
  inside a shared folder with its default application, or, with --with, with\n\
  the host application TOKEN (a Linux-only file is then copied to the share).";

/// `wc-host-open [--with TOKEN] TARGET...`. Exit 0 if every target opened, 1
/// if the host refused one, 2 for a target refused here or a usage error, 3
/// if there is no host-integration channel.
pub fn open_cli(args: &[String]) -> std::process::ExitCode {
    use std::process::ExitCode;
    let mut token: Option<String> = None;
    let mut targets: Vec<String> = Vec::new();
    let mut i = 0;
    let mut opts = true;
    while i < args.len() {
        let a = &args[i];
        match a.as_str() {
            "--with" if opts => {
                let Some(t) = args.get(i + 1) else {
                    eprintln!("wc-host-open: --with needs a token\n{OPEN_USAGE}");
                    return ExitCode::from(2);
                };
                if !wire::token_ok(t) {
                    eprintln!("wc-host-open: {t:?} is not a host application token");
                    return ExitCode::from(2);
                }
                token = Some(t.clone());
                i += 2;
                continue;
            }
            "-h" | "--help" if opts => {
                println!("{OPEN_USAGE}");
                return ExitCode::SUCCESS;
            }
            "--" if opts => opts = false,
            _ => targets.push(a.clone()),
        }
        i += 1;
    }
    if targets.is_empty() {
        eprintln!("wc-host-open: nothing to open\n{OPEN_USAGE}");
        return ExitCode::from(2);
    }
    let Some(sock) = std::env::var_os("WC_HOSTINT_SOCK") else {
        eprintln!("wc-host-open: not in a waycompose session with host integration (WC_HOSTINT_SOCK is not set)");
        return ExitCode::from(3);
    };
    let mut c = match Client::connect(Path::new(&sock)) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("wc-host-open: no host-integration channel: {e}");
            return ExitCode::from(3);
        }
    };
    let host = host_names(c.platform.as_deref()).0;
    let mut code = 0u8;
    for t in &targets {
        let given = linux::classify(t);
        let resolved = match &given {
            Given::Path(p) => std::fs::canonicalize(p).ok().map(|p| p.to_string_lossy().into_owned()),
            Given::Url(_) => None,
        };
        let req = match linux::plan(&given, resolved.as_deref(), token.as_deref(), &c.paths, host) {
            Plan::Send(r) => r,
            Plan::CopyThenSend { from, linux_dir, host_dir, token } => {
                let copy = match copy_to_drop(&from, &linux_dir) {
                    Ok(p) => p,
                    Err(e) => {
                        eprintln!("wc-host-open: {e}");
                        code = code.max(2);
                        continue;
                    }
                };
                let hp = linux::linux_to_host(&copy, &c.paths).unwrap_or_else(|| {
                    let rel = copy.strip_prefix(linux_dir.trim_end_matches('/')).unwrap_or(&copy);
                    if host_dir.contains('\\') || host_dir.as_bytes().get(1) == Some(&b':') {
                        format!("{}{}", host_dir.trim_end_matches('\\'), rel.replace('/', "\\"))
                    } else {
                        format!("{}{rel}", host_dir.trim_end_matches('/'))
                    }
                });
                match token {
                    Some(t) => Req::With { token: t, target: Target::HostPath(hp) },
                    None => Req::Path(hp),
                }
            }
            Plan::Refuse(why) => {
                eprintln!("wc-host-open: {why}");
                code = code.max(2);
                continue;
            }
        };
        match c.request(req) {
            Ok(Ok(())) => {}
            Ok(Err(why)) => {
                eprintln!("wc-host-open: the {host} desktop refused to open {t}: {why}");
                code = code.max(1);
            }
            Err(e) => {
                eprintln!("wc-host-open: {t}: {e}");
                code = 3;
            }
        }
    }
    ExitCode::from(code)
}

// ------------------------------------------------------------ pcm-serve --

/// `wc-proxy pcm-serve FIFO SOCKET`: the PCM written into FIFO (by the
/// session PipeWire's monitor tunnel) to whoever is connected to SOCKET, which
/// is what PulseAudio's simple protocol gave §86's `audio.sock`. With nobody
/// connected the PCM is read and dropped, so the tunnel never stalls.
pub fn pcm_serve_cli(args: &[String]) -> std::process::ExitCode {
    let [fifo, sock] = args else {
        eprintln!("usage: wc-proxy pcm-serve FIFO SOCKET");
        return std::process::ExitCode::from(2);
    };
    match pcm_serve(Path::new(fifo), Path::new(sock)) {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("wc-proxy pcm-serve: {e}");
            std::process::ExitCode::FAILURE
        }
    }
}

fn pcm_serve(fifo: &Path, sock: &Path) -> std::io::Result<()> {
    if !fifo.exists() {
        mkfifo(fifo)?;
    }
    let fd = open_fifo(fifo)?;
    let _ = std::fs::remove_file(sock);
    let l = UnixListener::bind(sock)?;
    std::fs::set_permissions(sock, std::fs::Permissions::from_mode(0o600))?;
    l.set_nonblocking(true)?;
    let mut client: Option<UnixStream> = None;
    let mut buf = vec![0u8; 3840];
    loop {
        if let Ok((c, _)) = l.accept() {
            let _ = c.set_nonblocking(false);
            let _ = c.set_write_timeout(Some(Duration::from_millis(500)));
            client = Some(c);
        }
        let mut p = libc::pollfd { fd: fd.as_raw_fd(), events: libc::POLLIN, revents: 0 };
        // SAFETY: one live pollfd.
        if unsafe { libc::poll(&mut p, 1, 50) } <= 0 {
            continue;
        }
        // SAFETY: a live fd and a buffer of the length we pass.
        let n = unsafe { libc::read(fd.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len()) };
        if n <= 0 {
            continue;
        }
        if let Some(c) = &mut client
            && c.write_all(&buf[..n as usize]).is_err()
        {
            client = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;
    use sv_transport::control::{Geometry, ProtocolIds};
    use sv_transport::region::SharedRegion;
    use sv_transport::wlbytes::Endpoint;
    use wc_hostint::audio::Device;

    fn tmpdir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("wc-hostint-{tag}-{}-{}", std::process::id(), random_name()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// The host's half: the control region's owner, opening the slot the
    /// proxy claimed, as `wc-launch`'s `service_channel` does.
    struct FakeHost {
        ch: ByteChannel,
        lines: LineBuf,
    }

    impl FakeHost {
        fn find(owner: &Control) -> FakeHost {
            let t0 = Instant::now();
            loop {
                for s in ByteChannel::live_connections(owner).unwrap() {
                    if owner.ring_owner(s).ok() == Some(OWNER_HOSTINT) {
                        return FakeHost { ch: ByteChannel::open(owner, s, Endpoint::Peer).unwrap(), lines: LineBuf::default() };
                    }
                }
                assert!(t0.elapsed() < Duration::from_secs(5), "no hostint slot");
                std::thread::sleep(Duration::from_millis(5));
            }
        }
        fn send(&mut self, m: &Msg) {
            assert!(write_line(&mut self.ch, &m.encode()));
        }
        fn recv(&mut self) -> String {
            let t0 = Instant::now();
            let mut tmp = [0u8; 4096];
            loop {
                if let Some(l) = self.lines.next_line() {
                    return l;
                }
                if let Ok(ReadOutcome::Bytes(n)) = self.ch.read(&mut tmp) {
                    self.lines.push(&tmp[..n]);
                    continue;
                }
                assert!(t0.elapsed() < Duration::from_secs(5), "nothing from Linux");
                std::thread::sleep(Duration::from_millis(2));
            }
        }
    }

    fn region() -> (Arc<Control>, Arc<Control>) {
        let g = Geometry::small();
        let r = Arc::new(SharedRegion::heap(g.control_size as usize).unwrap());
        let ids = ProtocolIds::new(1, 2);
        let owner = Control::create(Arc::clone(&r), g, ids, 1).unwrap();
        (Arc::new(owner), Arc::new(Control::attach(r, ids).unwrap()))
    }

    fn wait_for(what: &str, mut f: impl FnMut() -> bool) {
        let t0 = Instant::now();
        while !f() {
            assert!(t0.elapsed() < Duration::from_secs(5), "{what}");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn handler() -> Handler {
        Handler {
            token: "n0".into(),
            name: "Notepad".into(),
            mimes: vec!["text/plain".into()],
            default_for: vec!["text/plain".into()],
            schemes: vec![],
        }
    }

    /// The whole Linux side with no PipeWire: hello, paths, handlers become
    /// files, and a local request crosses the ring and its reply comes back.
    #[test]
    fn hello_paths_handlers_and_a_request_round_trip() {
        let (owner, linux) = region();
        let dir = tmpdir("svc");
        let cfg = Config { dir: dir.clone(), opener: Some("/opt/wc/wc-host-open".into()), ..Config::default() };
        let h = start(&linux, cfg.clone()).expect("started");
        let mut host = FakeHost::find(&owner);
        assert_eq!(host.recv(), "hello 1");
        host.send(&Msg::Hello { version: 1, platform: Some("windows".into()) });
        let share = dir.join("share");
        std::fs::create_dir_all(share.join("waycompose-open")).unwrap();
        let sl = share.to_string_lossy().into_owned();
        let map = PathMap {
            shares: vec![("Z:\\".into(), sl.clone())],
            drop: Some(("Z:\\waycompose-open".into(), format!("{sl}/waycompose-open"))),
        };
        host.send(&Msg::Paths(map.clone()));
        host.send(&Msg::Handlers(vec![handler()]));

        let apps = dir.join("hostint/xdg-data/applications");
        wait_for("handler entry", || apps.join("wc-host-n0.desktop").exists());
        let e = std::fs::read_to_string(apps.join("wc-host-n0.desktop")).unwrap();
        assert!(e.contains("Name=Notepad (Windows)") && e.contains("Exec=\"/opt/wc/wc-host-open\" --with n0 %U"), "{e}");
        assert!(apps.join("wc-host-browser.desktop").exists());
        let m = std::fs::read_to_string(dir.join("hostint/xdg-config/mimeapps.list")).unwrap();
        assert!(m.contains("text/plain=wc-host-n0.desktop"));
        wait_for("roots", || std::fs::read_to_string(dir.join("hostint/roots")).unwrap_or_default().trim() == sl);

        // A handler the host no longer offers is removed.
        host.send(&Msg::Handlers(vec![]));
        wait_for("stale entry removed", || !apps.join("wc-host-n0.desktop").exists());

        let sock = cfg.socket();
        assert_eq!(std::fs::metadata(&sock).unwrap().permissions().mode() & 0o777, 0o600);
        let t = std::thread::spawn(move || {
            let mut c = Client::connect(&sock).unwrap();
            assert_eq!(c.platform.as_deref(), Some("windows"));
            assert_eq!(c.paths, map);
            let a = c.request(Req::Url("https://example.org".into())).unwrap();
            let b = c.request(Req::Path("Z:\\a.txt".into())).unwrap();
            (a, b)
        });
        let l = host.recv();
        let Ok(Msg::Req { n, req }) = Msg::decode(&l) else { panic!("{l}") };
        assert_eq!(req, Req::Url("https://example.org".into()));
        host.send(&Msg::Rep { n, result: Ok(()) });
        let l = host.recv();
        let Ok(Msg::Req { n: n2, req }) = Msg::decode(&l) else { panic!("{l}") };
        assert!(n2 > n, "the proxy numbers requests");
        assert_eq!(req, Req::Path("Z:\\a.txt".into()));
        host.send(&Msg::Rep { n: n2, result: Err("not today".into()) });
        let (a, b) = t.join().unwrap();
        assert_eq!(a, Ok(()));
        assert_eq!(b, Err("not today".into()));

        // The ring ends: a request is answered at once, not after 5 s.
        drop(host);
        h.stop();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_request_nobody_answers_times_out() {
        let shared = Mutex::new(Shared::default());
        let (tx, _rx) = mpsc::channel();
        let t0 = Instant::now();
        let r = forward(Req::Url("https://a/".into()), &shared, &tx);
        assert!(r.unwrap_err().contains("no answer"));
        assert!(t0.elapsed() >= REPLY_TIMEOUT);
        assert!(shared.lock().unwrap().pending.is_empty());
        shared.lock().unwrap().closed = true;
        assert!(forward(Req::Url("https://a/".into()), &shared, &tx).unwrap_err().contains("ended"));
    }

    #[test]
    fn copies_go_to_a_private_directory_of_the_drop() {
        let dir = tmpdir("copy");
        let f = dir.join("notes 1.txt");
        std::fs::write(&f, b"hi").unwrap();
        let drop = dir.join("drop");
        let c = copy_to_drop(f.to_str().unwrap(), drop.to_str().unwrap()).unwrap();
        assert!(c.ends_with("/notes 1.txt"));
        assert_eq!(std::fs::read(&c).unwrap(), b"hi");
        let parent = Path::new(&c).parent().unwrap();
        assert_eq!(std::fs::metadata(parent).unwrap().permissions().mode() & 0o777, 0o700);
        assert!(copy_to_drop(dir.to_str().unwrap(), drop.to_str().unwrap()).is_err(), "a directory is refused");
        let _ = std::fs::remove_dir_all(&dir);
    }

    struct Counting(Arc<AtomicUsize>);
    impl StreamSink for Counting {
        fn play(&mut self, pcm: &[u8]) {
            self.0.fetch_add(pcm.len(), Ordering::SeqCst);
        }
    }
    struct CountRing(Arc<AtomicUsize>);
    impl SoundRing for CountRing {
        fn sink(&self, _: &Node) -> Box<dyn StreamSink> {
            Box::new(Counting(Arc::clone(&self.0)))
        }
        fn source(&self, _: &Node) -> Box<dyn StreamSource> {
            Box::new(Silence)
        }
    }

    /// An output's FIFO is drained into its stream; an input's FIFO gets the
    /// stream's samples at the device's rate.
    #[test]
    fn bridges_move_samples_between_fifos_and_streams() {
        let dir = tmpdir("bridge");
        let st = State {
            devices: vec![
                Device { dir: Dir::Output, id: "o".into(), description: "Speakers".into(), channels: 2, rate: 48_000 },
                Device { dir: Dir::Input, id: "i".into(), description: "Mic".into(), channels: 1, rate: 16_000 },
            ],
            ..State::default()
        };
        let nodes = audio::plan(&st, "win");
        let d = dir.to_string_lossy().into_owned();
        let count = Arc::new(AtomicUsize::new(0));
        let ring = CountRing(Arc::clone(&count));
        let fo = PathBuf::from(audio::fifo_path(&d, &nodes[0]));
        let fi = PathBuf::from(audio::fifo_path(&d, &nodes[1]));
        mkfifo(&fo).unwrap();
        mkfifo(&fi).unwrap();
        let b1 = bridge(&nodes[0], &fo, &ring).unwrap();
        let b2 = bridge(&nodes[1], &fi, &ring).unwrap();
        // PipeWire's end of the output FIFO.
        let w = open_fifo(&fo).unwrap();
        // SAFETY: a live fd and a 1000-byte buffer.
        unsafe { libc::write(w.as_raw_fd(), [7u8; 1000].as_ptr().cast(), 1000) };
        wait_for("output drained", || count.load(Ordering::SeqCst) == 1000);
        // PipeWire's end of the input FIFO: 100 ms of 16 kHz mono is 3200 bytes.
        std::thread::sleep(Duration::from_millis(100));
        let r = open_fifo(&fi).unwrap();
        let mut buf = vec![1u8; 65536];
        // SAFETY: as above.
        let n = unsafe { libc::read(r.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len()) };
        assert!(n >= 2 * 320, "silence arrives in 10 ms chunks ({n})");
        assert!(buf[..n as usize].iter().all(|b| *b == 0));
        drop((b1, b2));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn pw_commands_never_inherit_the_session_s_pipewire() {
        let c = pw_command(Some(Path::new("/pw/bin")), "pw-dump", Path::new("/run/wc/s1/pw/pipewire-0"));
        assert_eq!(c.get_program(), "/pw/bin/pw-dump");
        let envs: HashMap<_, _> = c.get_envs().map(|(k, v)| (k.to_string_lossy().into_owned(), v.map(|v| v.to_string_lossy().into_owned()))).collect();
        assert_eq!(envs["PIPEWIRE_REMOTE"].as_deref(), Some("/run/wc/s1/pw/pipewire-0"));
        assert_eq!(envs["PIPEWIRE_RUNTIME_DIR"].as_deref(), Some("/run/wc/s1/pw"));
        assert_eq!(envs["XDG_RUNTIME_DIR"].as_deref(), Some("/run/wc/s1/pw"));
        assert_eq!(envs["PULSE_SERVER"], None, "removed, not inherited");
    }

    #[test]
    fn host_names_follow_hello() {
        assert_eq!(host_names(Some("windows")), ("Windows", "win"));
        assert_eq!(host_names(Some("macos")), ("macOS", "mac"));
        assert_eq!(host_names(None), ("host", "host"));
    }
}
