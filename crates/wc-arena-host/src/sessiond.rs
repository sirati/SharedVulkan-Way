//! `wc-arena-host mac-sessiond` (waycompose-design.md §103.15): the host's
//! session service for a macOS guest, which starts and ends its sessions with
//! no ssh and no socket into the guest.
//!
//! The guest's main process stamps two windows of its RAM (the arena and the
//! ring region) and writes a boot record into page 0 of the ring window
//! (`wc_handle::macboot`). Then it rings the hello token on the rings'
//! doorbell, once a second until answered. This service, a user unit beside
//! the VM:
//!
//! 1. on a hello (or, with no doorbell, every `--sweep-secs`), sweeps QEMU's
//!    memory backend for a stamp with page index 0 and a boot record whose
//!    state is still `Waiting`;
//! 2. for each new one runs `wc-session up`, `rings` (the two locates) and,
//!    after answering, `proxy`, exactly the verbs the forced-command key ran,
//!    with this unit's environment;
//! 3. answers in that page: the message, then the state word, `Located` or
//!    `Failed`. The guest then writes its control region over the stamps;
//! 4. runs `wc-session down` once a session's proxy unit has ended, which it
//!    does by itself when the guest's lease stops (§103.14) or the guest says
//!    `down` on the control ring.
//!
//! What the guest controls is the record's fields, and they are checked as
//! untrusted input here (`macboot::Record::read`) and again by `wc-session`.

use std::collections::{HashMap, HashSet};
use std::io::Read;
use std::os::unix::fs::FileExt;
use std::path::PathBuf;
use std::process::Command;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use wc_handle::macboot;

struct Args {
    qmp: PathBuf,
    vm: String,
    session: PathBuf,
    bell: Option<String>,
    sweep: Duration,
}

fn parse(argv: &[String]) -> Result<Args, String> {
    let mut a = Args { qmp: PathBuf::new(), vm: String::new(), session: PathBuf::new(), bell: None, sweep: Duration::from_secs(5) };
    let mut it = argv.iter();
    while let Some(k) = it.next() {
        let mut v = || it.next().cloned().ok_or(format!("{k} needs a value"));
        match k.as_str() {
            "--qmp" => a.qmp = v()?.into(),
            "--vm" => a.vm = v()?,
            "--session" => a.session = v()?.into(),
            "--bell" => a.bell = Some(v()?),
            "--sweep-secs" => a.sweep = Duration::from_secs(v()?.parse().map_err(|e| format!("--sweep-secs: {e}"))?),
            _ => return Err(format!("unknown argument {k}")),
        }
    }
    if a.qmp.as_os_str().is_empty() || a.vm.is_empty() || a.session.as_os_str().is_empty() {
        return Err("usage: wc-arena-host mac-sessiond --qmp SOCK --vm HINT --session WC-SESSION \
                    [--bell HUB-SOCKET] [--sweep-secs N]"
            .into());
    }
    Ok(a)
}

fn say(msg: &str) {
    println!("wc-mac-sessiond: {msg}");
}

/// A boot record found by a sweep: where page 0 is in the backing file.
struct Found {
    rec: macboot::Record,
    offset: u64,
}

/// Sweep the backend for page-0 stamps with a waiting boot record. Reads the
/// whole file in 32 MiB chunks, which is the arena sweep's cost (§103.11).
fn sweep(file: &std::fs::File, len: u64) -> std::io::Result<Vec<Found>> {
    const CHUNK: usize = 32 << 20;
    let page = wc_handle::arena::REGION_PAGE;
    let mut buf = vec![0u8; CHUNK];
    let mut out = Vec::new();
    let mut base = 0u64;
    while base < len {
        let want = CHUNK.min((len - base) as usize);
        let got = sv_hostd::qemu::read_at(file, base, &mut buf[..want])?;
        for p in (0..got.saturating_sub(page - 1)).step_by(page) {
            let pg = &buf[p..p + page];
            if pg[..8] != wc_handle::arena::STAMP_MAGIC {
                continue;
            }
            let idx = wc_handle::arena::STAMP_OFF_PAGE_INDEX;
            if u32::from_le_bytes(pg[idx..idx + 4].try_into().unwrap_or([0xff; 4])) != 0 {
                continue;
            }
            let Some(rec) = macboot::Record::read(pg) else { continue };
            let run_at = wc_handle::arena::STAMP_OFF_RUN_ID;
            if u64::from_le_bytes(pg[run_at..run_at + 8].try_into().unwrap_or([0; 8])) != rec.rings_run {
                continue;
            }
            if macboot::state(pg) == macboot::State::Waiting {
                out.push(Found { rec, offset: base + p as u64 });
            }
        }
        if got == 0 {
            break;
        }
        base += got as u64;
    }
    Ok(out)
}

/// Write the answer into page 0, if it still holds the record: the message
/// first, the state word last (the guest reads the state with acquire).
fn answer(file: &std::fs::File, f: &Found, state: macboot::State, msg: &str) -> std::io::Result<bool> {
    let mut pg = vec![0u8; wc_handle::arena::REGION_PAGE];
    sv_hostd::qemu::read_at(file, f.offset, &mut pg)?;
    if macboot::Record::read(&pg).as_ref() != Some(&f.rec) {
        return Ok(false);
    }
    file.write_all_at(&macboot::message_bytes(msg), f.offset + macboot::OFF_MESSAGE as u64)?;
    std::sync::atomic::fence(std::sync::atomic::Ordering::Release);
    file.write_all_at(&state.word().to_le_bytes(), f.offset + macboot::OFF_STATE as u64)?;
    Ok(true)
}

/// One `wc-session` verb, with this unit's environment. The words go in
/// `SSH_ORIGINAL_COMMAND`, as the forced-command key sent them, so they meet
/// the validation `wc-session` gives a guest's request (§94): some of them
/// came out of guest RAM.
fn verb(session: &PathBuf, words: &[&str]) -> Result<String, String> {
    let out = Command::new("bash")
        .arg(session)
        .env("SSH_ORIGINAL_COMMAND", words.join(" "))
        .output()
        .map_err(|e| format!("bash {}: {e}", session.display()))?;
    let text = String::from_utf8_lossy(&out.stdout).trim().replace('\n', "; ");
    if out.status.success() {
        Ok(text)
    } else {
        let err = String::from_utf8_lossy(&out.stderr);
        let err: Vec<&str> = err.lines().filter(|l| !l.trim().is_empty()).collect();
        Err(format!("{} failed: {}", words[0], err.iter().rev().take(4).rev().copied().collect::<Vec<_>>().join(" / ")))
    }
}

fn unit_active(id: &str) -> bool {
    Command::new("systemctl")
        .args(["--user", "is-active", "--quiet", &format!("wc-launch-{id}")])
        .status()
        .is_ok_and(|s| s.success())
}

/// Sessions whose proxy units are running now: a restarted service adopts
/// them, so it still runs their `down`.
fn running_sessions() -> Vec<String> {
    let out = Command::new("systemctl")
        .args(["--user", "list-units", "--plain", "--no-legend", "--state=active", "wc-launch-*"])
        .output();
    let Ok(out) = out else { return Vec::new() };
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| l.split_whitespace().next())
        .filter_map(|u| u.strip_prefix("wc-launch-")?.strip_suffix(".service"))
        .filter(|id| !id.starts_with("audio-") && !id.starts_with("gpu-") && macboot::valid_id(id))
        .map(str::to_string)
        .collect()
}

/// The hello reader: a thread on the doorbell hub, reconnecting, which sends
/// one message per hello byte.
fn spawn_bell(spec: String, tx: mpsc::Sender<()>) {
    std::thread::Builder::new()
        .name("bell".into())
        .spawn(move || {
            let mut said = false;
            loop {
                match std::os::unix::net::UnixStream::connect(&spec) {
                    Ok(mut s) => {
                        say(&format!("listening for hellos on {spec}"));
                        said = false;
                        let mut buf = [0u8; 256];
                        loop {
                            match s.read(&mut buf) {
                                Ok(0) | Err(_) => break,
                                Ok(n) => {
                                    if buf[..n].contains(&sv_transport::park::HELLO_TOKEN) && tx.send(()).is_err() {
                                        return;
                                    }
                                }
                            }
                        }
                        say(&format!("{spec} closed; reconnecting"));
                    }
                    Err(e) if !said => {
                        say(&format!("no doorbell hub at {spec} ({e}); sweeping on the timer until there is"));
                        said = true;
                    }
                    Err(_) => {}
                }
                std::thread::sleep(Duration::from_secs(1));
            }
        })
        .ok();
}

pub fn main(argv: &[String]) -> std::process::ExitCode {
    let args = match parse(argv) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("wc-arena-host mac-sessiond: {e}");
            return std::process::ExitCode::from(2);
        }
    };
    let (tx, rx) = mpsc::channel();
    if let Some(b) = &args.bell {
        spawn_bell(b.clone(), tx.clone());
    }
    let mut live: HashMap<String, u64> = running_sessions().into_iter().map(|id| (id, 0)).collect();
    if !live.is_empty() {
        say(&format!("adopted running session(s): {:?}", live.keys().collect::<Vec<_>>()));
    }
    // Ring windows answered (or refused) by this process, so a record whose
    // page the guest has not yet overwritten is never served twice.
    let mut served: HashSet<u64> = HashSet::new();
    let mut last_sweep: Option<Instant> = None;
    say(&format!(
        "serving {} with {}; sweep on hello{}",
        args.vm,
        args.session.display(),
        if args.sweep.is_zero() { String::new() } else { format!(" and every {:?}", args.sweep) }
    ));
    loop {
        let tick = Duration::from_secs(2);
        let woke = rx.recv_timeout(tick).is_ok();
        while rx.try_recv().is_ok() {}
        let due = !args.sweep.is_zero() && last_sweep.is_none_or(|t| t.elapsed() >= args.sweep);
        if woke || due {
            last_sweep = Some(Instant::now());
            serve(&args, &mut live, &mut served, if woke { "hello" } else { "timer" });
        }
        live.retain(|id, _| {
            if unit_active(id) {
                return true;
            }
            match verb(&args.session, &["down", id]) {
                Ok(r) => say(&format!("session {id} ended: {r}")),
                Err(e) => say(&format!("session {id} ended; {e}")),
            }
            false
        });
    }
}

fn serve(args: &Args, live: &mut HashMap<String, u64>, served: &mut HashSet<u64>, why: &str) {
    let t0 = Instant::now();
    let target = match sv_hostd::qemu::QemuTarget::discover(&args.qmp, &args.vm) {
        Ok(t) => t,
        Err(e) => {
            if why == "hello" {
                say(&format!("cannot reach the VM's memory: {e}"));
            }
            return;
        }
    };
    let found = match sweep(&target.file, target.file_len) {
        Ok(f) => f,
        Err(e) => {
            say(&format!("sweep: {e}"));
            return;
        }
    };
    let swept = t0.elapsed();
    let fresh: Vec<Found> = found.into_iter().filter(|f| !served.contains(&f.rec.rings_run)).collect();
    if fresh.is_empty() {
        if why == "hello" {
            say(&format!("hello, and no new boot record in {swept:?}"));
        }
        return;
    }
    for f in fresh {
        served.insert(f.rec.rings_run);
        let r = &f.rec;
        say(&format!(
            "boot record for session {} after {swept:?} ({why}): arena run 0x{:016x} ({} pages), rings run 0x{:016x} ({} pages)",
            r.session, r.arena_run, r.arena_pages, r.rings_run, r.rings_pages
        ));
        let t1 = Instant::now();
        let (ar, ap, out, rr, rp) = (
            format!("0x{:016x}", r.arena_run),
            r.arena_pages.to_string(),
            r.outbound.to_string(),
            format!("0x{:016x}", r.rings_run),
            r.rings_pages.to_string(),
        );
        let located = verb(&args.session, &["up", &r.session, &ar, &ap, &out])
            .and_then(|u| verb(&args.session, &["rings", &r.session, &rr, &rp]).map(|g| format!("{u}; {g}")));
        match located {
            Ok(text) => {
                let ok = answer(&target.file, &f, macboot::State::Located, &text);
                say(&format!("session {}: located in {:?}: {text} (answered: {ok:?})", r.session, t1.elapsed()));
                if !matches!(ok, Ok(true)) {
                    let _ = verb(&args.session, &["down", &r.session]);
                    continue;
                }
                match verb(&args.session, &["proxy", &r.session]) {
                    Ok(p) => {
                        say(&format!("session {}: started in {:?} after the hello's sweep: {p}", r.session, t0.elapsed()));
                        live.insert(r.session.clone(), r.rings_run);
                        // The session's GPU server, which the main process
                        // started in the guest and which offers its cold page
                        // under the session's label (§103.15).
                        if r.gpu {
                            let t2 = Instant::now();
                            match verb(&args.session, &["gpu", &r.session]) {
                                Ok(g) => say(&format!("session {}: gpu in {:?}: {g}", r.session, t2.elapsed())),
                                Err(e) => say(&format!("session {}: NO GPU: {e}", r.session)),
                            }
                        }
                    }
                    Err(e) => {
                        say(&format!("session {}: {e}", r.session));
                        let _ = verb(&args.session, &["down", &r.session]);
                    }
                }
            }
            Err(e) => {
                let _ = answer(&target.file, &f, macboot::State::Failed, &e);
                say(&format!("session {}: refused: {e}", r.session));
                let _ = verb(&args.session, &["down", &r.session]);
            }
        }
    }
}
