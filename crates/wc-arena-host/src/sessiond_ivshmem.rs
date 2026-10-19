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
    };
    let mut it = argv.iter();
    while let Some(k) = it.next() {
        let mut v = || it.next().cloned().ok_or(format!("{k} needs a value"));
        match k.as_str() {
            "--ivshmem" => a.arena = v()?.into(),
            "--installs" => a.installs = v()?.into(),
            "--state" => a.state = v()?.into(),
            "--bell" => a.bell = Some(v()?),
            "--poll-ms" => a.poll = Duration::from_millis(v()?.parse().map_err(|e| format!("--poll-ms: {e}"))?),
            _ => return Err(format!("unknown argument {k}")),
        }
    }
    if a.arena.as_os_str().is_empty() {
        return Err("usage: wc-arena-host sessiond --ivshmem ARENA-FILE [--installs DIR] [--state DIR] \
                    [--bell HUB-SOCKET] [--poll-ms N]"
            .into());
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
    let install = lookup(installs, &auth.installation)?;
    let key = key_of(&install)?;
    if !auth.verifies(&rec, &key) {
        return Err(format!("the boot record for installation {} does not verify under its key", install.name));
    }
    let last = last_seq(state, &install.name);
    if auth.seq <= last {
        return Err(format!(
            "installation {}: sequence number {} is not above {last}, the last one accepted: a replayed record",
            install.name, auth.seq
        ));
    }
    Ok(Some(Request { rec, install, seq: auth.seq }))
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
    fn reap(&mut self) {
        let gone: Vec<(String, PathBuf)> =
            self.map.iter().filter(|(id, _)| !crate::sessiond::unit_active(id)).map(|(i, b)| (i.clone(), b.clone())).collect();
        for (id, base) in gone {
            match crate::sessiond::verb(&base.join("wc-session"), &["down", &id]) {
                Ok(r) => say(&format!("session {id} ended: {r}")),
                Err(e) => say(&format!("session {id} ended; {e}")),
            }
            let _ = std::fs::remove_file(self.dir.join(&id));
            self.map.remove(&id);
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
    if unsafe { libc::geteuid() } == 0 {
        eprintln!("wc-arena-host sessiond runs as the user whose sessions it starts; refusing to run as root");
        return std::process::ExitCode::from(2);
    }
    // Every `wc-session` this service runs is in service mode: no ssh tunnel,
    // no upstream socket, the control and sound on rings.
    // SAFETY: single-threaded here, before any thread is spawned.
    unsafe { std::env::set_var("WC_LAUNCH_SERVICE", "1") };
    let (tx, rx) = mpsc::channel();
    if let Some(b) = &args.bell {
        crate::sessiond::spawn_bell(b.clone(), tx.clone());
    } else {
        say("NO DOORBELL HUB given: the arena's header page is read every poll interval only");
    }
    let runtime = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from).unwrap_or_else(|| std::env::temp_dir());
    let mut live = Live::load(runtime.join("wc-sessiond/live"));
    if !live.map.is_empty() {
        say(&format!("adopted running session(s): {:?}", live.map.keys().collect::<Vec<_>>()));
    }
    say(&format!(
        "serving {} for the installations in {}; on hello, on reconnect and every {:?}",
        args.arena.display(),
        args.installs.display(),
        args.poll
    ));
    // Records answered or refused by this process, by run id, so a page the
    // guest has not yet rewritten is never served twice and a refusal is said
    // once.
    let mut done: HashSet<u64> = HashSet::new();
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
            serve(&args, &mut live, &mut done, why);
        }
        live.reap();
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

fn serve(args: &Args, live: &mut Live, done: &mut HashSet<u64>, why: &str) {
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
    if done.contains(&rec.arena_run) {
        return;
    }
    let req = match check(&page, &args.installs, &args.state) {
        Ok(Some(r)) => r,
        Ok(None) => return,
        Err(e) => {
            done.insert(rec.arena_run);
            say(&format!("session {}: REFUSED: {e}", rec.session));
            let _ = answer(&file, &rec, macboot::State::Failed, &format!("refused: {e}"));
            return;
        }
    };
    done.insert(rec.arena_run);
    if let Err(e) = store_seq(&args.state, &req.install.name, req.seq) {
        say(&format!("cannot record installation {}'s sequence number: {e}; refusing", req.install.name));
        let _ = answer(&file, &rec, macboot::State::Failed, "the host cannot record the sequence number");
        return;
    }
    let id = rec.session.clone();
    let ws = req.install.session();
    say(&format!(
        "boot record for session {id} of installation {} ({why}): run 0x{:016x}, {} pages, gpu {}",
        req.install.name, rec.arena_run, rec.arena_pages, rec.gpu
    ));
    let run = format!("0x{:016x}", rec.arena_run);
    let up = crate::sessiond::verb(&ws, &["up", &id, &run, "ivshmem"]);
    let started = up.and_then(|u| {
        let p = crate::sessiond::verb(&ws, &["proxy", &id])?;
        Ok(format!("{u}; {p}"))
    });
    let text = match started {
        Ok(t) => t,
        Err(e) => {
            say(&format!("session {id}: FAILED: {e}"));
            let _ = answer(&file, &rec, macboot::State::Failed, &e);
            let _ = crate::sessiond::verb(&ws, &["down", &id]);
            return;
        }
    };
    live.insert(&id, &req.install.base);
    let mut msg = text;
    if rec.gpu {
        match crate::sessiond::verb(&ws, &["gpu", &id]) {
            Ok(g) => msg = format!("{msg}; {g}"),
            // Not fatal, as on every path: the application renders in software.
            Err(e) => msg = format!("{msg}; NO GPU: {e}"),
        }
    }
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
        let p = std::env::temp_dir().join(format!("wc-sessiond-test-{tag}-{}", std::process::id()));
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
}
