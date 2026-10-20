//! §113: the session lease, and a guest that is frozen rather than gone.
//!
//! The guest pings every half second; five seconds without a ping ended the
//! proxy (§103.14). On testhost that killed sessions whenever the whole
//! Windows guest stalled for more than five seconds: its passed-through iGPU
//! waking from D3 holds every vCPU for up to ten (evidence in
//! /mnt/datasets/builds/evidence/frozen-guest/). A stalled guest and a dead
//! one look the same from one session's pings. Two more witnesses tell them
//! apart:
//!
//! * **The guest's heartbeat**, the owner word of the ring control region,
//!   which the compositor beats every 100 ms for the whole process
//!   (`ringlink::start_guest_beat`) and every session's proxy can read. If it
//!   moves while this session's pings have stopped, the guest runs and this
//!   session's side is gone: the lease ends after five seconds, as before.
//! * **The VM's process** on this host (`WC_LAUNCH_VM_PID`,
//!   `WC_LAUNCH_VM_PIDFILE` or `WC_LAUNCH_VM_UNIT`), read from `/proc`, nothing privileged. When the
//!   heartbeat stops too, it decides: threads stopped (`T`) or at least two
//!   threads busy all the time (vCPUs that never halt) is a stalled guest,
//!   held up to [`Cfg::frozen_max`]; a VM process that is gone, or idle, means
//!   the guest runs but its compositor does not, and the lease ends after five
//!   seconds.
//!
//! Without a VM witness a guest whose heartbeat has stopped is held for
//! [`Cfg::blind_max`]. A guest that never beat (a build before §113) gets the
//! old five seconds. A stall of this process itself (the host suspended, a
//! loaded machine) restarts the clocks rather than counting against the guest.
//!
//! [`Judge`] is the policy, pure and clocked by its caller; [`ProcVm`] is the
//! `/proc` witness.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

/// The lease as it always was: this long without a ping ends it.
pub const LEASE: Duration = Duration::from_secs(5);
/// The guest's heartbeat moves every 100 ms; this long unmoved, the guest is
/// silent.
pub const SILENT: Duration = Duration::from_millis(1000);
/// A gap this long between two of this process's own ticks (every 250 ms) is
/// this process stalled, not the guest.
pub const SELF_STALL: Duration = Duration::from_millis(1500);
/// How many moves of the heartbeat make it a witness. Fewer, and the guest is
/// taken for one that does not beat at all.
const MOVES: u32 = 3;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cfg {
    /// The longest a stalled guest is held while the VM witness says stalled.
    pub frozen_max: Duration,
    /// The longest a silent guest is held without a VM witness.
    pub blind_max: Duration,
}

impl Default for Cfg {
    fn default() -> Self {
        Cfg { frozen_max: Duration::from_secs(60), blind_max: Duration::from_secs(20) }
    }
}

impl Cfg {
    /// `WC_LAUNCH_LEASE_FROZEN_SECS`, `WC_LAUNCH_LEASE_BLIND_SECS`.
    pub fn from_env() -> Cfg {
        let secs = |k: &str, d: Duration| {
            std::env::var(k).ok().and_then(|v| v.trim().parse::<u64>().ok()).map_or(d, Duration::from_secs)
        };
        let d = Cfg::default();
        Cfg { frozen_max: secs("WC_LAUNCH_LEASE_FROZEN_SECS", d.frozen_max), blind_max: secs("WC_LAUNCH_LEASE_BLIND_SECS", d.blind_max) }
    }
}

/// What the VM's process says while the guest is silent.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Vm {
    /// No witness configured, or `/proc` could not be read.
    Unknown,
    /// The process has exited.
    Gone,
    /// A thread is stopped or traced (`T`, `t`): SIGSTOP, a debugger.
    Stopped,
    /// This many threads were busy for the whole window: vCPUs that do not halt.
    Busy(u32),
    /// Running, and mostly halted: the guest is idle.
    Idle,
}

impl Vm {
    fn stalled(&self) -> bool {
        matches!(self, Vm::Stopped | Vm::Busy(_))
    }
}

/// Consulted only while the guest is silent.
pub trait Witness {
    fn sample(&mut self, now: Instant) -> Vm;
}

/// No VM witness.
pub struct Blind;

impl Witness for Blind {
    fn sample(&mut self, _: Instant) -> Vm {
        Vm::Unknown
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Verdict {
    Live,
    /// The guest as a whole is silent and this lease rides it out.
    Hold { silent: Duration, vm: Vm },
    Lost(String),
}

pub struct Judge {
    cfg: Cfg,
    last_ping: Option<Instant>,
    /// The latest point the ping clock restarts from: the start, a stall of
    /// this process, the guest's heartbeat moving again after a silence.
    since: Instant,
    beat: Option<u64>,
    beat_moved: Option<Instant>,
    moves: u32,
    last_tick: Option<Instant>,
    vm: Vm,
}

impl Judge {
    pub fn new(cfg: Cfg, now: Instant) -> Judge {
        Judge { cfg, last_ping: None, since: now, beat: None, beat_moved: None, moves: 0, last_tick: None, vm: Vm::Unknown }
    }

    pub fn ping(&mut self, at: Instant) {
        if self.last_ping.is_none_or(|p| at > p) {
            self.last_ping = Some(at);
        }
    }

    /// Whether the guest's heartbeat is a witness and has stopped.
    pub fn guest_silent(&self, now: Instant) -> Option<Duration> {
        let moved = self.beat_moved?;
        (self.moves >= MOVES && now.duration_since(moved) > SILENT).then(|| now.duration_since(moved))
    }

    /// One look, every 250 ms or so. `beat` is the guest's heartbeat word,
    /// `None` when it cannot be read.
    pub fn tick(&mut self, now: Instant, beat: Option<u64>, w: &mut dyn Witness) -> Verdict {
        if self.last_tick.is_some_and(|t| now.duration_since(t) > SELF_STALL) {
            // This process did not run: nothing it failed to see counts.
            self.since = now;
            if self.beat_moved.is_some() {
                self.beat_moved = Some(now);
            }
        }
        self.last_tick = Some(now);
        if let Some(b) = beat
            && self.beat != Some(b)
        {
            if self.beat.is_some() {
                if self.guest_silent(now).is_some() {
                    // Back from a silence: its pinger gets a fresh lease.
                    self.since = now;
                }
                self.moves = self.moves.saturating_add(1);
            }
            self.beat = Some(b);
            self.beat_moved = Some(now);
        }
        let silent = self.guest_silent(now);
        self.vm = if silent.is_some() { w.sample(now) } else { Vm::Unknown };
        let Some(ping) = self.last_ping else { return Verdict::Live };
        let age = now.duration_since(ping.max(self.since));
        if age <= LEASE {
            return Verdict::Live;
        }
        let Some(silent) = silent else {
            return Verdict::Lost(if self.moves >= MOVES {
                "no ping for 5 s while the guest's compositor beats: this session's side is gone".into()
            } else {
                "no ping from the guest's main process for 5 s".into()
            });
        };
        let secs = silent.as_secs();
        match &self.vm {
            Vm::Gone => Verdict::Lost(format!("the guest has been silent for {secs} s and its VM process is gone")),
            Vm::Idle => Verdict::Lost(format!("the guest's compositor has been silent for {secs} s while its VM runs idle: the compositor is gone")),
            vm if vm.stalled() && silent > self.cfg.frozen_max => Verdict::Lost(format!(
                "the guest has been stalled for {secs} s, longer than the {} s a frozen guest is held",
                self.cfg.frozen_max.as_secs()
            )),
            Vm::Unknown if silent > self.cfg.blind_max => Verdict::Lost(format!(
                "the guest has been silent for {secs} s, longer than the {} s a silent guest is held without a VM witness",
                self.cfg.blind_max.as_secs()
            )),
            vm => Verdict::Hold { silent, vm: vm.clone() },
        }
    }
}

/// The VM's process, from `/proc/PID/task/*/stat`.
pub struct ProcVm {
    source: Source,
    pid: Option<u32>,
    resolved: bool,
    /// (when, tid → utime+stime ticks), oldest first, at most ~3 s.
    samples: Vec<(Instant, HashMap<u32, u64>)>,
    hz: f64,
    proc_root: PathBuf,
}

enum Source {
    Pid(u32),
    /// Read when first needed: the harness's guest starts after the service.
    PidFile(PathBuf),
    Unit(String),
}

/// A window shorter than this does not decide busy.
const WINDOW: Duration = Duration::from_millis(900);
/// The share of the window a thread must have run to count as busy.
const BUSY: f64 = 0.8;

impl ProcVm {
    /// `WC_LAUNCH_VM_PID`, `WC_LAUNCH_VM_PIDFILE` or `WC_LAUNCH_VM_UNIT` (a
    /// system unit whose main process is the VM, e.g. `windows-vm`); `None`
    /// with none of them.
    pub fn from_env() -> Option<ProcVm> {
        if let Some(p) = std::env::var("WC_LAUNCH_VM_PID").ok().and_then(|v| v.trim().parse().ok()) {
            return Some(ProcVm::new(Source::Pid(p), "/proc"));
        }
        if let Some(f) = std::env::var_os("WC_LAUNCH_VM_PIDFILE").filter(|f| !f.is_empty()) {
            return Some(ProcVm::new(Source::PidFile(f.into()), "/proc"));
        }
        let unit = std::env::var("WC_LAUNCH_VM_UNIT").ok().filter(|u| !u.trim().is_empty())?;
        Some(ProcVm::new(Source::Unit(unit.trim().to_string()), "/proc"))
    }

    pub fn for_pid(pid: u32, proc_root: impl Into<PathBuf>) -> ProcVm {
        ProcVm::new(Source::Pid(pid), proc_root)
    }

    fn new(source: Source, proc_root: impl Into<PathBuf>) -> ProcVm {
        // SAFETY: sysconf has no preconditions.
        let hz = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
        ProcVm { source, pid: None, resolved: false, samples: Vec::new(), hz: if hz > 0 { hz as f64 } else { 100.0 }, proc_root: proc_root.into() }
    }

    pub fn describe(&self) -> String {
        match &self.source {
            Source::Pid(p) => format!("VM process {p}"),
            Source::PidFile(f) => format!("VM process from {}", f.display()),
            Source::Unit(u) => format!("VM unit {u} (process {})", self.pid.map_or("unresolved".into(), |p| p.to_string())),
        }
    }

    /// At the start, and then never again once found: a VM that restarts
    /// under a session has ended that session's guest, and the old process
    /// being gone is the right answer.
    pub fn prime(&mut self) -> Option<u32> {
        self.pid()
    }

    fn pid(&mut self) -> Option<u32> {
        if !self.resolved {
            self.pid = match &self.source {
                Source::Pid(p) => Some(*p),
                Source::PidFile(f) => std::fs::read_to_string(f).ok().and_then(|s| s.trim().parse().ok()),
                Source::Unit(u) => std::process::Command::new("systemctl")
                    .args(["show", "-p", "MainPID", "--value", u])
                    .output()
                    .ok()
                    .and_then(|o| String::from_utf8_lossy(&o.stdout).trim().parse::<u32>().ok())
                    .filter(|p| *p != 0),
            };
            // Until there is one: a pid file may not be written yet.
            self.resolved = self.pid.is_some();
        }
        self.pid
    }

    /// `(state, utime + stime)` of every thread, `None` when the process is gone.
    fn read(&self, pid: u32) -> Option<Vec<(u32, char, u64)>> {
        let dir = std::fs::read_dir(self.proc_root.join(pid.to_string()).join("task")).ok()?;
        let mut out = Vec::new();
        for e in dir.flatten() {
            let Some(tid) = e.file_name().to_str().and_then(|s| s.parse::<u32>().ok()) else { continue };
            let Ok(s) = std::fs::read_to_string(e.path().join("stat")) else { continue };
            if let Some((state, ticks)) = parse_stat(&s) {
                out.push((tid, state, ticks));
            }
        }
        Some(out)
    }
}

/// The state and utime + stime of one `/proc/.../stat` line. The command
/// name may hold spaces and parentheses; the fields start after the last `)`.
pub fn parse_stat(s: &str) -> Option<(char, u64)> {
    let rest = &s[s.rfind(')')? + 1..];
    let f: Vec<&str> = rest.split_whitespace().collect();
    let state = f.first()?.chars().next()?;
    let utime: u64 = f.get(11)?.parse().ok()?;
    let stime: u64 = f.get(12)?.parse().ok()?;
    Some((state, utime + stime))
}

impl Witness for ProcVm {
    fn sample(&mut self, now: Instant) -> Vm {
        let Some(pid) = self.pid() else { return Vm::Unknown };
        let Some(threads) = self.read(pid) else {
            return if self.proc_root.join(pid.to_string()).exists() { Vm::Unknown } else { Vm::Gone };
        };
        if threads.is_empty() {
            return Vm::Gone;
        }
        if threads.iter().any(|(_, s, _)| matches!(s, 'T' | 't')) {
            return Vm::Stopped;
        }
        let map: HashMap<u32, u64> = threads.iter().map(|(t, _, c)| (*t, *c)).collect();
        self.samples.retain(|(t, _)| now.duration_since(*t) <= Duration::from_secs(3));
        self.samples.push((now, map));
        let (t0, old) = &self.samples[0];
        let span = now.duration_since(*t0);
        if span < WINDOW {
            return Vm::Unknown;
        }
        let (_, new) = self.samples.last().expect("just pushed");
        let need = BUSY * span.as_secs_f64() * self.hz;
        let busy = new.iter().filter(|(tid, c)| old.get(tid).is_some_and(|o| (c.saturating_sub(*o)) as f64 >= need)).count() as u32;
        if busy >= 2 { Vm::Busy(busy) } else { Vm::Idle }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixed(Vm);
    impl Witness for Fixed {
        fn sample(&mut self, _: Instant) -> Vm {
            self.0.clone()
        }
    }

    /// A guest that pings and beats until `stop`, from `t0`, ticking every
    /// 250 ms up to `until`; after `resume` (if any) both start again.
    /// Returns the first verdict that is not Live, with its time.
    fn run(
        judge: &mut Judge,
        w: &mut dyn Witness,
        t0: Instant,
        stop_ping: Duration,
        stop_beat: Duration,
        resume: Option<Duration>,
        until: Duration,
    ) -> Option<(Duration, Verdict)> {
        let mut t = Duration::ZERO;
        let mut beat = 0u64;
        let mut held = None;
        while t <= until {
            let now = t0 + t;
            let back = resume.is_some_and(|r| t >= r);
            let beating = t < stop_beat || back;
            if beating {
                beat += 1;
            }
            if (t < stop_ping || back) && (t.as_millis() % 500 == 0) {
                judge.ping(now);
            }
            match judge.tick(now, Some(beat), w) {
                Verdict::Live => {}
                v @ Verdict::Hold { .. } => held = held.or(Some((t, v))),
                v => return Some((t, v)),
            }
            t += Duration::from_millis(250);
        }
        held
    }

    #[test]
    fn a_guest_that_pings_is_live() {
        let t0 = Instant::now();
        let mut j = Judge::new(Cfg::default(), t0);
        assert_eq!(run(&mut j, &mut Blind, t0, Duration::from_secs(100), Duration::from_secs(100), None, Duration::from_secs(30)), None);
    }

    #[test]
    fn one_session_dying_while_the_guest_beats_ends_after_five_seconds() {
        let t0 = Instant::now();
        let mut j = Judge::new(Cfg::default(), t0);
        let (at, v) = run(&mut j, &mut Fixed(Vm::Busy(12)), t0, Duration::from_secs(10), Duration::from_secs(1000), None, Duration::from_secs(60)).unwrap();
        assert!(matches!(&v, Verdict::Lost(m) if m.contains("this session's side is gone")), "{v:?}");
        assert!(at > Duration::from_millis(14_500) && at <= Duration::from_millis(15_500), "ended at {at:?}");
    }

    #[test]
    fn an_eight_second_freeze_is_ridden_out() {
        // Pings and beat stop together at 10 s, both come back at 18 s.
        for w in [&mut Fixed(Vm::Busy(12)) as &mut dyn Witness, &mut Fixed(Vm::Stopped), &mut Blind] {
            let t0 = Instant::now();
            let mut j = Judge::new(Cfg::default(), t0);
            let first = run(&mut j, w, t0, Duration::from_secs(10), Duration::from_secs(10), Some(Duration::from_secs(18)), Duration::from_secs(60));
            match first {
                Some((_, Verdict::Hold { .. })) => {}
                other => panic!("{other:?}"),
            }
        }
    }

    #[test]
    fn after_a_freeze_the_pinger_gets_a_fresh_lease() {
        // The beat comes back at 18 s but the pings never do: lost five
        // seconds after the guest was seen running again, not at once.
        let t0 = Instant::now();
        let mut j = Judge::new(Cfg::default(), t0);
        let mut t = Duration::ZERO;
        let mut beat = 0;
        let mut lost = None;
        while t <= Duration::from_secs(40) && lost.is_none() {
            let now = t0 + t;
            if t < Duration::from_secs(10) || t >= Duration::from_secs(18) {
                beat += 1;
            }
            if t < Duration::from_secs(10) {
                j.ping(now);
            }
            if let Verdict::Lost(m) = j.tick(now, Some(beat), &mut Fixed(Vm::Busy(4))) {
                lost = Some((t, m));
            }
            t += Duration::from_millis(250);
        }
        let (at, m) = lost.unwrap();
        assert!(at > Duration::from_secs(23) && at <= Duration::from_secs(24), "ended at {at:?}: {m}");
    }

    #[test]
    fn a_dead_compositor_on_an_idle_vm_ends_after_five_seconds() {
        let t0 = Instant::now();
        let mut j = Judge::new(Cfg::default(), t0);
        let (at, v) = run(&mut j, &mut Fixed(Vm::Idle), t0, Duration::from_secs(10), Duration::from_secs(10), None, Duration::from_secs(60)).unwrap();
        assert!(matches!(&v, Verdict::Lost(m) if m.contains("compositor is gone")), "{v:?}");
        assert!(at <= Duration::from_secs(16), "ended at {at:?}");
    }

    #[test]
    fn a_gone_vm_ends_after_five_seconds() {
        let t0 = Instant::now();
        let mut j = Judge::new(Cfg::default(), t0);
        let (at, v) = run(&mut j, &mut Fixed(Vm::Gone), t0, Duration::from_secs(10), Duration::from_secs(10), None, Duration::from_secs(60)).unwrap();
        assert!(matches!(&v, Verdict::Lost(m) if m.contains("VM process is gone")), "{v:?}");
        assert!(at <= Duration::from_secs(16), "ended at {at:?}");
    }

    #[test]
    fn a_stall_is_held_only_so_long() {
        for (w, max) in [(&mut Fixed(Vm::Busy(12)) as &mut dyn Witness, 60u64), (&mut Blind, 20)] {
            let t0 = Instant::now();
            let mut j = Judge::new(Cfg::default(), t0);
            let mut t = Duration::ZERO;
            let mut beat = 0;
            let lost = loop {
                let now = t0 + t;
                if t < Duration::from_secs(10) {
                    beat += 1;
                    j.ping(now);
                }
                if let Verdict::Lost(m) = j.tick(now, Some(beat), w) {
                    break (t, m);
                }
                t += Duration::from_millis(250);
                assert!(t < Duration::from_secs(200));
            };
            let silent = lost.0 - Duration::from_millis(9_750);
            assert!(silent > Duration::from_secs(max) && silent <= Duration::from_secs(max + 1), "{max}: {lost:?}");
        }
    }

    #[test]
    fn a_guest_that_never_beats_keeps_the_old_lease() {
        let t0 = Instant::now();
        let mut j = Judge::new(Cfg::default(), t0);
        let mut t = Duration::ZERO;
        let lost = loop {
            let now = t0 + t;
            if t < Duration::from_secs(10) {
                j.ping(now);
            }
            if let Verdict::Lost(m) = j.tick(now, None, &mut Fixed(Vm::Busy(12))) {
                break (t, m);
            }
            t += Duration::from_millis(250);
        };
        assert!(lost.0 > Duration::from_millis(14_500) && lost.0 <= Duration::from_millis(15_500), "{lost:?}");
        assert!(lost.1.contains("main process"), "{lost:?}");
    }

    #[test]
    fn a_stall_of_this_process_does_not_count_against_the_guest() {
        // Ticks stop for 8 s (this process stopped); the guest kept pinging
        // but nobody looked. The next tick sees an old ping and must not end.
        let t0 = Instant::now();
        let mut j = Judge::new(Cfg::default(), t0);
        let mut beat = 0;
        for i in 0..40 {
            let now = t0 + Duration::from_millis(250 * i);
            beat += 1;
            j.ping(now);
            assert_eq!(j.tick(now, Some(beat), &mut Blind), Verdict::Live);
        }
        let now = t0 + Duration::from_millis(250 * 39) + Duration::from_secs(8);
        assert_eq!(j.tick(now, Some(beat + 30), &mut Blind), Verdict::Live);
    }

    #[test]
    fn stat_lines_parse_with_awkward_names() {
        let s = "38452 (CPU 0/KVM) R 1 2 3 4 5 6 7 8 9 10 1234 56 0 0 20 0 1 0 1 1 1";
        assert_eq!(parse_stat(s), Some(('R', 1290)));
        let s = "1 (a) b) c) T 1 2 3 4 5 6 7 8 9 10 7 8 0 0";
        assert_eq!(parse_stat(s), Some(('T', 15)));
    }

    #[test]
    fn the_proc_witness_reads_this_test_process() {
        // This process: running, not stopped; one sample is too short a
        // window, so Unknown; and a pid nobody has is Gone.
        let mut w = ProcVm::for_pid(std::process::id(), "/proc");
        assert_eq!(w.sample(Instant::now()), Vm::Unknown);
        let mut g = ProcVm::for_pid(u32::MAX - 7, "/proc");
        assert_eq!(g.sample(Instant::now()), Vm::Gone);
    }

    #[test]
    fn the_proc_witness_sees_busy_threads() {
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let spin: Vec<_> = (0..2)
            .map(|_| {
                let s = std::sync::Arc::clone(&stop);
                std::thread::spawn(move || {
                    while !s.load(std::sync::atomic::Ordering::Relaxed) {
                        std::hint::spin_loop();
                    }
                })
            })
            .collect();
        let mut w = ProcVm::for_pid(std::process::id(), "/proc");
        let t0 = Instant::now();
        w.sample(t0);
        std::thread::sleep(Duration::from_millis(1500));
        let v = w.sample(Instant::now());
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        for t in spin {
            t.join().unwrap();
        }
        assert!(matches!(v, Vm::Busy(n) if n >= 2), "{v:?}");
    }
}
