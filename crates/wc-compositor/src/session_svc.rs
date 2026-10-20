//! §105: the Windows session's boot, control, lease and sound, held by the
//! process that maps the shared-memory device; §107: for every installation
//! of the user at once.
//!
//! On Windows the ivshmem driver gives one mapping per device, and this
//! process holds it. So everything the launcher used ssh for until §105 goes
//! through here:
//!
//! * **The boot.** After the arena header is published and the Wayland rings
//!   are made in the region's tail (§83, §89), a boot writes a record into the
//!   header page (`wc_handle::macboot`, the record macOS writes into its ring
//!   window, §103.15) with an authenticator under the installation's key, and
//!   rings `HELLO_TOKEN` on the rings' doorbell once a second until the host's
//!   session service (`wc-arena-host sessiond --ivshmem`) answers in the same
//!   page. The host then has the proxy up and has claimed the control and
//!   sound slots.
//! * **Control.** The control slot (`OWNER_CONTROL`) carries the launcher's
//!   requests, the words `wc-session` takes. Every `wc-launch.exe` of the
//!   installation sends them to a local AF_UNIX socket in its state directory;
//!   this module passes each through, one at a time, and returns the answer.
//!   The socket is in the user's profile, so only that user (and
//!   administrators) can reach it. Nothing in that hop crosses the VM boundary.
//! * **The lease.** `ping` on the control slot every half second. The host's
//!   proxy ends five seconds after the last one (`wc_proxy::macsvc`), and the
//!   session service then takes the session down.
//! * **Sound.** The sound slot (`OWNER_AUDIO`) carries the session's
//!   PulseAudio sink, played through waveOut here.
//!
//! # §107: one compositor, many installations
//!
//! With `--user-control PATH` this process is the user's one compositor and
//! serves **every** installation: a launcher asks for its session with a
//! `boot` line on that socket, and each session gets an index (1..=255, never
//! reused within this process's run) that its proxy puts into every ring claim
//! (`sv_transport::wlbytes::with_session`). With the index the module routes
//! each slot to its session, and:
//!
//! * boots are **serialised through the one record** of the header page; each
//!   record is signed with *its* installation's key and covers its index;
//! * the arena is the compositor's to cut: each session's proxy asks for
//!   regions on its own arena slot (`OWNER_ARENA`, `grow N` / `release ID`),
//!   granted out of [`wc_handle::barpool::BarPool`] and registered as arena
//!   ids of their own; a client may only name its own session's regions
//!   ([`pool_check`]);
//! * a session ends when its proxy closes its control slot, goes silent, or
//!   its last client has been gone for the linger; its clients are
//!   disconnected, its slots closed and quarantined, its regions returned, and
//!   no other installation's windows notice.
//!
//! Without `--user-control` nothing of that runs: one session, index 0, the
//! proxy's allocator owns the arena, exactly as before §107.

use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};

use sv_transport::wlbytes::{self, ByteChannel, Endpoint, ReadOutcome, OWNER_ARENA, OWNER_AUDIO, OWNER_CONTROL};
use wc_handle::barpool::BarPool;

use crate::ringlink::SlotReader;

/// What a launcher asked for: on the command line (`--boot-*`), or in a
/// `boot` request on the user's control socket (§107).
#[derive(Debug, Clone, Default)]
pub struct Config {
    pub session: String,
    pub install: String,
    pub key_file: PathBuf,
    pub gpu: bool,
    pub control_socket: Option<PathBuf>,
    pub audio_volume: Option<u8>,
    /// §107: this session's clients see the nested-compositor globals.
    pub nested: bool,
    /// §107: where this installation keeps its relaunch files (§92).
    pub relaunch_dir: Option<PathBuf>,
    /// §92.12: how this installation's windows are named on the taskbar
    /// (`taskbar=INSTALL[:APP,…]`). `None` from a launcher before §92.12,
    /// whose windows keep the application-wide identity.
    pub taskbar: Option<crate::taskbar::Scope>,
}

impl Config {
    fn check(&self) -> Result<(), String> {
        if self.session.is_empty() || self.install.is_empty() || self.key_file.as_os_str().is_empty() {
            return Err("a boot needs a session id, an installation and its key file".into());
        }
        if !wc_handle::macboot::valid_id(&self.session) {
            return Err(format!("{:?} is not a session id", self.session));
        }
        if !wc_handle::macboot::valid_installation(&self.install) {
            return Err(format!("{:?} is not an installation name", self.install));
        }
        // §92.12: a session names its own installation's windows, never
        // another's.
        if let Some(t) = &self.taskbar
            && t.install != self.install
        {
            return Err(format!("taskbar={} is not installation {}'s scope", t.render(), self.install));
        }
        Ok(())
    }

    /// §107: the `boot` line a launcher sends on the user's control socket.
    /// Fields are tab-separated `key=value`, because a Windows path may hold
    /// spaces and never holds a tab.
    /// `wc-launch`'s `plan::boot_request` writes the same line; this one is
    /// the tests' half.
    #[cfg(test)]
    pub fn request_line(&self) -> String {
        let mut f = vec![
            "boot".to_string(),
            format!("session={}", self.session),
            format!("install={}", self.install),
            format!("key={}", self.key_file.display()),
        ];
        if let Some(c) = &self.control_socket {
            f.push(format!("control={}", c.display()));
        }
        if self.gpu {
            f.push("gpu=1".into());
        }
        if let Some(v) = self.audio_volume {
            f.push(format!("audio={v}"));
        }
        if self.nested {
            f.push("nested=1".into());
        }
        if let Some(d) = &self.relaunch_dir {
            f.push(format!("relaunch={}", d.display()));
        }
        if let Some(t) = &self.taskbar {
            f.push(format!("taskbar={}", t.render()));
        }
        f.join("\t")
    }

    /// The other half of [`Config::request_line`]. A key this build does not
    /// know is refused rather than ignored: a launcher newer than this
    /// compositor asked for something it would silently not get.
    pub fn parse_request(line: &str) -> Result<Config, String> {
        let mut it = line.split('\t');
        if it.next() != Some("boot") {
            return Err("not a boot request".into());
        }
        let mut c = Config::default();
        for f in it.filter(|f| !f.is_empty()) {
            let (k, v) = f.split_once('=').ok_or_else(|| format!("{f:?} is not key=value"))?;
            if v.contains(['\n', '\r', '\0']) {
                return Err(format!("{k}: a value cannot hold a line break"));
            }
            match k {
                "session" => c.session = v.to_string(),
                "install" => c.install = v.to_string(),
                "key" => c.key_file = v.into(),
                "control" => c.control_socket = Some(v.into()),
                "gpu" => c.gpu = v == "1",
                "audio" => c.audio_volume = Some(v.parse::<u8>().map_err(|_| format!("audio={v}: a volume, 0..100"))?.min(100)),
                "nested" => c.nested = v == "1",
                "relaunch" => c.relaunch_dir = Some(v.into()),
                "taskbar" => c.taskbar = Some(crate::taskbar::Scope::parse(v)?),
                _ => return Err(format!("unknown boot field {k:?}")),
            }
        }
        c.check()?;
        Ok(c)
    }
}

static CONFIG: Mutex<Option<Config>> = Mutex::new(None);
static USER_CONTROL: Mutex<Option<PathBuf>> = Mutex::new(None);
static SINGLETON: Mutex<Option<String>> = Mutex::new(None);
static REGION_FILE: Mutex<Option<PathBuf>> = Mutex::new(None);
/// §107: the process-wide `--nested-compositor-globals`, for clients that are
/// no session's (a socket client).
static LEGACY_NESTED: AtomicBool = AtomicBool::new(false);

fn with_config(f: impl FnOnce(&mut Config)) {
    let mut g = CONFIG.lock().unwrap_or_else(|e| e.into_inner());
    f(g.get_or_insert_with(Config::default));
}

/// One of the `--boot-*`, `--control-socket`, `--session-audio`,
/// `--user-control` or `--singleton` flags. Returns whether `flag` was one.
pub fn parse_flag(flag: &str, value: &mut dyn FnMut() -> Option<String>) -> Result<bool, String> {
    let mut need = |f: &str| value().ok_or(format!("{f} needs a value"));
    match flag {
        "--boot-session" => {
            let v = need(flag)?;
            if !wc_handle::macboot::valid_id(&v) {
                return Err(format!("--boot-session {v:?} is not a session id"));
            }
            with_config(|c| c.session = v);
        }
        "--boot-install" => {
            let v = need(flag)?;
            if !wc_handle::macboot::valid_installation(&v) {
                return Err(format!("--boot-install {v:?} is not an installation name"));
            }
            with_config(|c| c.install = v);
        }
        "--boot-key" => {
            let v = need(flag)?;
            with_config(|c| c.key_file = v.into());
        }
        "--boot-gpu" => with_config(|c| c.gpu = true),
        "--control-socket" => {
            let v = need(flag)?;
            with_config(|c| c.control_socket = Some(v.into()));
        }
        "--session-audio" => {
            let v = need(flag)?;
            let pct: u8 = v.parse().map_err(|_| format!("--session-audio {v:?}: a volume, 0..100"))?;
            with_config(|c| c.audio_volume = Some(pct.min(100)));
        }
        // §107: the user's one compositor; launchers boot their sessions here.
        "--user-control" => {
            let v = need(flag)?;
            *USER_CONTROL.lock().unwrap_or_else(|e| e.into_inner()) = Some(v.into());
        }
        // §110: a file standing in for the GPU device, as pool region 1 (the
        // harness; on the guest `sv-guest` adds the GPU pool itself).
        "--pool-region-file" => {
            let v = need(flag)?;
            *REGION_FILE.lock().unwrap_or_else(|e| e.into_inner()) = Some(v.into());
        }
        // §107: a name only one compositor of the user may hold.
        "--singleton" => {
            let v = need(flag)?;
            if v.is_empty() || !v.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_') {
                return Err(format!("--singleton {v:?}: [A-Za-z0-9_-]"));
            }
            *SINGLETON.lock().unwrap_or_else(|e| e.into_inner()) = Some(v);
        }
        _ => return Ok(false),
    }
    Ok(true)
}

/// The configuration, if a boot was asked for; an error for half of one.
pub fn configured() -> Result<Option<Config>, String> {
    let g = CONFIG.lock().unwrap_or_else(|e| e.into_inner()).clone();
    match g {
        None => Ok(None),
        Some(c) if c.session.is_empty() || c.install.is_empty() || c.key_file.as_os_str().is_empty() => {
            Err("--boot-session, --boot-install and --boot-key go together".into())
        }
        Some(c) => Ok(Some(c)),
    }
}

/// §107: whether this process serves several installations.
pub fn multi() -> bool {
    USER_CONTROL.lock().unwrap_or_else(|e| e.into_inner()).is_some()
}

/// §107: the user's control socket, if this process serves one.
pub fn user_control() -> Option<PathBuf> {
    USER_CONTROL.lock().unwrap_or_else(|e| e.into_inner()).clone()
}

/// §107: `--nested-compositor-globals` given on the command line.
pub fn set_legacy_nested(on: bool) {
    LEGACY_NESTED.store(on, Ordering::Relaxed);
}

fn say(msg: &str) {
    println!("wc-compositor: {msg}");
    let _ = std::io::stdout().flush();
}

fn now_micros() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_micros() as u64)
}

// ------------------------------------------------------------ the singleton ---

/// §107: what keeps a second compositor of the user from starting. Held for
/// the life of the process; the kernel lets go when it ends, crash included.
pub struct Singleton {
    #[cfg(windows)]
    _handle: isize,
    #[cfg(unix)]
    _file: std::fs::File,
}

/// Claim `--singleton NAME`, if given, **before the device is mapped**: a
/// second compositor must not be the one that clears the header of the first.
/// Windows: the named mutex `Local\NAME` (one per logon session, released by
/// the kernel when this process ends). Unix, for the harness: an exclusive
/// `flock` on `$XDG_RUNTIME_DIR/NAME.lock`.
pub fn claim_singleton() -> Result<Option<Singleton>, String> {
    let Some(name) = SINGLETON.lock().unwrap_or_else(|e| e.into_inner()).clone() else {
        return Ok(None);
    };
    #[cfg(windows)]
    {
        use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, ERROR_ALREADY_EXISTS};
        use windows_sys::Win32::System::Threading::CreateMutexW;
        let wide: Vec<u16> = format!("Local\\{name}").encode_utf16().chain(Some(0)).collect();
        // SAFETY: a null security descriptor and a NUL-terminated name.
        let h = unsafe { CreateMutexW(std::ptr::null(), 0, wide.as_ptr()) };
        if h.is_null() {
            return Err(format!("CreateMutexW(Local\\{name}): {}", std::io::Error::last_os_error()));
        }
        // SAFETY: straight after the call above.
        if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
            // SAFETY: our own handle to somebody else's mutex.
            unsafe { CloseHandle(h) };
            return Err(format!("another compositor of this user holds Local\\{name}"));
        }
        Ok(Some(Singleton { _handle: h as isize }))
    }
    #[cfg(unix)]
    {
        use std::os::fd::AsRawFd;
        let dir = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from).unwrap_or_else(std::env::temp_dir);
        let path = dir.join(format!("{name}.lock"));
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&path)
            .map_err(|e| format!("{}: {e}", path.display()))?;
        // SAFETY: a descriptor this function owns.
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            return Err(format!("another compositor of this user holds {}", path.display()));
        }
        Ok(Some(Singleton { _file: file }))
    }
}

// ------------------------------------------------------------------ the hub ---

/// One session of this compositor.
pub struct Sess {
    index: u8,
    cfg: Config,
    /// Booted, and its control slot found.
    up: AtomicBool,
    ended: AtomicBool,
    /// The Wayland thread has disconnected its clients.
    dropped: AtomicBool,
    why: Mutex<String>,
    ctl: Mutex<Option<Arc<Ctl>>>,
    /// Its service slots (control, sound, arena), closed when it ends.
    service_slots: Mutex<Vec<u32>>,
    /// Connections now, whether it ever had one, and since when it has none.
    clients: AtomicUsize,
    had_clients: AtomicBool,
    empty_since: Mutex<Option<Instant>>,
    booted_at: Instant,
    /// Arena requests refused, so the log says the first and then every
    /// power of two rather than every one.
    refused: AtomicUsize,
}

impl Sess {
    fn new(index: u8, cfg: Config) -> Sess {
        Sess {
            index,
            cfg,
            up: AtomicBool::new(false),
            ended: AtomicBool::new(false),
            dropped: AtomicBool::new(false),
            why: Mutex::new(String::new()),
            ctl: Mutex::new(None),
            service_slots: Mutex::new(Vec::new()),
            clients: AtomicUsize::new(0),
            had_clients: AtomicBool::new(false),
            empty_since: Mutex::new(None),
            booted_at: Instant::now(),
            refused: AtomicUsize::new(0),
        }
    }

    fn ended(&self) -> bool {
        self.ended.load(Ordering::SeqCst)
    }
}

struct Hub {
    /// The ivshmem arena's id (the whole BAR, with the outbound window).
    arena: u32,
    pool: Mutex<BarPool>,
    sessions: Mutex<Vec<Arc<Sess>>>,
    /// Signalled whenever a session ends or comes up.
    changed: Condvar,
    /// One record in the header page at a time.
    boot_turn: Mutex<()>,
    /// The next index to try; indices are not reused within this run.
    next_index: Mutex<u16>,
    /// `--exit-when-empty`: a session ends this long after its last client
    /// left, and a shared compositor this long after its last session.
    linger: Option<Duration>,
    /// Since when no session has been live (shared compositor only).
    none_since: Mutex<Option<Instant>>,
    ever: AtomicBool,
}

static HUB: OnceLock<Hub> = OnceLock::new();

fn hub() -> Option<&'static Hub> {
    HUB.get()
}

/// After ivshmem arena `arena`'s header is published: the pool of its BAR
/// (everything but the outbound window, which stays the compositor's), and,
/// for a shared compositor, the checks that keep each session to its own
/// memory and globals.
pub fn init(arena: u32, linger: Option<Duration>) -> Result<(), String> {
    let (_, region_len, rings_len) = wc_handle::arena::ivshmem_identity(arena).map_err(|e| e.to_string())?;
    let arena_len = region_len - wc_handle::ivshmem::HEADER_BYTES - rings_len;
    let limit = wc_handle::arena::outbound().filter(|w| w.arena == arena).map_or(arena_len, |w| w.offset);
    // §110: the first 64 KiB of the arena are kept for the region table of
    // §108.6 (a whole granule, so the first MiB), on a shared compositor.
    let pool = if multi() { BarPool::with_front(limit, REGION_TABLE_BYTES) } else { BarPool::new(limit) };
    if multi() {
        say(&format!(
            "sessions: one compositor for every installation (§107): {} MiB of the BAR in {} KiB granules for the \
             sessions' regions, at most {} MiB per session; the outbound window and the rings are outside them",
            pool.limit() >> 20,
            wc_handle::barpool::GRANULE >> 10,
            pool.cap() >> 20
        ));
        smithay::wayland::shm::set_pool_check(pool_check);
        smithay::wayland::set_nested_filter(nested_visible);
        if let Some(p) = REGION_FILE.lock().unwrap_or_else(|e| e.into_inner()).clone()
            && let Err(e) = FileRegion::add(1, &p)
        {
            say(&format!("sessions: NO POOL REGION 1 from {}: {e}", p.display()));
        }
    }
    HUB.set(Hub {
        arena,
        pool: Mutex::new(pool),
        sessions: Mutex::new(Vec::new()),
        changed: Condvar::new(),
        boot_turn: Mutex::new(()),
        next_index: Mutex::new(1),
        linger,
        none_since: Mutex::new(None),
        ever: AtomicBool::new(false),
    })
    .map_err(|_| "the session hub is already made".to_string())
}

fn live_sessions(h: &Hub) -> Vec<Arc<Sess>> {
    h.sessions.lock().unwrap_or_else(|e| e.into_inner()).clone()
}

fn session(index: u8) -> Option<Arc<Sess>> {
    live_sessions(hub()?).into_iter().find(|s| s.index == index && !s.ended())
}

/// Whether a ring claim of session `index` is to be accepted as a client.
/// A shared compositor accepts its live sessions only; a single-session one
/// accepts what it always did.
pub fn accepts(index: u8) -> bool {
    if !multi() {
        return true;
    }
    index != 0 && session(index).is_some()
}

/// Whether a closed slot of `owner` may be given out again: a client's may
/// (after its quarantine); a session service's only once its session is no
/// longer live, because this process's own threads write into those.
pub fn may_recycle(owner: u64) -> bool {
    !wlbytes::is_service(owner) || session(wlbytes::session_of(owner)).is_none()
}

/// Sessions that have ended and whose clients the Wayland thread has yet to
/// disconnect ([`RingLink::drop_session`](crate::ringlink::RingLink::drop_session)).
pub fn doomed() -> Vec<u8> {
    if !multi() {
        return Vec::new();
    }
    hub().map_or_else(Vec::new, |h| {
        live_sessions(h).iter().filter(|s| s.ended() && !s.dropped.load(Ordering::SeqCst)).map(|s| s.index).collect()
    })
}

/// The Wayland thread has disconnected session `index`'s clients.
pub fn dropped(index: u8) {
    if let Some(h) = hub() {
        for s in live_sessions(h).iter().filter(|s| s.index == index) {
            s.dropped.store(true, Ordering::SeqCst);
        }
        h.changed.notify_all();
    }
}

/// Once a turn, from the Wayland thread: how many connections each session
/// has, for each session's linger.
pub fn note_connections(by_session: &std::collections::HashMap<u8, usize>) {
    let Some(h) = hub() else { return };
    let now = Instant::now();
    for s in live_sessions(h) {
        let n = by_session.get(&s.index).copied().unwrap_or(0);
        s.clients.store(n, Ordering::Relaxed);
        let mut since = s.empty_since.lock().unwrap_or_else(|e| e.into_inner());
        if n > 0 {
            s.had_clients.store(true, Ordering::Relaxed);
            *since = None;
        } else if since.is_none() {
            *since = Some(now);
        }
    }
}

/// A shared compositor whose last session ended `linger` ago, and that has
/// served at least one, has nothing left to do.
pub fn shared_compositor_idle() -> bool {
    let Some(h) = hub() else { return false };
    let Some(linger) = h.linger else { return false };
    if !multi() || !h.ever.load(Ordering::Relaxed) {
        return false;
    }
    let live = live_sessions(h).iter().any(|s| !s.ended());
    let mut since = h.none_since.lock().unwrap_or_else(|e| e.into_inner());
    if live {
        *since = None;
        return false;
    }
    since.get_or_insert_with(Instant::now).elapsed() >= linger
}

/// The sessions, for the user socket's `sessions` and for a log.
fn describe(h: &Hub) -> String {
    live_sessions(h)
        .iter()
        .filter(|s| !s.ended())
        .map(|s| {
            let pool = h.pool.lock().unwrap_or_else(|e| e.into_inner());
            format!(
                "{}:{}:{}:{}:{}MiB/{}",
                s.index,
                s.cfg.install,
                s.cfg.session,
                if s.up.load(Ordering::Relaxed) { "up" } else { "booting" },
                pool.held(s.index) >> 20,
                pool.regions_of(s.index).len()
            )
        })
        .collect::<Vec<_>>()
        .join(" ")
}

// ------------------------------------------------------------- the checks ---

/// §107, smithay's `wl_shm.create_pool` check: a session's client may only
/// name its own session's arena regions. A client that is no session's (a
/// socket client of a developer's run) is not judged here.
pub fn pool_check(client: &smithay::reexports::wayland_server::Client, fd: &smithay::utils::os::ProtoFd) -> Result<(), String> {
    let s = crate::state::client_session(client);
    let Some(wc_handle::Descriptor::ArenaSlice { arena, .. }) = fd.descriptor() else {
        return Ok(());
    };
    if s == 0 {
        return Ok(());
    }
    let Some(h) = hub() else { return Ok(()) };
    let r = slice_allowed(&h.pool.lock().unwrap_or_else(|e| e.into_inner()), s, *arena);
    if let Err(e) = &r {
        say(&format!("session {s}: a pool REFUSED: {e}"));
    }
    r
}

/// The rule [`pool_check`] applies: a client of session `s` may name an arena
/// region only if the pool granted it to `s`. Arena 0, the whole BAR, is no
/// session's, and neither is a region given back or never granted.
pub fn slice_allowed(pool: &BarPool, s: u8, arena: u32) -> Result<(), String> {
    match pool.get(arena).map(|g| g.session) {
        Some(o) if o == s => Ok(()),
        Some(o) => Err(format!("arena {arena:#x} is session {o}'s memory, not session {s}'s (§107)")),
        None => Err(format!("arena {arena:#x} is no session's region; session {s} may only name its own (§107)")),
    }
}

/// §107, smithay's filter for the nested-compositor globals.
pub fn nested_visible(client: &smithay::reexports::wayland_server::Client) -> bool {
    match crate::state::client_session(client) {
        0 => LEGACY_NESTED.load(Ordering::Relaxed),
        s => session(s).is_some_and(|x| x.cfg.nested),
    }
}

// ------------------------------------------------------------------ booting ---

/// Start the boot `--boot-*` asked for, for ivshmem arena `id`, once its
/// header is published, its rings exist and [`init`] has run. Returns at
/// once; the work is on threads of its own and never on the Wayland thread.
pub fn start(cfg: Config, id: u32) {
    let _ = id;
    let _ = std::thread::Builder::new().name("session-boot".into()).spawn(move || {
        if let Err(e) = boot_session(cfg) {
            say(&format!("boot: FAILED: {e}"));
        }
    });
}

/// Boot one session and start its services. The record is written under the
/// boot turn, so two installations' clicks are served one after the other.
fn boot_session(cfg: Config) -> Result<(Arc<Sess>, String), String> {
    let h = hub().ok_or("the arena is not published yet")?;
    cfg.check()?;
    let index = {
        let sessions = h.sessions.lock().unwrap_or_else(|e| e.into_inner());
        let live: Vec<&Arc<Sess>> = sessions.iter().filter(|s| !s.ended()).collect();
        if live.iter().any(|s| s.cfg.session == cfg.session) {
            return Err(format!("session {} is already up", cfg.session));
        }
        if !multi() {
            if !live.is_empty() {
                return Err("this compositor serves one session (it was started without --user-control)".into());
            }
            0
        } else {
            let mut next = h.next_index.lock().unwrap_or_else(|e| e.into_inner());
            if *next > 255 {
                return Err("this compositor has given out every session index; it must be restarted".into());
            }
            let i = *next as u8;
            *next += 1;
            i
        }
    };
    let sess = Arc::new(Sess::new(index, cfg));
    h.sessions.lock().unwrap_or_else(|e| e.into_inner()).push(Arc::clone(&sess));
    h.ever.store(true, Ordering::Relaxed);
    // §92.12: the installation its windows are named for, and where its
    // relaunch files are.
    let refused = crate::taskbar::add_session(index, sess.cfg.taskbar.clone(), sess.cfg.relaunch_dir.clone());
    if !refused.is_empty() {
        say(&format!(
            "taskbar: session {index} ({}) asked for the application-wide identity of {}, which another \
             installation's session holds; its windows get their own (§92.12)",
            sess.cfg.install,
            refused.join(", ")
        ));
    }
    let answer = {
        let _turn = h.boot_turn.lock().unwrap_or_else(|e| e.into_inner());
        boot(h, &sess)
    };
    match answer {
        Ok(msg) => {
            let s = Arc::clone(&sess);
            let _ = std::thread::Builder::new().name(format!("session-{index}")).spawn(move || services(s));
            Ok((sess, msg))
        }
        Err(e) => {
            end(&sess, &format!("its boot failed: {e}"));
            Err(e)
        }
    }
}

/// Write the record, ring, and wait for the answer.
fn boot(h: &Hub, sess: &Sess) -> Result<String, String> {
    use wc_handle::macboot;
    let cfg = &sess.cfg;
    let id = h.arena;
    let t0 = Instant::now();
    let text = std::fs::read_to_string(&cfg.key_file).map_err(|e| format!("the installation key {}: {e}", cfg.key_file.display()))?;
    let key = macboot::parse_key(&text).ok_or_else(|| format!("{} is not 64 hex digits", cfg.key_file.display()))?;
    let (run, region_len, rings_len) = wc_handle::arena::ivshmem_identity(id).map_err(|e| e.to_string())?;
    if rings_len == 0 {
        return Err("the region kept no Wayland rings, and without them nothing can cross".into());
    }
    let outbound = wc_handle::arena::outbound().filter(|w| w.arena == id).map_or(0, |w| w.len);
    let rec = macboot::Record {
        session: cfg.session.clone(),
        arena_run: run,
        arena_pages: (region_len / 4096) as u32,
        outbound,
        rings_run: run,
        rings_pages: (rings_len / 4096) as u32,
        gpu: cfg.gpu,
    };
    let auth = macboot::Auth::sign_indexed(&rec, sess.index, &cfg.install, now_micros(), &key);
    let page0 = wc_handle::arena::ivshmem_header_page(id).map_err(|e| e.to_string())?;
    // SAFETY: the header page of the device this process maps, for the life of
    // the process; nothing else in this process writes past the header's 80
    // bytes of it, and the boot turn keeps other boots out.
    unsafe { macboot::publish_indexed(page0, &rec, sess.index, Some(&auth)) };
    let bell = sv_transport::park::cross_wake();
    say(&format!(
        "boot: record for session {} of installation {} in the header page (run 0x{run:016x}, index {}, gpu {}); {}",
        cfg.session,
        cfg.install,
        sess.index,
        cfg.gpu,
        if bell.is_some() { "ringing the host's session service" } else { "NO DOORBELL: the host's service finds it on its timer" }
    ));
    let wait = Duration::from_secs(90);
    let mut last_hello: Option<Instant> = None;
    loop {
        // SAFETY: as above; the host writes the state word last, with the
        // message before it.
        match unsafe { macboot::load_state(page0) } {
            macboot::State::Located => {
                // SAFETY: as above.
                let page = unsafe { std::slice::from_raw_parts(page0, 4096) };
                let msg = macboot::message(page);
                say(&format!(
                    "boot: located by the host in {} ms: session {} index {}: {msg}",
                    t0.elapsed().as_millis(),
                    cfg.session,
                    sess.index
                ));
                return Ok(msg);
            }
            macboot::State::Failed => {
                // SAFETY: as above.
                let page = unsafe { std::slice::from_raw_parts(page0, 4096) };
                return Err(format!("REFUSED by the host's session service: {}", macboot::message(page)));
            }
            macboot::State::Waiting => {}
        }
        if t0.elapsed() > wait {
            return Err(format!(
                "NO ANSWER in {wait:?}: the host's session service (wc-sessiond.service) is not running, \
                 or this installation is not registered with it"
            ));
        }
        if let Some(b) = &bell
            && last_hello.is_none_or(|t| t.elapsed() >= Duration::from_secs(1))
        {
            b.poke(sv_transport::park::HELLO_TOKEN);
            last_hello = Some(Instant::now());
        }
        std::thread::sleep(Duration::from_millis(2));
    }
}

/// The ring slot the host claimed for a session service, once it has.
/// `owner` is the whole owner id, session index included.
fn service_channel(owner: u64, wait: Duration) -> Option<ByteChannel> {
    let t0 = Instant::now();
    while t0.elapsed() < wait {
        if let Some(c) = crate::ringlink::shared_control()
            && let Ok(live) = ByteChannel::live_connections(&c)
        {
            for slot in live {
                if c.ring_owner(slot).ok() == Some(owner)
                    && let Ok(ch) = ByteChannel::open(&c, slot, Endpoint::Peer)
                {
                    return Some(ch);
                }
            }
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    None
}

/// [`service_channel`] for one of `sess`'s services: its index in the owner
/// id, and its slot remembered so the session's end closes it.
///
/// §107.11: the [`SlotReader`] keeps the slot from being recycled while the
/// thread that holds it may still read it; the thread drops it only once it
/// will touch the ring no more. A channel found after the session ended (its
/// service was still waiting for the slot when [`end`] closed the others) is
/// closed here instead, under the same lock `end` takes, so no slot of an
/// ended session is ever left open.
fn session_channel(sess: &Sess, owner: u64, wait: Duration) -> Option<(ByteChannel, SlotReader)> {
    let ch = service_channel(wlbytes::with_session(owner, sess.index), wait)?;
    let reader = SlotReader::new(ch.slot());
    let mut slots = sess.service_slots.lock().unwrap_or_else(|e| e.into_inner());
    if sess.ended() {
        drop(slots);
        if let Some(c) = crate::ringlink::shared_control() {
            let _ = c.kill_ring(ch.slot());
            crate::ringlink::quarantine(ch.slot());
        }
        say(&format!("session {}: slot {} came after the session ended; closed", sess.index, ch.slot()));
        return None;
    }
    slots.push(ch.slot());
    Some((ch, reader))
}

/// §107.11: whether a service thread of `sess` must stop reading `ch` now:
/// its session has ended, or the slot is DEAD with nothing left to read.
fn service_over(sess: &Sess, ch: &ByteChannel) -> bool {
    sess.ended()
        || crate::ringlink::shared_control().is_some_and(|c| c.ring_state(ch.slot()).ok() == Some(sv_transport::control::RingState::Dead))
            && !ch.readable().unwrap_or(false)
}

/// §105.14: a request's thread, raised above normal priority while it waits
/// for its answer. Measured on a loaded guest (a game at half a core), a
/// wait that yielded with `SwitchToThread` handed its vCPU to the load and
/// got it back 7.6 ms later; raised, and yielding with `Sleep(0)`, which
/// gives way only to threads of the same or higher priority, it keeps the
/// vCPU for the at most [`spin`] it waits. Restored when dropped.
struct Urgent {
    #[cfg(windows)]
    was: i32,
}

#[cfg(windows)]
mod prio {
    pub const HIGHEST: i32 = 2;
    #[link(name = "kernel32")]
    unsafe extern "system" {
        pub fn GetCurrentThread() -> isize;
        pub fn GetThreadPriority(t: isize) -> i32;
        pub fn SetThreadPriority(t: isize, p: i32) -> i32;
        pub fn Sleep(ms: u32);
    }
}

impl Urgent {
    fn now() -> Urgent {
        #[cfg(windows)]
        {
            if !raised() {
                return Urgent { was: i32::MIN };
            }
            // SAFETY: the pseudo-handle of the calling thread; no other effect.
            let was = unsafe { prio::GetThreadPriority(prio::GetCurrentThread()) };
            // SAFETY: as above.
            unsafe { prio::SetThreadPriority(prio::GetCurrentThread(), prio::HIGHEST) };
            Urgent { was }
        }
        #[cfg(not(windows))]
        Urgent {}
    }
}

impl Drop for Urgent {
    fn drop(&mut self) {
        #[cfg(windows)]
        if self.was == i32::MIN {
            return;
        }
        #[cfg(windows)]
        // SAFETY: as in `now`.
        unsafe {
            prio::SetThreadPriority(prio::GetCurrentThread(), self.was);
        }
    }
}

/// `WC_REQ_PRIO=0` leaves every thread at its own priority, to measure.
#[cfg_attr(not(windows), allow(dead_code))]
fn raised() -> bool {
    static R: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *R.get_or_init(|| !std::env::var("WC_REQ_PRIO").is_ok_and(|v| v == "0"))
}

/// One turn of a wait's spin: give way to a thread of this priority that is
/// ready on this processor, and to nobody else (Windows); a yield elsewhere.
fn relax() {
    #[cfg(windows)]
    {
        if !raised() {
            std::thread::yield_now();
            return;
        }
        // SAFETY: no arguments that matter; Sleep(0) only gives up the slice.
        unsafe { prio::Sleep(0) };
    }
    #[cfg(not(windows))]
    std::thread::yield_now();
}

/// How long a request waits for its answer before it parks (§105.13): the
/// host waking from its park and answering takes 0.1–1.5 ms on an idle
/// guest, and an answer seen here needs no doorbell back (§105.14). A
/// yielding wait, and only while a request is outstanding.
/// `WC_CTL_SPIN_US` overrides it, for measuring (§105.14).
fn spin() -> Duration {
    static S: std::sync::OnceLock<Duration> = std::sync::OnceLock::new();
    *S.get_or_init(|| {
        std::env::var("WC_CTL_SPIN_US")
            .ok()
            .and_then(|v| v.parse().ok())
            .map_or(Duration::from_millis(2), Duration::from_micros)
    })
}

/// The control channel, shared by the lease and the requests. The channel
/// lock is held for one read or write at a time, never across a request, so
/// the lease's ping is never late behind a slow `wc-session` verb. `None`
/// once the session has ended: nothing of this process writes into its slot
/// again.
struct Ctl {
    ch: Mutex<Option<ByteChannel>>,
    /// One request at a time: replies are matched to requests by order.
    turn: Mutex<()>,
    /// §107.11: the control slot's reader, given up with the channel.
    reader: Mutex<Option<SlotReader>>,
}

impl Ctl {
    fn close(&self) {
        *self.ch.lock().unwrap_or_else(|e| e.into_inner()) = None;
        // After the channel: nothing of this process reads the slot now.
        self.reader.lock().unwrap_or_else(|e| e.into_inner()).take();
    }

    fn write_all(&self, mut b: &[u8]) -> Result<(), String> {
        let t0 = Instant::now();
        while !b.is_empty() {
            let n = {
                let mut g = self.ch.lock().unwrap_or_else(|e| e.into_inner());
                let ch = g.as_mut().ok_or("the session has ended")?;
                ch.write(b).map_err(|e| e.to_string())?
            };
            if n > 0 {
                b = &b[n..];
            } else if t0.elapsed() > Duration::from_secs(5) {
                return Err("the control ring stayed full for 5 s".into());
            } else if t0.elapsed() < Duration::from_millis(5) {
                // §105.13: the host drains a lane in microseconds.
                std::thread::yield_now();
            } else {
                std::thread::sleep(Duration::from_micros(200));
            }
        }
        Ok(())
    }

    /// Read at least one byte into `out`, or fail after `deadline`.
    fn read_some(&self, out: &mut Vec<u8>, deadline: Instant) -> Result<(), String> {
        let mut buf = [0u8; 16 * 1024];
        loop {
            {
                let mut g = self.ch.lock().unwrap_or_else(|e| e.into_inner());
                let ch = g.as_mut().ok_or("the session has ended")?;
                match ch.read(&mut buf).map_err(|e| e.to_string())? {
                    ReadOutcome::Bytes(n) => {
                        sv_transport::frametrace::hop("C.reply");
                        out.extend_from_slice(&buf[..n]);
                        return Ok(());
                    }
                    ReadOutcome::Eof => return Err("the host ended the control ring".into()),
                    _ => {}
                }
                if Instant::now() > deadline {
                    return Err("no answer on the control ring".into());
                }
                // §105.13: an answer the host makes in-process comes back in
                // tens of microseconds; look for it before parking, which
                // would add a doorbell round trip.
                let t0 = Instant::now();
                while t0.elapsed() < spin() && !ch.readable().unwrap_or(true) {
                    // §105.14: a yielding spin (a busy one held its vCPU
                    // against the thread it waited for), at the raised
                    // priority `Urgent` gives the request.
                    relax();
                }
                if ch.readable().unwrap_or(true) {
                    continue;
                }
                // Short, so the lease never waits long behind this lock.
                sv_transport::frametrace::hop("C.park");
                let _ = ch.wait_readable(crate::ringlink::ring_wait_slice().min(Duration::from_millis(20)));
                sv_transport::frametrace::hop("C.unpark");
            }
        }
    }

    /// §107: bytes left over from a request whose answer came after its
    /// deadline. Requests are strictly in turn, so anything readable before
    /// a request is written belongs to an earlier one and is dropped.
    fn drain_stale(&self) {
        let mut buf = [0u8; 4096];
        let mut g = self.ch.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(ch) = g.as_mut() {
            while let Ok(ReadOutcome::Bytes(_)) = ch.read(&mut buf) {}
        }
    }

    /// One request, and its whole answer: a line, plus the bytes an
    /// `ok-raw N` line announces.
    fn request(&self, line: &str) -> Result<Vec<u8>, String> {
        let _turn = self.turn.lock().unwrap_or_else(|e| e.into_inner());
        // Every verb answers in seconds; a proxy that ended without answering
        // (its lease, a crash) must not hold the launcher for minutes.
        self.exchange(line, Duration::from_secs(30))
    }

    /// [`Ctl::request`] for a caller that holds the turn.
    fn exchange(&self, line: &str, within: Duration) -> Result<Vec<u8>, String> {
        let _urgent = Urgent::now();
        self.drain_stale();
        sv_transport::frametrace::hop("C.ringtx0");
        self.write_all(format!("{line}\n").as_bytes())?;
        sv_transport::frametrace::hop("C.ringtx1");
        let deadline = Instant::now() + within;
        let mut got = Vec::new();
        let nl = loop {
            if let Some(i) = got.iter().position(|b| *b == b'\n') {
                break i;
            }
            self.read_some(&mut got, deadline)?;
        };
        let head = String::from_utf8_lossy(&got[..nl]).into_owned();
        if let Some(n) = head.strip_prefix("ok-raw ").and_then(|n| n.trim().parse::<usize>().ok()) {
            while got.len() < nl + 1 + n {
                self.read_some(&mut got, deadline)?;
            }
            got.truncate(nl + 1 + n);
        } else {
            got.truncate(nl + 1);
        }
        Ok(got)
    }
}

/// After the boot: control, lease, sound, and for a shared compositor the
/// session's arena and its watch.
fn services(sess: Arc<Sess>) {
    let Some((ch, reader)) = session_channel(&sess, OWNER_CONTROL, Duration::from_secs(20)) else {
        if !multi() {
            say("session: the host never opened the control ring; ending");
            std::process::exit(3);
        }
        end(&sess, "the host never opened its control ring");
        return;
    };
    say(&format!("session: control on ring slot {} (no ssh){}", ch.slot(), index_note(&sess)));
    let ctl = Arc::new(Ctl { ch: Mutex::new(Some(ch)), turn: Mutex::new(()), reader: Mutex::new(Some(reader)) });
    *sess.ctl.lock().unwrap_or_else(|e| e.into_inner()) = Some(Arc::clone(&ctl));
    sess.up.store(true, Ordering::SeqCst);
    if let Some(h) = hub() {
        h.changed.notify_all();
    }
    {
        let (ctl, sess) = (Arc::clone(&ctl), Arc::clone(&sess));
        let _ = std::thread::Builder::new().name("session-lease".into()).spawn(move || loop {
            std::thread::sleep(Duration::from_millis(500));
            if sess.ended() {
                return;
            }
            if let Err(e) = ctl.write_all(b"ping\n") {
                say(&format!("session: the lease could not ping ({e}); the host will end the session"));
                if multi() {
                    end(&sess, &format!("the lease could not ping: {e}"));
                }
                return;
            }
        });
    }
    if let Some(path) = sess.cfg.control_socket.clone() {
        let (ctl, sess) = (Arc::clone(&ctl), Arc::clone(&sess));
        let _ = std::thread::Builder::new().name("session-control".into()).spawn(move || serve_socket(path, ctl, sess));
    }
    if let Some(vol) = sess.cfg.audio_volume {
        let s = Arc::clone(&sess);
        let _ = std::thread::Builder::new().name("session-sound".into()).spawn(move || {
            // §107.11: the sound ring's thread ends when the slot is DEAD,
            // which the session's end makes it; the reader is held until it
            // has (both players below return only after the ring's end).
            match session_channel(&s, OWNER_AUDIO, Duration::from_secs(20)) {
                Some((ch, _reader)) => {
                    say(&format!("audio: the session's sound ring is slot {} (§103.16, no ssh){}", ch.slot(), index_note(&s)));
                    // §106.3: every endpoint a stream, both directions, the
                    // defaults following Windows'. The single default-output
                    // player stays for the Linux test fixture.
                    #[cfg(windows)]
                    {
                        let _ = vol;
                        match crate::ringlink::shared_control() {
                            Some(control) => {
                                if let Err(e) = crate::sound_win::serve(ch, &control, say) {
                                    say(&format!("audio: the sound ring: {e}; no sound this session"));
                                }
                            }
                            None => say("audio: no control region; no sound this session"),
                        }
                    }
                    #[cfg(not(windows))]
                    sound_ring(ch, vol);
                }
                None => say("audio: the host opened no sound ring; no sound this session"),
            }
        });
    }
    if multi() {
        let s = Arc::clone(&sess);
        let _ = std::thread::Builder::new().name("session-arena".into()).spawn(move || serve_arena(s));
        let (ctl, s) = (Arc::clone(&ctl), Arc::clone(&sess));
        let _ = std::thread::Builder::new().name("session-watch".into()).spawn(move || watch(ctl, s));
    }
    // §106: host integration (links and files both ways) on its own slot and
    // thread, when the launcher passed `--hostint`. Not on macOS: there the
    // main process serves the slot itself (wc-launch mac_hostint.rs), in
    // both roles.
    #[cfg(not(target_os = "macos"))]
    if let Some(hcfg) = crate::hostint::configured() {
        // §107: the session's own slot, its index in the owner id.
        // §107.11: served on this thread, which holds the slot's reader
        // until the serving ends: at the session's end, or the slot's.
        let hs = Arc::clone(&sess);
        let _ = std::thread::Builder::new().name("session-hostint".into()).spawn(move || {
            match session_channel(&hs, sv_transport::wlbytes::OWNER_HOSTINT, Duration::from_secs(20)) {
                Some((ch, _reader)) => {
                    say(&format!("hostint: on ring slot {} (no ssh){}", ch.slot(), index_note(&hs)));
                    let s2 = Arc::clone(&hs);
                    let stop: crate::hostint::Stop = Box::new(move || s2.ended());
                    #[cfg(windows)]
                    crate::hostint::serve_windows(ch, hcfg, say, stop);
                    #[cfg(target_os = "linux")]
                    crate::hostint::serve_logging(ch, hcfg, say, stop);
                }
                None if hs.ended() => {}
                None => say("hostint: the host opened no host-integration slot; links and files stay on their side"),
            }
        });
    }
    // [hostint hook] A further service channel (e.g. OWNER_HOSTINT) is opened
    // here, on a thread of its own, with `session_channel(&sess, owner, wait)`
    // (the session's index goes into the owner id; `service_channel(owner,
    // wait)` takes a whole owner id).
}

fn index_note(sess: &Sess) -> String {
    if sess.index == 0 { String::new() } else { format!(", session {} of {}", sess.index, sess.cfg.install) }
}

/// §107: end `sess`. Its services stop writing, its slots are closed and
/// quarantined, the Wayland thread disconnects its clients, and then its
/// arena regions go back to the pool. Every other session is untouched.
fn end(sess: &Arc<Sess>, why: &str) {
    if sess.ended.swap(true, Ordering::SeqCst) {
        return;
    }
    *sess.why.lock().unwrap_or_else(|e| e.into_inner()) = why.to_string();
    say(&format!(
        "session {} (index {}, installation {}) ENDED after {} s: {why}",
        sess.cfg.session,
        sess.index,
        sess.cfg.install,
        sess.booted_at.elapsed().as_secs()
    ));
    if let Some(ctl) = sess.ctl.lock().unwrap_or_else(|e| e.into_inner()).take() {
        ctl.close();
    }
    let Some(h) = hub() else { return };
    h.changed.notify_all();
    if let Some(c) = crate::ringlink::shared_control() {
        for slot in sess.service_slots.lock().unwrap_or_else(|e| e.into_inner()).iter() {
            let _ = c.kill_ring(*slot);
            crate::ringlink::quarantine(*slot);
        }
    }
    if let Some(p) = &sess.cfg.control_socket {
        let _ = std::fs::remove_file(p);
    }
    if multi() {
        // Its clients first: a region given to somebody else while a window
        // of this session still showed it would show their pixels.
        let t0 = Instant::now();
        let mut sessions = h.sessions.lock().unwrap_or_else(|e| e.into_inner());
        while !sess.dropped.load(Ordering::SeqCst) && t0.elapsed() < Duration::from_secs(3) {
            sessions = h.changed.wait_timeout(sessions, Duration::from_millis(50)).map(|(g, _)| g).unwrap_or_else(|e| e.into_inner().0);
        }
        drop(sessions);
        let gone = h.pool.lock().unwrap_or_else(|e| e.into_inner()).reclaim(sess.index);
        for g in &gone {
            let _ = wc_handle::arena::detach_ivshmem_window(g.id);
            release_external(g);
        }
        if !gone.is_empty() {
            let (free, largest) = h.pool.lock().unwrap_or_else(|e| e.into_inner()).free();
            say(&format!(
                "arena: session {}'s {} region(s), {} MiB, back in the pool ({} MiB free, largest run {} MiB)",
                sess.index,
                gone.len(),
                gone.iter().map(|g| g.len).sum::<u64>() >> 20,
                free >> 20,
                largest >> 20
            ));
        }
    }
    crate::taskbar::remove_session(sess.index);
    h.sessions.lock().unwrap_or_else(|e| e.into_inner()).retain(|s| !Arc::ptr_eq(s, sess));
    h.changed.notify_all();
}

/// §107: a session ends when its proxy closes its control slot, when the
/// proxy stops answering (`rtt` every two seconds, twice missed), or when its
/// last client has been gone for the linger, which also tells the proxy
/// `down`. A proxy that crashed is the first or the second; a flood of
/// requests from one session waits for its own turn and nobody else's.
fn watch(ctl: Arc<Ctl>, sess: Arc<Sess>) {
    let Some(control) = crate::ringlink::shared_control() else { return };
    let slot = sess.service_slots.lock().unwrap_or_else(|e| e.into_inner()).first().copied();
    let linger = hub().and_then(|h| h.linger);
    let mut misses = 0u32;
    let mut last_probe = Instant::now();
    loop {
        std::thread::sleep(Duration::from_millis(250));
        if sess.ended() {
            return;
        }
        if let Some(slot) = slot
            && control.ring_state(slot).ok() != Some(sv_transport::control::RingState::Live)
        {
            end(&sess, "its proxy closed the control ring");
            return;
        }
        if let Some(linger) = linger
            && sess.had_clients.load(Ordering::Relaxed)
            && sess.clients.load(Ordering::Relaxed) == 0
            && sess.empty_since.lock().unwrap_or_else(|e| e.into_inner()).is_some_and(|t| t.elapsed() >= linger)
        {
            let said = ctl.request("raw down").map(|b| String::from_utf8_lossy(&b).trim().to_string());
            end(&sess, &format!("its last client left {} s ago (down: {})", linger.as_secs(), said.unwrap_or_else(|e| e)));
            return;
        }
        if last_probe.elapsed() >= Duration::from_secs(2) {
            last_probe = Instant::now();
            // Only between requests: a request in flight is its own probe.
            if let Ok(turn) = ctl.turn.try_lock() {
                match ctl.exchange("rtt", Duration::from_secs(2)) {
                    Ok(_) => misses = 0,
                    Err(e) => {
                        misses += 1;
                        if misses >= 2 {
                            drop(turn);
                            end(&sess, &format!("its proxy is silent ({e})"));
                            return;
                        }
                    }
                }
            }
        }
    }
}

/// §107: the session's arena slot. The proxy asks for regions of the shared
/// BAR and gives them back; each grant is registered as an arena id of its
/// own, which only this session's clients may name.
fn serve_arena(sess: Arc<Sess>) {
    let Some(h) = hub() else { return };
    // §107.11: the reader is held for the whole loop, which ends with the
    // session or the slot.
    let Some((mut ch, _reader)) = session_channel(&sess, OWNER_ARENA, Duration::from_secs(30)) else {
        if !sess.ended() {
            say(&format!("arena: session {} opened no arena channel; its proxy allocates nothing", sess.index));
        }
        return;
    };
    say(&format!("arena: session {} asks for regions on ring slot {}", sess.index, ch.slot()));
    let mut buf = Vec::new();
    let mut tmp = [0u8; 1024];
    while !sess.ended() {
        match ch.read(&mut tmp) {
            Ok(ReadOutcome::Bytes(n)) => buf.extend_from_slice(&tmp[..n]),
            Ok(ReadOutcome::Eof) | Err(_) => break,
            Ok(ReadOutcome::Empty) if service_over(&sess, &ch) => break,
            Ok(ReadOutcome::Empty) => {
                let _ = ch.wait_readable(Duration::from_millis(100));
                continue;
            }
        }
        if buf.len() > 4096 {
            say(&format!("arena: session {} sent a line longer than 4 KiB; its arena channel is closed", sess.index));
            break;
        }
        while let Some(i) = buf.iter().position(|b| *b == b'\n') {
            let line: Vec<u8> = buf.drain(..=i).collect();
            let line = String::from_utf8_lossy(&line[..i]).trim().to_string();
            let reply = arena_request(h, &sess, &line);
            let mut out = reply.into_bytes();
            out.push(b'\n');
            let mut b = &out[..];
            let t0 = Instant::now();
            while !b.is_empty() && t0.elapsed() < Duration::from_secs(2) {
                match ch.write(b) {
                    Ok(0) => std::thread::sleep(Duration::from_millis(1)),
                    Ok(n) => b = &b[n..],
                    Err(_) => return,
                }
            }
        }
    }
}

/// One arena request of session `sess`: `grow MIN`, `release ID`, `regions`.
fn arena_request(h: &Hub, sess: &Sess, line: &str) -> String {
    let w: Vec<&str> = line.split_whitespace().collect();
    match w.as_slice() {
        ["grow", n] => {
            let Ok(min) = n.parse::<u64>() else { return format!("err {n:?} is not a size") };
            let mut pool = h.pool.lock().unwrap_or_else(|e| e.into_inner());
            match pool.grant(sess.index, min) {
                Ok(g) => {
                    if let Err(e) = wc_handle::arena::attach_ivshmem_window(g.id, h.arena, g.offset, g.len) {
                        let _ = pool.release(sess.index, g.id);
                        return format!("err the window could not be registered: {e}");
                    }
                    let (free, _) = pool.free();
                    say(&format!(
                        "arena: session {} granted region {:#x}: {} MiB at {} MiB (it holds {} MiB in {}; {} MiB free)",
                        sess.index,
                        g.id,
                        g.len >> 20,
                        g.offset >> 20,
                        pool.held(sess.index) >> 20,
                        pool.regions_of(sess.index).len(),
                        free >> 20
                    ));
                    format!("ok {} {} {}", g.id, g.offset, g.len)
                }
                Err(e) => {
                    // §110: the BAR is full for this session; the pool's other
                    // regions (the GPU device's, when this process runs the GPU
                    // pool) are asked in turn.
                    drop(pool);
                    let mut why = e.to_string();
                    for (region, base, src) in pool_regions() {
                        match src.lease(sess.index, &sess.cfg.install, min) {
                            Ok((off, len)) => match grant_external(h, sess, region, base, &src, off, len) {
                                Ok(reply) => return reply,
                                Err(e) => why = format!("{why}; region {region}: {e}"),
                            },
                            Err(e) => why = format!("{why}; region {region} ({}): {e}", src.describe()),
                        }
                    }
                    let n = sess.refused.fetch_add(1, Ordering::Relaxed) + 1;
                    if n.is_power_of_two() {
                        say(&format!("arena: session {} REFUSED {min} bytes: {why} (refusal {n})", sess.index));
                    }
                    format!("err {why}")
                }
            }
        }
        ["release", id] => {
            let Ok(id) = id.parse::<u32>() else { return format!("err {id:?} is not a region") };
            let mut pool = h.pool.lock().unwrap_or_else(|e| e.into_inner());
            match pool.release(sess.index, id) {
                Ok(g) => {
                    let _ = wc_handle::arena::detach_ivshmem_window(g.id);
                    release_external(&g);
                    say(&format!(
                        "arena: session {} gave back region {:#x} ({} MiB); it holds {} MiB",
                        sess.index,
                        g.id,
                        g.len >> 20,
                        pool.held(sess.index) >> 20
                    ));
                    "ok".into()
                }
                Err(e) => format!("err {e}"),
            }
        }
        ["regions"] => {
            let pool = h.pool.lock().unwrap_or_else(|e| e.into_inner());
            format!("ok {} {}", pool.regions_of(sess.index).len(), pool.held(sess.index))
        }
        _ => format!("err unknown arena request {line:?}"),
    }
}

/// A range another region's allocator leased to `sess`, recorded in the
/// pool's table under a new id and registered as a window of that region.
fn grant_external(h: &Hub, sess: &Sess, region: u32, base: u32, src: &Arc<dyn PoolRegion>, off: u64, len: u64) -> Result<String, String> {
    let mut pool = h.pool.lock().unwrap_or_else(|e| e.into_inner());
    let g = pool.adopt(sess.index, region, off, len).map_err(|e| {
        src.release(off);
        e.to_string()
    })?;
    if let Err(e) = wc_handle::arena::attach_ivshmem_window(g.id, base, off, len) {
        let _ = pool.release(sess.index, g.id);
        src.release(off);
        return Err(format!("the window could not be registered: {e}"));
    }
    say(&format!(
        "arena: session {} granted region {:#x}: {} MiB at {} MiB of pool region {region} ({})",
        sess.index,
        g.id,
        g.len >> 20,
        g.offset >> 20,
        src.describe()
    ));
    Ok(format!("ok {} region {region} {} {}", g.id, g.offset, g.len))
}

/// Give a grant of another region back to its allocator.
fn release_external(g: &wc_handle::barpool::Grant) {
    if g.region == 0 {
        return;
    }
    if let Some((_, _, src)) = pool_regions().into_iter().find(|(r, _, _)| *r == g.region) {
        src.release(g.offset);
    }
}

// ------------------------------------------------ §110: one pool, many regions ---

/// §110: the first bytes of the shared compositor's arena kept for §108.6's
/// region table (`BarPool::with_front`).
pub const REGION_TABLE_BYTES: u64 = 64 << 10;

/// The arena id a pool region's base is registered under, so that its grants
/// are windows of it (`wc_handle::arena::attach_ivshmem_window`).
pub const REGION_BASE_ID: u32 = 0xF000_0000;

/// §110: a region of the one pool that is not this BAR: another allocator's
/// memory this process maps, from which the shared compositor leases a
/// session's Wayland buffers when its BAR is full. The GPU device's pool is
/// the one there is (`sv-guest`, when this process also runs the GPU pool);
/// a guest-RAM region announced in the region table (§108) is the next.
/// Substrate-agnostic: a region is a mapping and an allocator, nothing more.
pub trait PoolRegion: Send + Sync {
    /// `len` bytes for session `session` of installation `install`, as
    /// `(offset, len)` within this region's mapping.
    fn lease(&self, session: u8, install: &str, len: u64) -> Result<(u64, u64), String>;
    /// The range at `offset` goes back.
    fn release(&self, offset: u64);
    /// For the log: what the region is.
    fn describe(&self) -> String;
}

type Regions = Vec<(u32, u32, Arc<dyn PoolRegion>)>;
static POOL_REGIONS: Mutex<Regions> = Mutex::new(Vec::new());

fn pool_regions() -> Regions {
    POOL_REGIONS.lock().unwrap_or_else(|e| e.into_inner()).clone()
}

/// §110: add region `region` (1 and up; 0 is this BAR) of the one pool: the
/// memory at `[ptr, ptr + len)`, which this process keeps mapped for its life,
/// with `src` its allocator. A proxy is told its grants as `ok ID region K
/// OFFSET LEN`, and maps region K by its own configuration (`--arena-region`).
///
/// # Safety
/// `ptr` must be valid for reads of `len` bytes for the rest of the process.
pub unsafe fn add_pool_region(region: u32, ptr: *const u8, len: usize, src: Arc<dyn PoolRegion>) -> Result<(), String> {
    if region == 0 {
        return Err("region 0 is the BAR".into());
    }
    let base = REGION_BASE_ID + region;
    let origin = format!("pool region {region}: {}", src.describe());
    // SAFETY: the caller's contract.
    unsafe { wc_handle::arena::register_borrowed_base(base, ptr, len, origin) }.map_err(|e| e.to_string())?;
    say(&format!("sessions: pool region {region} ({} MiB, {}) serves Wayland buffers when a session's BAR is full", len >> 20, src.describe()));
    POOL_REGIONS.lock().unwrap_or_else(|e| e.into_inner()).push((region, base, src));
    Ok(())
}

/// §110, for the harness and for a machine with no GPU pool in this process:
/// a plain file standing in for the GPU device, sub-allocated by the same
/// lease allocator the GPU pool uses (`sv_arena::pool`).
pub struct FileRegion {
    pool: Mutex<sv_arena::pool::Pool>,
    sessions: Mutex<std::collections::HashMap<u8, u64>>,
    path: PathBuf,
}

impl FileRegion {
    /// Map `path` read-write (it must exist) and make it region `region`.
    pub fn add(region: u32, path: &std::path::Path) -> Result<(), String> {
        let len = std::fs::metadata(path).map_err(|e| format!("{}: {e}", path.display()))?.len();
        let view = wc_handle::arena::map_file_for_region(path, len).map_err(|e| format!("{}: {e}", path.display()))?;
        let cfg = sv_arena::pool::PoolConfig { reserve: 0, ..sv_arena::pool::PoolConfig::for_capacity(len) };
        let pool = sv_arena::pool::Pool::new(cfg, len).map_err(|e| e.to_string())?;
        let src = Arc::new(FileRegion { pool: Mutex::new(pool), sessions: Mutex::new(Default::default()), path: path.to_path_buf() });
        // SAFETY: `map_file_for_region` keeps the mapping for the process.
        unsafe { add_pool_region(region, view, len as usize, src) }
    }
}

impl PoolRegion for FileRegion {
    fn lease(&self, session: u8, install: &str, len: u64) -> Result<(u64, u64), String> {
        let mut pool = self.pool.lock().unwrap_or_else(|e| e.into_inner());
        let s = *self.sessions.lock().unwrap_or_else(|e| e.into_inner()).entry(session).or_insert_with(|| {
            let s = 1000 + u64::from(session);
            pool.open_session(s, &format!("wayland {install}"));
            s
        });
        let l = pool.lease(s, len, sv_arena::pool::Purpose::Dedicated).map_err(|e| e.to_string())?;
        Ok((sv_arena::pool::split(l.offset).1, l.len))
    }

    fn release(&self, offset: u64) {
        let _ = self.pool.lock().unwrap_or_else(|e| e.into_inner()).release(offset);
    }

    fn describe(&self) -> String {
        format!("the lease pool over {}", self.path.display())
    }
}

/// The output stream this side announces: Windows' default output device.
#[cfg(not(windows))]
const OUT_STREAM: u16 = 1;
/// What the session may send before the device has played anything: 80 ms.
/// With the device's own queue (`audio::BUFFERS`) that bounds the delay.
#[cfg(not(windows))]
const CREDIT_INIT: u32 = 4 * 3840;

/// §103.16 on Windows: this side is the **device side** of the sound ring.
/// It announces one output stream, Windows' default device, which the host's
/// proxy makes the session's default sink (a pipe sink, paced by this side's
/// credit), plays what arrives through waveOut, and gives the credit back as
/// it hands each chunk to the device. Nothing is dropped on the way: a device
/// that is slow slows the sink. Microphones are not announced (see §105).
#[cfg(not(windows))]
fn sound_ring(ch: ByteChannel, vol: u8) {
    use sv_transport::soundring::{Dir, Msg, VERSION};
    let Some(control) = crate::ringlink::shared_control() else { return };
    let (tx, rx) = std::sync::mpsc::channel::<Vec<u8>>();
    let tx = Arc::new(Mutex::new(Some(tx)));
    let tx_end = Arc::clone(&tx);
    let link = sv_transport::soundring::spawn(
        ch,
        &control,
        crate::ringlink::ring_wait_slice(),
        move |m, _link| match m {
            Msg::Data { stream: OUT_STREAM, pcm } => {
                if let Some(t) = tx.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
                    let _ = t.send(pcm);
                }
            }
            Msg::Hello { version } => say(&format!("audio: the session side speaks sound ring version {version}")),
            _ => {}
        },
        // §107: the player ends with the ring, so a session that ended takes
        // its waveOut device with it.
        move |why| {
            say(&format!("audio: the sound ring ended: {why}"));
            tx_end.lock().unwrap_or_else(|e| e.into_inner()).take();
        },
    );
    let link = match link {
        Ok(l) => l,
        Err(e) => {
            say(&format!("audio: the sound ring: {e}; no sound this session"));
            return;
        }
    };
    link.send(Msg::Hello { version: VERSION });
    link.send(Msg::Device { stream: OUT_STREAM, dir: Dir::Out, channels: 2, rate: 48_000, name: "Windows (default output)".into() });
    link.send(Msg::Default { stream: OUT_STREAM, dir: Dir::Out });
    link.send(Msg::Credit { stream: OUT_STREAM, bytes: CREDIT_INIT });
    play(rx, vol, link);
}

#[cfg(not(windows))]
fn play(rx: std::sync::mpsc::Receiver<Vec<u8>>, _vol: u8, link: sv_transport::soundring::Link) {
    // Off Windows only the test fixture runs this: consume and credit, as a
    // device would, so the session's sink keeps moving.
    let mut total = 0u64;
    while let Ok(b) = rx.recv() {
        if total == 0 {
            say("audio: the first bytes arrived on the sound ring");
        }
        total += b.len() as u64;
        if total / 192_000 != (total - b.len() as u64) / 192_000 {
            say(&format!("audio: {total} bytes through the sound ring"));
        }
        link.send(sv_transport::soundring::Msg::Credit { stream: OUT_STREAM, bytes: b.len() as u32 });
    }
}

/// The launcher's socket: request lines, each answered in order. Ends, and
/// the socket goes, with the session.
fn serve_socket(path: PathBuf, ctl: Arc<Ctl>, sess: Arc<Sess>) {
    let _ = std::fs::remove_file(&path);
    let listener = match wc_transport::Listener::bind(&path) {
        Ok(l) => l,
        Err(e) => {
            say(&format!("session: cannot listen on {}: {e}; the launcher cannot reach the host", path.display()));
            return;
        }
    };
    say(&format!("session: control socket {}", path.display()));
    while !sess.ended() {
        let conn = match wc_transport::accept(&listener) {
            Ok(Some(c)) => c,
            Ok(None) => {
                std::thread::sleep(Duration::from_millis(20));
                continue;
            }
            Err(e) => {
                say(&format!("session: control socket accept: {e}"));
                std::thread::sleep(Duration::from_millis(200));
                continue;
            }
        };
        let ctl = Arc::clone(&ctl);
        let _ = std::thread::Builder::new().name("session-request".into()).spawn(move || answer(conn, &ctl));
    }
    drop(listener);
    let _ = std::fs::remove_file(&path);
}

/// Read request lines from a local connection until it closes; `f` answers
/// each. Shared by the launcher's socket and the user's.
fn lines(mut conn: wc_transport::Stream, mut f: impl FnMut(&str) -> Vec<u8>) {
    let _ = conn.set_nonblocking(false);
    let _ = conn.set_read_timeout(Some(Duration::from_secs(600)));
    let mut buf = Vec::with_capacity(256);
    let mut tmp = [0u8; 4096];
    loop {
        let line = loop {
            if let Some(i) = buf.iter().position(|b| *b == b'\n') {
                let l: Vec<u8> = buf.drain(..=i).collect();
                break String::from_utf8_lossy(&l[..l.len() - 1]).trim_end_matches('\r').to_string();
            }
            if buf.len() > 16 * 1024 {
                return;
            }
            match conn.read(&mut tmp) {
                Ok(0) | Err(_) => return,
                Ok(n) => buf.extend_from_slice(&tmp[..n]),
            }
        };
        sv_transport::frametrace::hop("C.line");
        if conn.write_all(&f(&line)).is_err() {
            return;
        }
        sv_transport::frametrace::hop("C.socktx");
    }
}

/// One connection of a launcher: request lines, each answered in order, until
/// it closes. A launcher sends one; `--ping N` sends N on one connection.
fn answer(conn: wc_transport::Stream, ctl: &Ctl) {
    // §105.14: HIGHEST for as long as the connection lives. The thread runs
    // only when a request arrives; on a loaded guest a normal one waited up
    // to 133 ms for its vCPU after the launcher had written, and an
    // above-normal one still 53 ms: a readied thread that does not outrank
    // the thread running where it is queued waits for that thread's quantum
    // to end (one or two 15.6 ms ticks).
    #[cfg(windows)]
    if raised() {
        // SAFETY: the calling thread's pseudo-handle.
        unsafe {
            prio::SetThreadPriority(prio::GetCurrentThread(), prio::HIGHEST);
        }
    }
    lines(conn, |line| {
        let line = line.trim();
        // `ping` is the lease's word on the ring; a launcher's bare `ping` is
        // answered here, from this side. `rtt` crosses (§105.13).
        if line == "ping" {
            b"ok session up\n".to_vec()
        } else {
            match ctl.request(line) {
                Ok(v) => v,
                Err(e) => format!("err {e}\n").into_bytes(),
            }
        }
    });
}

// ------------------------------------------------- §107: the user's socket ---

/// Serve the user's control socket: every installation's launcher boots its
/// session here. In the user's profile, as the launchers' own sockets are.
///
/// ```text
///   boot<TAB>session=ID<TAB>install=NAME<TAB>key=PATH[<TAB>control=PATH]
///       [<TAB>gpu=1][<TAB>audio=VOL][<TAB>nested=1][<TAB>relaunch=DIR]
///                       -> ok located INDEX MESSAGE | err WHY
///   wait ID             -> ok ended WHY            (when session ID has ended)
///   sessions            -> ok INDEX:INSTALL:ID:STATE:MiB/REGIONS ...
///   ping                -> ok compositor PID
/// ```
pub fn serve_user(path: PathBuf) {
    let _ = std::thread::Builder::new().name("user-control".into()).spawn(move || {
        let _ = std::fs::remove_file(&path);
        let listener = match wc_transport::Listener::bind(&path) {
            Ok(l) => l,
            Err(e) => {
                say(&format!("sessions: CANNOT LISTEN on {}: {e}; no launcher can boot a session", path.display()));
                return;
            }
        };
        say(&format!("sessions: user control socket {}", path.display()));
        loop {
            match wc_transport::accept(&listener) {
                Ok(Some(c)) => {
                    let _ = std::thread::Builder::new().name("user-request".into()).spawn(move || lines(c, user_request));
                }
                Ok(None) => std::thread::sleep(Duration::from_millis(10)),
                Err(e) => {
                    say(&format!("sessions: accept: {e}"));
                    std::thread::sleep(Duration::from_millis(200));
                }
            }
        }
    });
}

fn user_request(line: &str) -> Vec<u8> {
    let reply = if line == "ping" {
        format!("ok compositor {}", std::process::id())
    } else if line == "sessions" {
        hub().map_or_else(|| "err the arena is not published yet".into(), |h| format!("ok {}", describe(h)))
    } else if let Some(id) = line.strip_prefix("wait ") {
        format!("ok ended {}", wait_ended(id.trim()))
    } else if line.starts_with("boot\t") {
        match Config::parse_request(line).and_then(boot_session) {
            Ok((s, msg)) => format!("ok located {} {}", s.index, msg.replace('\n', " ")),
            Err(e) => format!("err {}", e.replace('\n', " ")),
        }
    } else {
        format!("err unknown request {:?}", line.chars().take(40).collect::<String>())
    };
    format!("{reply}\n").into_bytes()
}

/// Block until session `id` is no longer live; why it ended, if this process
/// knew it.
fn wait_ended(id: &str) -> String {
    let Some(h) = hub() else { return "no sessions".into() };
    let mut g = h.sessions.lock().unwrap_or_else(|e| e.into_inner());
    loop {
        match g.iter().find(|s| s.cfg.session == id) {
            None => return "(no such session now)".into(),
            Some(s) if s.ended() => return s.why.lock().unwrap_or_else(|e| e.into_inner()).clone(),
            Some(_) => {}
        }
        g = h.changed.wait_timeout(g, Duration::from_millis(500)).map(|(g, _)| g).unwrap_or_else(|e| e.into_inner().0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_boot_request_round_trips_and_spaces_in_paths_survive() {
        let c = Config {
            session: "wcivs-4242".into(),
            install: "wcivs".into(),
            key_file: r"C:\Users\First Last\wcivs\session.key".into(),
            gpu: true,
            control_socket: Some(r"C:\Users\First Last\AppData\Local\waycompose\wc-launch-wcivs.ctl".into()),
            audio_volume: Some(80),
            nested: true,
            relaunch_dir: Some(r"C:\Users\First Last\wcivs\apps".into()),
            taskbar: Some(crate::taskbar::Scope::parse("wcivs:firefox").unwrap()),
        };
        let line = c.request_line();
        assert!(!line.contains('\n'));
        let back = Config::parse_request(&line).unwrap();
        assert_eq!(back.request_line(), line);
        assert_eq!(back.key_file, c.key_file);
        assert!(back.gpu && back.nested);
        assert_eq!(back.taskbar, c.taskbar);
        assert_eq!(back.audio_volume, Some(80));
    }

    /// §107: a client names only its own session's regions.
    #[test]
    fn a_client_may_name_only_its_own_sessions_regions() {
        let mut pool = BarPool::new(64 << 20);
        let a = pool.grant(1, 1 << 20).unwrap();
        let b = pool.grant(2, 1 << 20).unwrap();
        assert!(slice_allowed(&pool, 1, a.id).is_ok());
        assert!(slice_allowed(&pool, 2, a.id).unwrap_err().contains("session 1's memory"));
        assert!(slice_allowed(&pool, 1, 0).unwrap_err().contains("no session's region"), "the whole BAR");
        pool.release(2, b.id).unwrap();
        assert!(slice_allowed(&pool, 2, b.id).is_err(), "a region given back is nobody's");
    }

    #[test]
    fn a_boot_request_with_a_bad_field_is_refused() {
        let ok = "boot\tsession=s-1\tinstall=wcivs\tkey=k";
        assert!(Config::parse_request(ok).is_ok());
        assert!(Config::parse_request("boot\tsession=s;1\tinstall=wcivs\tkey=k").unwrap_err().contains("session id"));
        assert!(Config::parse_request("boot\tsession=s-1\tinstall=../x\tkey=k").unwrap_err().contains("installation"));
        assert!(Config::parse_request("boot\tsession=s-1\tinstall=wcivs").unwrap_err().contains("key file"));
        assert!(Config::parse_request(&format!("{ok}\tcolour=red")).unwrap_err().contains("unknown boot field"));
        assert!(Config::parse_request(&format!("{ok}\taudio=loud")).is_err());
        assert!(Config::parse_request("bootx\tsession=s-1").is_err());
    }

    /// §92.12: the session names its own installation's windows: a scope
    /// of another installation, or one that is not a scope, is refused.
    #[test]
    fn a_boot_s_taskbar_scope_is_its_own_installation_s() {
        let ok = "boot\tsession=s-1\tinstall=wcivs\tkey=k";
        let c = Config::parse_request(&format!("{ok}\ttaskbar=wcivs:firefox")).unwrap();
        assert_eq!(c.taskbar.unwrap().identity("firefox", 1), "Waycompose.app.firefox");
        let c = Config::parse_request(&format!("{ok}\ttaskbar=wcivs")).unwrap();
        assert_eq!(c.taskbar.unwrap().identity("firefox", 1), "Waycompose.install.wcivs.firefox");
        assert!(Config::parse_request(ok).unwrap().taskbar.is_none(), "a launcher before §92.12");
        assert!(Config::parse_request(&format!("{ok}\ttaskbar=wcpins")).unwrap_err().contains("not installation wcivs"));
        assert!(Config::parse_request(&format!("{ok}\ttaskbar=wcivs:a b")).is_err());
    }
}
