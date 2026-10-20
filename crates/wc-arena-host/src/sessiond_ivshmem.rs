//! `wc-arena-host sessiond --ivshmem` (waycompose-design.md §105): the host's
//! session service for a guest whose shared memory is an ivshmem device (the
//! Windows VM). It is §103.15's service, with a different way to find the
//! record and an authenticator, and it is what replaced the guest's ssh key.
//!
//! The guest's compositor maps the device, publishes its header (§83) and the
//! Wayland rings at the tail (§89), then writes a boot record into the header
//! page (`wc_handle::macboot`, at the same offset the macOS ring window uses)
//! with an authenticator under the installation's key, and rings
//! `HELLO_TOKEN` on the rings' doorbell. This service:
//!
//! 1. on a hello, on every (re)connect to the doorbell hub (a byte sent while
//!    nobody held the port is lost, so a reconnect is a reason to look), and
//!    every `--poll-ms` (one `pread` of one page), reads page 0 of the file
//!    QEMU backs the BAR with. No sweep: the device is at a known path;
//! 2. takes a record only if the header's magic and run id match it, it is
//!    still `Waiting`, the page reads the same twice, the installation is one
//!    of `--installs`, its MAC verifies under that installation's key, and its
//!    sequence number is above the last one accepted for it;
//! 3. runs that installation's own `wc-session`: `up ID RUN ivshmem`,
//!    `proxy ID` and, if the record asks, `gpu ID`, with
//!    `WC_LAUNCH_SERVICE=1` so they know nobody's ssh tunnel is involved;
//! 4. answers in the page, message first and state word last;
//! 5. runs `wc-session down ID` once the session's proxy unit has ended,
//!    which it does by itself when the guest stops pinging its control ring
//!    or says `down` on it.
//!
//! An installation is a name in `--installs DIR`: `DIR/NAME` is a symlink to
//! (or a directory that is) the installation's base, which holds `wc-session`
//! and `session.key` (64 hex digits, mode 0600, written by deploy).

use std::collections::{HashMap, HashSet};
use std::os::unix::fs::{FileExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use wc_handle::macboot;

struct Args {
    arena: PathBuf,
    installs: PathBuf,
    state: PathBuf,
    bell: Option<String>,
    poll: Duration,
    /// §111.10: the root-owned table that maps a record's identity to a
    /// Linux user, and the server (VM) this service serves. With it, every
    /// session runs as its mapped user through that user's agent.
    users: Option<PathBuf>,
    server: String,
    /// Transitional: records without identity map through `-` lines.
    accept_unidentified: bool,
    /// Where the users' agents listen: `DIR/<linux-user>/agent.sock`.
    agents: PathBuf,
    /// §111.10: serve the client role's broker on this socket, in this
    /// process, and publish the BAR files for each session's user.
    broker: Option<PathBuf>,
    gpu_region: Option<PathBuf>,
    hub_wl: Option<PathBuf>,
    hub_sv: Option<PathBuf>,
    /// §108 through §111.10: the guest-RAM substrate's flags, for the
    /// embedded component that serves them (`embedded()`).
    guest_ram: wc_broker::host::GuestRam,
    /// Where the sessions it started itself are recorded (default
    /// `$XDG_RUNTIME_DIR/wc-sessiond/live`).
    live: Option<PathBuf>,
}

fn parse(argv: &[String]) -> Result<Args, String> {
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| "/".into());
    let state_home = std::env::var_os("XDG_STATE_HOME").map(PathBuf::from).unwrap_or_else(|| home.join(".local/state"));
    let mut a = Args {
        arena: PathBuf::new(),
        installs: home.join(".config/waycompose/installs"),
        state: state_home.join("waycompose/sessiond"),
        bell: None,
        poll: Duration::from_millis(1000),
        users: None,
        server: String::new(),
        accept_unidentified: false,
        agents: "/run/waycompose-agent".into(),
        broker: None,
        gpu_region: None,
        hub_wl: None,
        hub_sv: None,
        guest_ram: Default::default(),
        live: None,
    };
    let mut it = argv.iter();
    while let Some(k) = it.next() {
        let mut v = || it.next().cloned().ok_or(format!("{k} needs a value"));
        match k.as_str() {
            // Any file whose page 0 is an ivshmem-style header: QEMU's ivshmem
            // backing file, or a shared file standing in for one (a Mac's
            // RAM-disk file seen through virtio-fs DAX, §103.5's role (b)).
            "--ivshmem" | "--arena-file" => a.arena = v()?.into(),
            "--installs" => a.installs = v()?.into(),
            "--state" => a.state = v()?.into(),
            "--bell" => a.bell = Some(v()?),
            "--poll-ms" => a.poll = Duration::from_millis(v()?.parse().map_err(|e| format!("--poll-ms: {e}"))?),
            "--users" => a.users = Some(v()?.into()),
            "--server" => a.server = v()?,
            "--accept-unidentified" => a.accept_unidentified = true,
            "--agents" => a.agents = v()?.into(),
            "--broker" => a.broker = Some(v()?.into()),
            "--gpu-region" => a.gpu_region = Some(v()?.into()),
            "--hub-wl" => a.hub_wl = Some(v()?.into()),
            "--hub-sv" => a.hub_sv = Some(v()?.into()),
            "--live" => a.live = Some(v()?.into()),
            "--guest-ram-boot" => a.guest_ram.boot = Some(v()?.into()),
            "--guest-ram-qemu-pid" | "--guest-ram-qemu-pidfile" | "--guest-ram" => {
                use wc_broker::host::QemuRam;
                if a.guest_ram.qemu.is_some() {
                    return Err("--guest-ram-qemu-pid, --guest-ram-qemu-pidfile and --guest-ram: one of them".into());
                }
                let val = v()?;
                a.guest_ram.qemu = Some(match k.as_str() {
                    "--guest-ram-qemu-pid" => QemuRam::Pid(val.parse().map_err(|_| format!("{k}: a pid, not {val:?}"))?),
                    "--guest-ram-qemu-pidfile" => QemuRam::PidFile(val.into()),
                    _ => QemuRam::File(val.into()),
                });
            }
            "--guest-ram-bell" => a.guest_ram.bell = Some(v()?),
            "--guest-ram-user" => {
                use wc_broker::host::GuestRamUser;
                let val = v()?;
                a.guest_ram.user = Some(match val.as_str() {
                    "mapped" => GuestRamUser::Mapped,
                    n => GuestRamUser::Uid(n.parse().map_err(|_| format!("{k}: a uid or `mapped`, not {n:?}"))?),
                });
            }
            _ => return Err(format!("unknown argument {k}")),
        }
    }
    if a.guest_ram.boot.is_none() {
        a.guest_ram.boot = a.gpu_region.clone();
    }
    if a.guest_ram.user == Some(wc_broker::host::GuestRamUser::Mapped) && a.users.is_none() {
        return Err("--guest-ram-user mapped needs --users".into());
    }
    if (a.guest_ram.user.is_some() || a.guest_ram.bell.is_some()) && a.guest_ram.qemu.is_none() {
        return Err("the --guest-ram-* flags need one of --guest-ram-qemu-pid, --guest-ram-qemu-pidfile, --guest-ram".into());
    }
    if a.arena.as_os_str().is_empty() {
        return Err("usage: wc-arena-host sessiond --ivshmem ARENA-FILE [--installs DIR] [--state DIR] \
                    [--bell HUB-SOCKET] [--poll-ms N] [--users USERS.CONF --server NAME [--accept-unidentified] \
                    [--agents DIR]] [--broker SOCK [--gpu-region FILE] [--hub-wl SOCK] [--hub-sv SOCK]]"
            .into());
    }
    if a.users.is_some() && a.server.is_empty() {
        return Err("--users needs --server: the name of the VM this service serves, as users.conf spells it".into());
    }
    if a.accept_unidentified && a.users.is_none() {
        return Err("--accept-unidentified is a users.conf setting; it needs --users".into());
    }
    Ok(a)
}

fn say(msg: &str) {
    println!("wc-sessiond: {msg}");
}

/// One installation, as the service knows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Install {
    pub name: String,
    pub base: PathBuf,
}

impl Install {
    pub fn session(&self) -> PathBuf {
        self.base.join("wc-session")
    }
}

/// `DIR/NAME`, resolved, if `NAME` is a valid installation name with a
/// `wc-session` in it.
pub fn lookup(dir: &Path, name: &str) -> Result<Install, String> {
    if !macboot::valid_installation(name) {
        return Err(format!("{name:?} is not an installation name"));
    }
    let base = std::fs::canonicalize(dir.join(name)).map_err(|_| format!("no installation {name} in {}", dir.display()))?;
    if !base.join("wc-session").is_file() {
        return Err(format!("installation {name} ({}) has no wc-session", base.display()));
    }
    Ok(Install { name: name.to_string(), base })
}

/// The installation's key: `base/session.key`, which must be this user's and
/// unreadable by anyone else.
pub fn key_of(i: &Install) -> Result<[u8; macboot::KEY_LEN], String> {
    let p = i.base.join("session.key");
    let md = std::fs::metadata(&p).map_err(|e| format!("installation {}: {}: {e}", i.name, p.display()))?;
    // SAFETY: geteuid has no preconditions.
    let me = unsafe { libc::geteuid() };
    if md.uid() != me || md.permissions().mode() & 0o077 != 0 {
        return Err(format!("installation {}: {} must be owned by uid {me} and mode 0600", i.name, p.display()));
    }
    let text = std::fs::read_to_string(&p).map_err(|e| format!("{}: {e}", p.display()))?;
    macboot::parse_key(&text).ok_or_else(|| format!("{}: not 64 hex digits", p.display()))
}

/// The last sequence number accepted for an installation.
fn last_seq(state: &Path, name: &str) -> u64 {
    std::fs::read_to_string(state.join(format!("{name}.seq"))).ok().and_then(|s| s.trim().parse().ok()).unwrap_or(0)
}

fn store_seq(state: &Path, name: &str, seq: u64) -> std::io::Result<()> {
    std::fs::create_dir_all(state)?;
    let tmp = state.join(format!("{name}.seq.tmp"));
    std::fs::write(&tmp, format!("{seq}\n"))?;
    std::fs::rename(tmp, state.join(format!("{name}.seq")))
}

/// What a page says, checked as untrusted input. `Ok(None)`: nothing to do
/// (no header, no record, already answered, or the record is not this
/// header's). `Err`: a record to refuse, with the reason.
#[derive(Debug, PartialEq, Eq)]
pub struct Request {
    pub rec: macboot::Record,
    pub install: Install,
    pub seq: u64,
    /// §107: the session's index in the guest's shared compositor; 0 for a
    /// compositor serving one session.
    pub index: u8,
    /// §111.6: who asked, as the server's privileged service signed it.
    /// `None`: a record without an identity area (§107's).
    pub identity: Option<macboot::Identity>,
}

/// §111.10: the Linux user a verified record's session runs as, or why it
/// runs as nobody. `map` is users.conf, `server` this service's VM.
pub fn map_user(
    map: &wc_broker::usermap::UserMap,
    server: &str,
    accept_unidentified: bool,
    req: &Request,
) -> Result<String, String> {
    match &req.identity {
        Some(id) => {
            let asker = wc_broker::usermap::Asker { os: &id.os, user: &id.user, id: &id.id };
            map.resolve(server, asker, &req.install.name).map(|m| m.linux_user.clone())
        }
        None if !accept_unidentified => Err(format!(
            "the boot record of installation {} carries no identity, and this host requires one \
             (the guest's waycompose service signs it)",
            req.install.name
        )),
        None => map.resolve_unidentified(server, &req.install.name).map(|m| m.linux_user.clone()).ok_or_else(|| {
            format!(
                "the boot record of installation {} carries no identity, and users.conf of {server} maps no \
                 user for that installation without one",
                req.install.name
            )
        }),
    }
}

pub fn check(page: &[u8], installs: &Path, state: &Path) -> Result<Option<Request>, String> {
    let Ok(h) = wc_handle::ivshmem::Header::decode(page) else { return Ok(None) };
    let Some(rec) = macboot::Record::read(page) else { return Ok(None) };
    if macboot::state(page) != macboot::State::Waiting {
        return Ok(None);
    }
    if rec.arena_run != h.run_id || rec.rings_run != h.run_id {
        // A record left by a previous compositor whose header has been replaced.
        return Ok(None);
    }
    let auth = macboot::Auth::read(page).ok_or("the boot record has no authenticator (deploy gives every installation a key)")?;
    let index = macboot::session_index(page).ok_or("the boot record's session index is not 0..255")?;
    let install = lookup(installs, &auth.installation)?;
    let key = key_of(&install)?;
    // §111.6: an identity area that is there is covered by the MAC; one that
    // does not even parse is refused, never read as "no identity".
    let identity = if macboot::has_identity(page) {
        Some(macboot::Identity::read(page).ok_or("the boot record's identity area does not parse")?)
    } else {
        None
    };
    // §107: the index is covered by the MAC, so a record cannot be replayed
    // into another session's place; index 0 is the one-session message.
    if !auth.verifies_identity(&rec, index, identity.as_ref(), &key) {
        return Err(format!(
            "the boot record for installation {} does not verify under its key{}",
            install.name,
            if identity.is_some() { " with the identity it carries" } else { "" }
        ));
    }
    let last = last_seq(state, &install.name);
    if auth.seq <= last {
        return Err(format!(
            "installation {}: sequence number {} is not above {last}, the last one accepted: a replayed record",
            install.name, auth.seq
        ));
    }
    Ok(Some(Request { rec, install, seq: auth.seq, index, identity }))
}

/// Write the answer into page 0, if it still holds the record: the message
/// first, the state word last (the guest reads the state with acquire).
fn answer(file: &std::fs::File, rec: &macboot::Record, st: macboot::State, msg: &str) -> std::io::Result<bool> {
    let mut pg = vec![0u8; 4096];
    file.read_exact_at(&mut pg, 0)?;
    if macboot::Record::read(&pg).as_ref() != Some(rec) {
        return Ok(false);
    }
    file.write_all_at(&macboot::message_bytes(msg), macboot::OFF_MESSAGE as u64)?;
    std::sync::atomic::fence(std::sync::atomic::Ordering::Release);
    file.write_all_at(&st.word().to_le_bytes(), macboot::OFF_STATE as u64)?;
    Ok(true)
}

/// Sessions this service started, kept in a file each so a restarted service
/// still runs their `down` with the right installation's `wc-session`.
struct Live {
    dir: PathBuf,
    map: HashMap<String, PathBuf>,
}

impl Live {
    fn load(dir: PathBuf) -> Live {
        let mut map = HashMap::new();
        if let Ok(rd) = std::fs::read_dir(&dir) {
            for e in rd.flatten() {
                let id = e.file_name().to_string_lossy().into_owned();
                if let (true, Ok(base)) = (macboot::valid_id(&id), std::fs::read_to_string(e.path())) {
                    map.insert(id, PathBuf::from(base.trim()));
                }
            }
        }
        Live { dir, map }
    }

    fn insert(&mut self, id: &str, base: &Path) {
        let _ = std::fs::create_dir_all(&self.dir);
        let _ = std::fs::write(self.dir.join(id), base.to_string_lossy().as_bytes());
        self.map.insert(id.to_string(), base.to_path_buf());
    }

    /// End every session whose proxy unit is gone.
    fn reap(&mut self, host: &Host) {
        let gone: Vec<(String, PathBuf)> =
            self.map.iter().filter(|(id, _)| !crate::sessiond::unit_active(id)).map(|(i, b)| (i.clone(), b.clone())).collect();
        for (id, base) in gone {
            match crate::sessiond::verb(&base.join("wc-session"), &["down", &id]) {
                Ok(r) => say(&format!("session {id} ended: {r}")),
                Err(e) => say(&format!("session {id} ended; {e}")),
            }
            host.ended(&id);
            let _ = std::fs::remove_file(self.dir.join(&id));
            self.map.remove(&id);
        }
    }
}

/// §111.10: what users.conf says, for a service that maps its sessions.
struct Mapped {
    map: wc_broker::usermap::UserMap,
    server: String,
    accept_unidentified: bool,
    agents: PathBuf,
}

/// §111.10: the client role's broker, in this process. It alone opens the
/// BAR files and the hub's sockets; a session's processes get them from it.
struct Brokered {
    broker: wc_broker::server::Broker<wc_broker::sys::unix::Unix>,
    path: String,
    arena: PathBuf,
    gpu: Option<PathBuf>,
}

impl Brokered {
    fn start(a: &Args, path: &Path, users: Vec<wc_broker::User>) -> std::io::Result<Brokered> {
        use wc_broker::sys::unix::{DevicePath, Unix};
        use wc_broker::{Allow, DeviceSpec};
        let mut services = Vec::new();
        let mut specs = Vec::new();
        for (name, p) in [("wl-hub", &a.hub_wl), ("sv-hub", &a.hub_sv)] {
            if let Some(p) = p {
                services.push(DevicePath { name: name.into(), path: p.clone() });
                specs.push(DeviceSpec { name: name.into(), allow: Allow::Users(users.clone()) });
            }
        }
        let unix = Unix::bind(path, Vec::new())?.with_services(services);
        // The system broker is the only cross-user sandbox entry. A forwarded
        // socket retains its creator's credentials; Connect uses `users` above.
        #[cfg(target_os = "linux")]
        {
            let services = Path::new("/run/sv-drm/services.sock");
            if services.exists() {
                unix.register_render_node(services)?;
            }
        }
        let run = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos() as u64);
        let config = wc_broker::server::Config {
            devices: Vec::new(),
            services: specs,
            keys: None,
            release: wc_broker::Release::OwnerFree,
            budget: None,
            run,
        };
        let broker = wc_broker::server::Broker::new(unix, config);
        let b = broker.clone();
        std::thread::Builder::new().name("broker".into()).spawn(move || b.serve())?;
        Ok(Brokered { broker, path: path.to_string_lossy().into_owned(), arena: a.arena.clone(), gpu: a.gpu_region.clone() })
    }

    /// The BAR files, published for one session's user: (arena, gpu).
    fn publish(&self, uid: u32) -> Result<(u64, Option<u64>), String> {
        let open = |p: &Path, pool: &str| -> Result<u64, String> {
            let f = std::fs::OpenOptions::new().read(true).write(true).open(p).map_err(|e| format!("{}: {e}", p.display()))?;
            let len = f.metadata().map_err(|e| e.to_string())?.len();
            let obj = wc_broker::sys::unix::Unix::object(std::os::fd::OwnedFd::from(f));
            Ok(self.broker.publish(wc_broker::User::uid(uid), pool, 1, vec![(obj, 0, len)]))
        };
        let arena = open(&self.arena, "arena")?;
        let gpu = match &self.gpu {
            None => None,
            Some(g) => match open(g, "gpu") {
                Ok(id) => Some(id),
                Err(e) => {
                    say(&format!("NO GPU region for uid {uid}: {e}"));
                    None
                }
            },
        };
        Ok((arena, gpu))
    }
}

/// §111.10: the components this daemon hosts beside its broker
/// (`wc_broker::host::Embedded`). The guest-RAM substrate (§108,
/// `sv_ram::brokered`) is added here, built from `guest_ram` (the
/// `--guest-ram-*` flags, with users.conf's map); each is attached once at
/// start and told of every session that starts and ends, with its user.
fn embedded(guest_ram: wc_broker::host::GuestRam, state: &Path) -> Result<Vec<Box<dyn wc_broker::host::Embedded>>, String> {
    let mut out: Vec<Box<dyn wc_broker::host::Embedded>> = Vec::new();
    if let Some(c) = sv_ram::brokered::GuestRamComponent::new(guest_ram, Some(state.join("guest-ram")))? {
        out.push(Box::new(c));
    }
    Ok(out)
}

/// Everything a pass of [`serve`] needs besides the page.
struct Host {
    mapped: Option<Mapped>,
    broker: Option<Brokered>,
    /// Regions published per session, unpublished when it ends.
    published: std::sync::Mutex<HashMap<String, Vec<u64>>>,
    /// The live sessions and their owners, for the embedded components.
    sessions: wc_broker::host::SessionsView,
    components: Vec<Box<dyn wc_broker::host::Embedded>>,
}

impl Host {
    fn started(&self, s: wc_broker::host::HostSession) {
        self.sessions.insert(s.clone());
        for c in &self.components {
            c.session_up(&s);
        }
    }

    fn ended(&self, id: &str) {
        let ids = self.published.lock().unwrap_or_else(|e| e.into_inner()).remove(id).unwrap_or_default();
        if let Some(b) = &self.broker {
            for r in ids {
                b.broker.unpublish(r);
            }
        }
        if let Some(s) = self.sessions.remove(id) {
            for c in &self.components {
                c.session_down(&s);
            }
        }
    }
}

pub fn main(argv: &[String]) -> std::process::ExitCode {
    let args = match parse(argv) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("wc-arena-host sessiond: {e}");
            return std::process::ExitCode::from(2);
        }
    };
    // SAFETY: geteuid has no preconditions.
    let me = unsafe { libc::geteuid() };
    if me == 0 {
        eprintln!("wc-arena-host sessiond runs as the user whose sessions it starts, or as its own system account; refusing to run as root");
        return std::process::ExitCode::from(2);
    }
    // §111.10: the mapping, read once; a changed table is a restart, and a
    // table that does not parse is no service at all.
    let mapped = match &args.users {
        None => None,
        Some(p) => match wc_broker::usermap::UserMap::load(p) {
            Ok(map) => Some(Mapped {
                map,
                server: args.server.clone(),
                accept_unidentified: args.accept_unidentified,
                agents: args.agents.clone(),
            }),
            Err(e) => {
                eprintln!("wc-arena-host sessiond: {e}");
                return std::process::ExitCode::from(2);
            }
        },
    };
    let broker = match &args.broker {
        None => None,
        Some(p) => {
            let users: Vec<wc_broker::User> = match &mapped {
                None => vec![wc_broker::User::uid(me)],
                Some(m) => m
                    .map
                    .linux_users(&m.server)
                    .into_iter()
                    .filter_map(|u| match crate::agent::uid_of(u) {
                        Some(uid) => Some(wc_broker::User::uid(uid)),
                        None => {
                            say(&format!("users.conf maps to {u}, which is no account here"));
                            None
                        }
                    })
                    .collect(),
            };
            match Brokered::start(&args, p, users) {
                Ok(b) => Some(b),
                Err(e) => {
                    eprintln!("wc-arena-host sessiond: the broker at {}: {e}", p.display());
                    return std::process::ExitCode::from(1);
                }
            }
        }
    };
    match (&mapped, &broker) {
        (Some(m), _) => say(&format!(
            "sessions of {} run as the users {} maps them to ({} mapping(s){}), through their agents in {}",
            m.server,
            args.users.as_ref().map_or(String::new(), |p| p.display().to_string()),
            m.map.mappings.iter().filter(|x| x.server == m.server).count(),
            if m.accept_unidentified { "; TRANSITIONAL: records without identity accepted by `-` lines" } else { "" },
            m.agents.display()
        )),
        (None, _) => say(&format!("sessions run as this service's own user, uid {me} (no --users)")),
    }
    match &broker {
        Some(b) => say(&format!("broker on {}: the BAR files and the hub are reached through it", b.path)),
        None => say("NO BROKER: each session's processes open the BAR files and the hub's sockets by path"),
    }
    // Every `wc-session` this service runs is in service mode: no ssh tunnel,
    // no upstream socket, the control and sound on rings.
    // SAFETY: single-threaded so far but for the broker's threads, which
    // never read the environment.
    unsafe { std::env::set_var("WC_LAUNCH_SERVICE", "1") };
    let sessions = wc_broker::host::SessionsView::default();
    let mut guest_ram = args.guest_ram.clone();
    if let Some(m) = &mapped {
        let (map, server) = (m.map.clone(), m.server.clone());
        guest_ram.map_user = Some(std::sync::Arc::new(move |os: &str, user: &str, id: &str| {
            let asker = wc_broker::usermap::Asker { os, user, id };
            let u = map.resolve_any(&server, asker).map(|m| m.linux_user.clone())?;
            match crate::agent::uid_of(&u) {
                Some(0) | None => Err(format!("users.conf maps {os} user {user} to {u}, which is not an account here")),
                Some(uid) => Ok(uid),
            }
        }));
    }
    let mut components = match embedded(guest_ram, &args.state) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("wc-arena-host sessiond: {e}");
            return std::process::ExitCode::from(2);
        }
    };
    match &broker {
        Some(b) => {
            for c in &components {
                say(&format!("attaching {} to the broker", c.name()));
                c.attach(&b.broker, &sessions);
            }
        }
        None if !components.is_empty() => {
            say("NO BROKER: the embedded components are not attached");
            components.clear();
        }
        None => {}
    }
    let host = std::sync::Arc::new(Host { mapped, broker, published: Default::default(), sessions, components });
    let (tx, rx) = mpsc::channel();
    if let Some(b) = &args.bell {
        crate::sessiond::spawn_bell(b.clone(), tx.clone());
    } else {
        say("NO DOORBELL HUB given: the arena's header page is read every poll interval only");
    }
    // The sessions this service started itself (not through an agent), so a
    // restarted service still ends them. `--live`: a harness keeps its own.
    let live_dir = args.live.clone().unwrap_or_else(|| {
        let runtime = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from).unwrap_or_else(std::env::temp_dir);
        runtime.join("wc-sessiond/live")
    });
    let mut live = Live::load(live_dir);
    if !live.map.is_empty() {
        say(&format!("adopted running session(s): {:?}", live.map.keys().collect::<Vec<_>>()));
    }
    say(&format!(
        "serving {} for the installations in {}; on hello, on reconnect and every {:?}",
        args.arena.display(),
        args.installs.display(),
        args.poll
    ));
    // Records answered or refused by this process, so a page the guest has not
    // yet rewritten is never served twice and a refusal is said once. §107:
    // by run id, session id and sequence number, not by run id alone, because
    // every session of a shared compositor has its compositor's run id.
    let mut done: HashSet<Seen> = HashSet::new();
    // Ended sessions are looked for every two seconds whatever the poll is.
    let tick = args.poll.min(Duration::from_secs(2));
    let mut last_read = Instant::now();
    loop {
        let why = match rx.recv_timeout(tick) {
            Ok(()) => Some("hello"),
            Err(_) if last_read.elapsed() >= args.poll => Some("timer"),
            Err(_) => None,
        };
        while rx.try_recv().is_ok() {}
        if let Some(why) = why {
            last_read = Instant::now();
            serve(&args, &host, &mut live, &mut done, why);
        }
        live.reap(&host);
    }
}

/// Read page 0 twice, a moment apart, and return it only if both reads agree:
/// a guest writing the page as it is read gives two different pages, and the
/// next pass sees the finished one.
fn stable_page(file: &std::fs::File) -> std::io::Result<Option<Vec<u8>>> {
    let mut a = vec![0u8; 4096];
    let mut b = vec![0u8; 4096];
    file.read_exact_at(&mut a, 0)?;
    if macboot::Record::read(&a).is_none() {
        return Ok(None);
    }
    std::thread::sleep(Duration::from_millis(2));
    file.read_exact_at(&mut b, 0)?;
    Ok((a == b).then_some(a))
}

/// §107: which record a page held: run id, session id, sequence number.
type Seen = (u64, String, u64);

fn seen(rec: &macboot::Record, page: &[u8]) -> Seen {
    (rec.arena_run, rec.session.clone(), macboot::Auth::read(page).map_or(0, |a| a.seq))
}

fn serve(args: &Args, host: &std::sync::Arc<Host>, live: &mut Live, done: &mut HashSet<Seen>, why: &str) {
    let t0 = Instant::now();
    let file = match std::fs::OpenOptions::new().read(true).write(true).open(&args.arena) {
        Ok(f) => f,
        Err(e) => {
            if why == "hello" {
                say(&format!("cannot open {}: {e}", args.arena.display()));
            }
            return;
        }
    };
    let page = match stable_page(&file) {
        Ok(Some(p)) => p,
        Ok(None) => return,
        Err(e) => {
            say(&format!("{}: {e}", args.arena.display()));
            return;
        }
    };
    let Some(rec) = macboot::Record::read(&page) else { return };
    let key = seen(&rec, &page);
    if done.contains(&key) {
        return;
    }
    let refuse = |e: &str| {
        say(&format!("session {}: REFUSED: {e}", rec.session));
        let _ = answer(&file, &rec, macboot::State::Failed, &format!("refused: {e}"));
    };
    let req = match check(&page, &args.installs, &args.state) {
        Ok(Some(r)) => r,
        Ok(None) => return,
        Err(e) => {
            done.insert(key);
            refuse(&e);
            return;
        }
    };
    done.insert(key);
    // §111.10: whose session it is, before anything is recorded or started.
    // SAFETY: geteuid has no preconditions.
    let me = unsafe { libc::geteuid() };
    let (user, uid) = match &host.mapped {
        None => (None, me),
        Some(m) => {
            let u = match map_user(&m.map, &m.server, m.accept_unidentified, &req) {
                Ok(u) => u,
                Err(e) => return refuse(&e),
            };
            match crate::agent::uid_of(&u) {
                Some(0) | None => return refuse(&format!("users.conf maps this session to {u}, which is not an account a session may run as here")),
                Some(uid) => (Some(u), uid),
            }
        }
    };
    if let Err(e) = store_seq(&args.state, &req.install.name, req.seq) {
        say(&format!("cannot record installation {}'s sequence number: {e}; refusing", req.install.name));
        let _ = answer(&file, &rec, macboot::State::Failed, "the host cannot record the sequence number");
        return;
    }
    let id = rec.session.clone();
    let ws = req.install.session();
    say(&format!(
        "boot record for session {id} of installation {} ({why}): run 0x{:016x}, {} pages, gpu {}, session index {}, {}{}",
        req.install.name,
        rec.arena_run,
        rec.arena_pages,
        rec.gpu,
        req.index,
        req.identity.as_ref().map_or_else(|| "no identity".to_string(), |i| format!("asked by {}", i.text())),
        user.as_ref().map_or_else(String::new, |u| format!(", runs as {u} (uid {uid})"))
    ));
    let (arena, gpu_region) = match &host.broker {
        None => (None, None),
        Some(b) => match b.publish(uid) {
            Ok((a, g)) => {
                host.published.lock().unwrap_or_else(|e| e.into_inner()).insert(id.clone(), [Some(a), g].into_iter().flatten().collect());
                (Some(a), g)
            }
            Err(e) => {
                say(&format!("session {id}: FAILED: the broker cannot publish the BAR: {e}"));
                let _ = answer(&file, &rec, macboot::State::Failed, &format!("the host cannot open the shared memory: {e}"));
                return;
            }
        },
    };
    let start = crate::agent::Start {
        install: req.install.name.clone(),
        id: id.clone(),
        run: rec.arena_run,
        index: req.index,
        gpu: rec.gpu,
        arena,
        gpu_region,
    };
    let started = match (&user, &host.mapped) {
        (Some(u), Some(m)) => {
            let sock = m.agents.join(u).join("agent.sock");
            crate::agent::call(&sock, uid, &start).map(|(msg, conn)| {
                // The agent says `ended` when the session's proxy unit has
                // gone and it ran `down`; then its regions go too.
                let (host, id) = (std::sync::Arc::clone(host), id.clone());
                let _ = std::thread::Builder::new().name(format!("agent-{id}")).spawn(move || {
                    let mut conn = conn;
                    let mut line = String::new();
                    let _ = std::io::BufRead::read_line(&mut conn, &mut line);
                    say(&format!("session {id} ended: {}", line.trim_end().strip_prefix("ended ").unwrap_or("the agent's connection closed")));
                    host.ended(&id);
                });
                msg
            })
        }
        _ => {
            let env = crate::agent::session_env(&start, host.broker.as_ref().map(|b| b.path.as_str()));
            crate::agent::start_session(&ws, &start, &env).inspect(|_| live.insert(&id, &req.install.base))
        }
    };
    let msg = match started {
        Ok(m) => m,
        Err(e) => {
            say(&format!("session {id}: FAILED: {e}"));
            host.ended(&id);
            let _ = answer(&file, &rec, macboot::State::Failed, &e);
            return;
        }
    };
    host.started(wc_broker::host::HostSession {
        id: id.clone(),
        install: req.install.name.clone(),
        index: req.index,
        run: rec.arena_run,
        user: wc_broker::User::uid(uid),
        uid,
        linux_user: user.clone(),
    });
    match answer(&file, &rec, macboot::State::Located, &msg) {
        Ok(true) => say(&format!("session {id}: started in {:?} ({why}): {msg}", t0.elapsed())),
        Ok(false) => say(&format!("session {id}: started, but the guest replaced its record before the answer; its lease will end it")),
        Err(e) => say(&format!("session {id}: could not answer: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Tmp(PathBuf);
    impl Drop for Tmp {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn tmp(tag: &str) -> Tmp {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let p = std::env::temp_dir().join(format!("wc-sessiond-test-{tag}-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        Tmp(p)
    }

    fn install(t: &Path, name: &str, key: &[u8; 32]) {
        let base = t.join(format!("base-{name}"));
        std::fs::create_dir_all(&base).unwrap();
        std::fs::write(base.join("wc-session"), "#!/bin/sh\n").unwrap();
        let hex: String = key.iter().map(|b| format!("{b:02x}")).collect();
        let kp = base.join("session.key");
        std::fs::write(&kp, hex).unwrap();
        std::fs::set_permissions(&kp, std::fs::Permissions::from_mode(0o600)).unwrap();
        std::fs::create_dir_all(t.join("installs")).unwrap();
        std::os::unix::fs::symlink(&base, t.join("installs").join(name)).unwrap();
    }

    fn page(run: u64, rec: &macboot::Record, auth: Option<&macboot::Auth>) -> Vec<u8> {
        let h = wc_handle::ivshmem::Header {
            run_id: run,
            generation: 1,
            region_len: 128 << 20,
            arena_id: 0,
            outbound: ((128 << 20) - 4096 - (6 << 20) - (1 << 20), 1 << 20),
            rings: ((128 << 20) - (6 << 20), 6 << 20),
        };
        let mut p = vec![0u8; 4096];
        p[..wc_handle::ivshmem::ENCODED_LEN].copy_from_slice(&h.encode());
        // SAFETY: a 4096-byte buffer this test owns.
        unsafe { macboot::publish(p.as_mut_ptr(), rec, auth) };
        p
    }

    fn rec(run: u64) -> macboot::Record {
        macboot::Record { session: "wcns-1".into(), arena_run: run, arena_pages: 32768, outbound: 1 << 20, rings_run: run, rings_pages: 1536, gpu: false }
    }

    #[test]
    fn a_signed_record_of_a_known_installation_is_taken() {
        let t = tmp("ok");
        let key = [3u8; 32];
        install(&t.0, "wcns", &key);
        let r = rec(0x77);
        let a = macboot::Auth::sign(&r, "wcns", 5, &key);
        let got = check(&page(0x77, &r, Some(&a)), &t.0.join("installs"), &t.0.join("state")).unwrap().unwrap();
        assert_eq!(got.install.name, "wcns");
        assert_eq!(got.seq, 5);
    }

    #[test]
    fn unsigned_wrong_key_unknown_installation_and_replay_are_refused() {
        let t = tmp("refuse");
        let key = [3u8; 32];
        install(&t.0, "wcns", &key);
        let (inst, st) = (t.0.join("installs"), t.0.join("state"));
        let r = rec(0x77);
        assert!(check(&page(0x77, &r, None), &inst, &st).unwrap_err().contains("no authenticator"));
        let bad = macboot::Auth::sign(&r, "wcns", 5, &[4u8; 32]);
        assert!(check(&page(0x77, &r, Some(&bad)), &inst, &st).unwrap_err().contains("does not verify"));
        let other = macboot::Auth::sign(&r, "nosuch", 5, &key);
        assert!(check(&page(0x77, &r, Some(&other)), &inst, &st).unwrap_err().contains("no installation"));
        store_seq(&st, "wcns", 5).unwrap();
        let replay = macboot::Auth::sign(&r, "wcns", 5, &key);
        assert!(check(&page(0x77, &r, Some(&replay)), &inst, &st).unwrap_err().contains("replayed"));
        let fresh = macboot::Auth::sign(&r, "wcns", 6, &key);
        assert!(check(&page(0x77, &r, Some(&fresh)), &inst, &st).unwrap().is_some());
    }

    #[test]
    fn a_record_of_another_run_or_already_answered_is_ignored() {
        let t = tmp("ignore");
        let key = [3u8; 32];
        install(&t.0, "wcns", &key);
        let (inst, st) = (t.0.join("installs"), t.0.join("state"));
        let r = rec(0x77);
        let a = macboot::Auth::sign(&r, "wcns", 9, &key);
        // The header now names run 0x78: the record is the previous compositor's.
        assert_eq!(check(&page(0x78, &r, Some(&a)), &inst, &st), Ok(None));
        let mut p = page(0x77, &r, Some(&a));
        p[macboot::OFF_STATE..macboot::OFF_STATE + 8].copy_from_slice(&macboot::State::Located.word().to_le_bytes());
        assert_eq!(check(&p, &inst, &st), Ok(None));
        // No header at all.
        let mut p = page(0x77, &r, Some(&a));
        p[..8].fill(0);
        assert_eq!(check(&p, &inst, &st), Ok(None));
    }

    /// §111.6, §111.8: a record the guest's broker signed carries the guest
    /// user it was signed for; the service takes it with the record, and a
    /// record whose identity was rewritten in the page (by the user process
    /// that can write it), removed, or made unreadable is refused.
    #[test]
    fn a_broker_signed_record_carries_its_identity_and_a_rewritten_one_is_refused() {
        let t = tmp("identity");
        let k = [5u8; 32];
        install(&t.0, "wcivs", &k);
        let (inst, st) = (t.0.join("installs"), t.0.join("state"));
        let r = rec(0x77);
        let alice = macboot::Identity { os: "windows".into(), user: "alice".into(), id: "S-1-5-21-1-2-3-1001".into() };
        let bob = macboot::Identity { os: "windows".into(), user: "bob".into(), id: "S-1-5-21-1-2-3-1002".into() };
        let a = macboot::Auth::sign_identity(&r, 2, Some(&alice), "wcivs", 20, &k);
        let signed = || {
            let mut p = page(0x77, &r, None);
            // SAFETY: a 4096-byte buffer this test owns.
            unsafe { macboot::publish_identity(p.as_mut_ptr(), &r, 2, Some(&alice), Some(&a)) };
            p
        };
        let got = check(&signed(), &inst, &st).unwrap().unwrap();
        assert_eq!((got.index, got.identity.as_ref()), (2, Some(&alice)));
        let mut p = signed();
        bob.write(&mut p);
        assert!(check(&p, &inst, &st).unwrap_err().contains("does not verify"), "another user's identity written over it");
        let mut p = signed();
        p[macboot::OFF_IDENTITY..macboot::OFF_IDENTITY + 8].fill(0);
        assert!(check(&p, &inst, &st).unwrap_err().contains("does not verify"), "the identity removed");
        let mut p = signed();
        p[macboot::OFF_IDENTITY + 10] = b' ';
        assert!(check(&p, &inst, &st).unwrap_err().contains("does not parse"), "an identity that does not parse");
        // A record signed without a broker has none, and is §107's.
        let b = macboot::Auth::sign_indexed(&r, 2, "wcivs", 21, &k);
        let mut p = page(0x77, &r, None);
        // SAFETY: as above.
        unsafe { macboot::publish_identity(p.as_mut_ptr(), &r, 2, None, Some(&b)) };
        assert_eq!(check(&p, &inst, &st).unwrap().unwrap().identity, None);
    }

    /// §107: a shared compositor's record carries its session's index, which
    /// the MAC covers; the service takes it to the session's proxy, and a
    /// record moved to another index, or signed by another installation's
    /// key, is refused.
    #[test]
    fn a_shared_compositors_record_carries_its_index_under_its_own_key() {
        let t = tmp("indexed");
        let (k1, k2) = ([3u8; 32], [4u8; 32]);
        install(&t.0, "wcivs", &k1);
        install(&t.0, "wclinux", &k2);
        let (inst, st) = (t.0.join("installs"), t.0.join("state"));
        let r = rec(0x77);
        let indexed = |idx: u8, auth: &macboot::Auth| {
            let mut p = page(0x77, &r, None);
            // SAFETY: a 4096-byte buffer this test owns.
            unsafe { macboot::publish_indexed(p.as_mut_ptr(), &r, idx, Some(auth)) };
            p
        };
        let a = macboot::Auth::sign_indexed(&r, 3, "wcivs", 10, &k1);
        let got = check(&indexed(3, &a), &inst, &st).unwrap().unwrap();
        assert_eq!((got.index, got.install.name.as_str()), (3, "wcivs"));
        assert!(check(&indexed(4, &a), &inst, &st).unwrap_err().contains("does not verify"), "moved to index 4");
        let other = macboot::Auth::sign_indexed(&r, 3, "wclinux", 10, &k1);
        assert!(check(&indexed(3, &other), &inst, &st).unwrap_err().contains("does not verify"), "one installation's key for another");
        // Two sessions of one compositor: one run id, two records, two keys.
        let r2 = macboot::Record { session: "wcns-2".into(), ..r.clone() };
        let b = macboot::Auth::sign_indexed(&r2, 4, "wclinux", 11, &k2);
        let mut p = page(0x77, &r2, None);
        // SAFETY: as above.
        unsafe { macboot::publish_indexed(p.as_mut_ptr(), &r2, 4, Some(&b)) };
        assert_eq!(check(&p, &inst, &st).unwrap().unwrap().index, 4);
        assert_ne!(seen(&r, &indexed(3, &a)), seen(&r2, &p), "the service tells them apart");
    }

    #[test]
    fn a_key_others_can_read_is_refused() {
        let t = tmp("perm");
        install(&t.0, "wcns", &[3u8; 32]);
        let i = lookup(&t.0.join("installs"), "wcns").unwrap();
        std::fs::set_permissions(i.base.join("session.key"), std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(key_of(&i).unwrap_err().contains("0600"));
        assert!(lookup(&t.0.join("installs"), "../etc").is_err());
    }

    #[test]
    fn the_answer_is_written_only_into_the_same_record() {
        let t = tmp("answer");
        let r = rec(0x77);
        let p = page(0x77, &r, None);
        let f = t.0.join("arena");
        std::fs::write(&f, &p).unwrap();
        let file = std::fs::OpenOptions::new().read(true).write(true).open(&f).unwrap();
        assert!(answer(&file, &r, macboot::State::Failed, "no").unwrap());
        let mut back = vec![0u8; 4096];
        file.read_exact_at(&mut back, 0).unwrap();
        assert_eq!(macboot::state(&back), macboot::State::Failed);
        assert_eq!(macboot::message(&back), "no");
        let mut other = r.clone();
        other.session = "wcns-2".into();
        assert!(!answer(&file, &other, macboot::State::Located, "yes").unwrap());
    }

    fn identified(rec: &macboot::Record, index: u8, id: Option<&macboot::Identity>, auth: &macboot::Auth) -> Vec<u8> {
        let mut p = page(rec.arena_run, rec, None);
        // SAFETY: a 4096-byte buffer this test owns.
        unsafe { macboot::publish_identity(p.as_mut_ptr(), rec, index, id, Some(auth)) };
        p
    }

    fn win(user: &str) -> macboot::Identity {
        macboot::Identity { os: "windows".into(), user: user.into(), id: format!("S-1-5-21-1-{user}") }
    }

    /// §111.6 on the host: an identity is verified with the record, a
    /// rewritten or unreadable one is refused, and none is §107's record.
    #[test]
    fn the_identity_is_verified_with_the_record() {
        let t = tmp("identity");
        let key = [3u8; 32];
        install(&t.0, "wcivs", &key);
        let (inst, st) = (t.0.join("installs"), t.0.join("state"));
        let r = rec(0x77);
        let (guest, other) = (win("guest"), win("mallory"));
        let a = macboot::Auth::sign_identity(&r, 2, Some(&guest), "wcivs", 10, &key);
        let got = check(&identified(&r, 2, Some(&guest), &a), &inst, &st).unwrap().unwrap();
        assert_eq!(got.identity, Some(guest.clone()));
        // Another user's name written over the signed one.
        let mut p = identified(&r, 2, Some(&guest), &a);
        other.write(&mut p);
        assert!(check(&p, &inst, &st).unwrap_err().contains("with the identity it carries"));
        // The identity area cleared: the MAC was over an identity.
        let mut p = identified(&r, 2, Some(&guest), &a);
        p[macboot::OFF_IDENTITY..macboot::OFF_IDENTITY + 8].fill(0);
        assert!(check(&p, &inst, &st).unwrap_err().contains("does not verify"));
        // An identity area whose text does not parse.
        let mut p = identified(&r, 2, Some(&guest), &a);
        p[macboot::OFF_IDENTITY + 10..macboot::OFF_IDENTITY + 12].copy_from_slice(b"xx");
        assert!(check(&p, &inst, &st).unwrap_err().contains("does not parse"));
        // §107's record, no identity: verifies, with none.
        let b = macboot::Auth::sign_indexed(&r, 2, "wcivs", 11, &key);
        assert_eq!(check(&identified(&r, 2, None, &b), &inst, &st).unwrap().unwrap().identity, None);
    }

    /// §111.10: the mapping, and each refusal the guest is told.
    #[test]
    fn a_verified_record_maps_to_its_linux_user_or_is_refused() {
        let t = tmp("map");
        let key = [3u8; 32];
        install(&t.0, "wcivs", &key);
        install(&t.0, "wcmc", &key);
        let (inst, st) = (t.0.join("installs"), t.0.join("state"));
        let map = wc_broker::usermap::UserMap::parse(
            "vm guest desk installs=wcivs\nvm guest wctest installs=wcmc\nvm - desk installs=wcivs\n",
        )
        .unwrap();
        let r = rec(0x77);
        let req = |install: &str, id: Option<&macboot::Identity>| {
            let a = macboot::Auth::sign_identity(&r, 1, id, install, 20, &key);
            check(&identified(&r, 1, id, &a), &inst, &st).unwrap().unwrap()
        };
        let guest = win("guest");
        assert_eq!(map_user(&map, "vm", false, &req("wcivs", Some(&guest))).unwrap(), "desk");
        assert_eq!(map_user(&map, "vm", false, &req("wcmc", Some(&guest))).unwrap(), "wctest");
        let e = map_user(&map, "vm", false, &req("wcivs", Some(&win("mallory")))).unwrap_err();
        assert!(e.contains("is not mapped to any Linux user"), "{e}");
        let e = map_user(&map, "vm", false, &req("wcivs", None)).unwrap_err();
        assert!(e.contains("carries no identity, and this host requires one"), "{e}");
        assert_eq!(map_user(&map, "vm", true, &req("wcivs", None)).unwrap(), "desk", "transitional");
        let e = map_user(&map, "vm", true, &req("wcmc", None)).unwrap_err();
        assert!(e.contains("maps no user for that installation without one"), "{e}");
    }
}
