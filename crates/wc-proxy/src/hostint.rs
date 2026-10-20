//! §106: the Linux end of host integration. The proxy claims the
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
//! * **requests**: `wc-host-open` connects to `DIR/hostint.sock` (0600, the
//!   peer's uid checked), sends one `req`, and gets the host's answer. The
//!   proxy numbers the request, forwards it on the ring and waits up to 5 s
//!   for `rep N`.
//!
//! Nothing here is a socket to the host. `hostint.sock` is local to Linux;
//! everything that reaches the host crosses on the ring slot. The host's audio
//! devices are not this channel's: they are streams on the sound ring
//! (§103.16, `sound.rs`).

use std::collections::HashMap;
use std::ffi::CString;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use sv_transport::control::Control;
use sv_transport::wlbytes::{ByteChannel, ReadOutcome, OWNER_HOSTINT};
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
    /// End every thread and remove the socket.
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
    start_indexed(control, cfg, 0)
}

/// §107: [`start`] for session `index` of a shared compositor, whose slot
/// carries the index in its owner id.
pub fn start_indexed(control: &Arc<Control>, cfg: Config, index: u8) -> Option<Handle> {
    match ByteChannel::claim(control, sv_transport::wlbytes::with_session(OWNER_HOSTINT, index)) {
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
    for d in [h.clone(), h.join("xdg-data/applications"), h.join("xdg-config")] {
        std::fs::DirBuilder::new().recursive(true).mode(0o700).create(&d)?;
    }
    write_atomic(&h.join("roots"), "")?;
    let shared = Arc::new(Mutex::new(Shared::default()));
    let stop = Arc::new(AtomicBool::new(false));
    let (out_tx, out_rx) = mpsc::channel::<String>();

    let listener = bind_socket(&cfg)?;
    let mut threads = Vec::new();
    {
        let (cfg, shared, stop) = (cfg.clone(), Arc::clone(&shared), Arc::clone(&stop));
        threads.push(
            std::thread::Builder::new()
                .name("hostint-ring".into())
                .spawn(move || ring_loop(ch, &cfg, &shared, &out_rx, &stop))?,
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
                    on_host_line(&l, cfg, shared);
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
}

fn on_host_line(line: &str, cfg: &Config, shared: &Mutex<Shared>) {
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

#[cfg(test)]
mod tests {
    use super::*;
    use sv_transport::control::{Geometry, ProtocolIds};
    use sv_transport::region::SharedRegion;
    use sv_transport::wlbytes::Endpoint;

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
        let cfg = Config { dir: dir.clone(), opener: Some("/opt/wc/wc-host-open".into()) };
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
        wait_for("default handler", || {
            std::fs::read_to_string(dir.join("hostint/xdg-config/mimeapps.list"))
                .unwrap_or_default().contains("text/plain=wc-host-n0.desktop")
        });
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
    #[test]
    fn host_names_follow_hello() {
        assert_eq!(host_names(Some("windows")), ("Windows", "win"));
        assert_eq!(host_names(Some("macos")), ("macOS", "mac"));
        assert_eq!(host_names(None), ("host", "host"));
    }
}
