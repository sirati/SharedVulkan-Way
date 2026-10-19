//! §105: the Windows session's boot, control, lease and sound, held by the
//! process that maps the shared-memory device.
//!
//! On Windows the ivshmem driver gives one mapping per device, and this
//! process holds it. So everything the launcher used ssh for until §105 goes
//! through here:
//!
//! * **The boot.** After the arena header is published and the Wayland rings
//!   are made in the region's tail (§83, §89), [`start`] writes a boot record
//!   into the header page (`wc_handle::macboot`, the record macOS writes into
//!   its ring window, §103.15) with an authenticator under the installation's
//!   key, and rings `HELLO_TOKEN` on the rings' doorbell once a second until
//!   the host's session service (`wc-arena-host sessiond --ivshmem`) answers in
//!   the same page. The host then has the proxy up and has claimed the control
//!   and sound slots.
//! * **Control.** The control slot (`OWNER_CONTROL`) carries the launcher's
//!   requests, the words `wc-session` takes. Every `wc-launch.exe` of this
//!   installation (the owner and every later click) sends them to a local
//!   AF_UNIX socket in its state directory, one request per connection; this
//!   module passes each through, one at a time, and returns the answer. The
//!   socket is in the user's profile, so only that user (and administrators)
//!   can reach it. Nothing in that hop crosses the VM boundary.
//! * **The lease.** `ping` on the control slot every half second. The host's
//!   proxy ends five seconds after the last one (`wc_proxy::macsvc`), and the
//!   session service then takes the session down. This process ending is the
//!   session ending, as it always was.
//! * **Sound.** The sound slot (`OWNER_AUDIO`) carries the session's
//!   PulseAudio monitor, s16le 48 kHz stereo, which is played through waveOut
//!   here (`wc-launch`'s player, compiled in).

use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use sv_transport::wlbytes::{ByteChannel, Endpoint, ReadOutcome, OWNER_AUDIO, OWNER_CONTROL};

/// What the launcher asked for, on the command line.
#[derive(Debug, Clone, Default)]
pub struct Config {
    pub session: String,
    pub install: String,
    pub key_file: PathBuf,
    pub gpu: bool,
    pub control_socket: Option<PathBuf>,
    pub audio_volume: Option<u8>,
}

static CONFIG: Mutex<Option<Config>> = Mutex::new(None);

fn with_config(f: impl FnOnce(&mut Config)) {
    let mut g = CONFIG.lock().unwrap_or_else(|e| e.into_inner());
    f(g.get_or_insert_with(Config::default));
}

/// One of the `--boot-*`, `--control-socket` or `--session-audio` flags.
/// Returns whether `flag` was one.
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

fn say(msg: &str) {
    println!("wc-compositor: {msg}");
    let _ = std::io::stdout().flush();
}

fn now_micros() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_micros() as u64)
}

/// Start the boot for ivshmem arena `id`, once its header is published and
/// its rings exist. Returns at once; the work is on threads of its own and
/// never on the Wayland thread.
pub fn start(cfg: Config, id: u32) {
    let _ = std::thread::Builder::new().name("session-boot".into()).spawn(move || {
        if let Err(e) = boot(&cfg, id) {
            say(&format!("boot: FAILED: {e}"));
            return;
        }
        services(cfg);
    });
}

/// Write the record, ring, and wait for the answer.
fn boot(cfg: &Config, id: u32) -> Result<(), String> {
    use wc_handle::macboot;
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
    let auth = macboot::Auth::sign(&rec, &cfg.install, now_micros(), &key);
    let page0 = wc_handle::arena::ivshmem_header_page(id).map_err(|e| e.to_string())?;
    // SAFETY: the header page of the device this process maps, for the life of
    // the process; nothing else in this process writes past the header's 80
    // bytes of it.
    unsafe { macboot::publish(page0, &rec, Some(&auth)) };
    let bell = sv_transport::park::cross_wake();
    say(&format!(
        "boot: record for session {} of installation {} in the header page (run 0x{run:016x}, gpu {}); {}",
        cfg.session,
        cfg.install,
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
                say(&format!("boot: located by the host in {} ms: {}", t0.elapsed().as_millis(), macboot::message(page)));
                return Ok(());
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

/// How long a request waits for its answer before it parks (§105.13).
const SPIN: Duration = Duration::from_micros(300);

/// The control channel, shared by the lease and the requests. The channel
/// lock is held for one read or write at a time, never across a request, so
/// the lease's ping is never late behind a slow `wc-session` verb.
struct Ctl {
    ch: Mutex<ByteChannel>,
    /// One request at a time: replies are matched to requests by order.
    turn: Mutex<()>,
}

impl Ctl {
    fn write_all(&self, mut b: &[u8]) -> Result<(), String> {
        let t0 = Instant::now();
        while !b.is_empty() {
            let n = self.ch.lock().unwrap_or_else(|e| e.into_inner()).write(b).map_err(|e| e.to_string())?;
            if n > 0 {
                b = &b[n..];
            } else if t0.elapsed() > Duration::from_secs(5) {
                return Err("the control ring stayed full for 5 s".into());
            } else {
                std::thread::sleep(Duration::from_millis(1));
            }
        }
        Ok(())
    }

    /// Read at least one byte into `out`, or fail after `wait`.
    fn read_some(&self, out: &mut Vec<u8>, deadline: Instant) -> Result<(), String> {
        let mut buf = [0u8; 16 * 1024];
        loop {
            {
                let mut ch = self.ch.lock().unwrap_or_else(|e| e.into_inner());
                match ch.read(&mut buf).map_err(|e| e.to_string())? {
                    ReadOutcome::Bytes(n) => {
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
                while t0.elapsed() < SPIN && !ch.readable().unwrap_or(true) {
                    std::hint::spin_loop();
                }
                if ch.readable().unwrap_or(true) {
                    continue;
                }
                // Short, so the lease never waits long behind this lock.
                let _ = ch.wait_readable(crate::ringlink::ring_wait_slice().min(Duration::from_millis(20)));
            }
        }
    }

    /// One request, and its whole answer: a line, plus the bytes an
    /// `ok-raw N` line announces.
    fn request(&self, line: &str) -> Result<Vec<u8>, String> {
        let _turn = self.turn.lock().unwrap_or_else(|e| e.into_inner());
        self.write_all(format!("{line}\n").as_bytes())?;
        // Every verb answers in seconds; a proxy that ended without answering
        // (its lease, a crash) must not hold the launcher for minutes.
        let deadline = Instant::now() + Duration::from_secs(30);
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

/// After the boot: control, lease, sound.
fn services(cfg: Config) {
    let Some(ch) = service_channel(OWNER_CONTROL, Duration::from_secs(20)) else {
        say("session: the host never opened the control ring; ending");
        std::process::exit(3);
    };
    say(&format!("session: control on ring slot {} (no ssh)", ch.slot()));
    let ctl = Arc::new(Ctl { ch: Mutex::new(ch), turn: Mutex::new(()) });
    {
        let ctl = Arc::clone(&ctl);
        let _ = std::thread::Builder::new().name("session-lease".into()).spawn(move || loop {
            std::thread::sleep(Duration::from_millis(500));
            if let Err(e) = ctl.write_all(b"ping\n") {
                say(&format!("session: the lease could not ping ({e}); the host will end the session"));
                return;
            }
        });
    }
    if let Some(path) = cfg.control_socket.clone() {
        let ctl = Arc::clone(&ctl);
        let _ = std::thread::Builder::new().name("session-control".into()).spawn(move || serve_socket(path, ctl));
    }
    if let Some(vol) = cfg.audio_volume {
        let _ = std::thread::Builder::new().name("session-sound".into()).spawn(move || {
            match service_channel(OWNER_AUDIO, Duration::from_secs(20)) {
                Some(ch) => {
                    say(&format!("audio: the session's sound ring is slot {} (§103.16, no ssh)", ch.slot()));
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
    // §106: host integration (links and files both ways) on its own slot and
    // thread, when the launcher passed `--hostint`.
    if let Some(hcfg) = crate::hostint::configured() {
        let _ = std::thread::Builder::new().name("session-hostint".into()).spawn(move || {
            match service_channel(sv_transport::wlbytes::OWNER_HOSTINT, Duration::from_secs(20)) {
                Some(ch) => {
                    say(&format!("hostint: on ring slot {} (no ssh)", ch.slot()));
                    #[cfg(windows)]
                    crate::hostint::start_windows(ch, hcfg, say);
                    #[cfg(not(windows))]
                    crate::hostint::start_logging(ch, hcfg, say);
                }
                None => say("hostint: the host opened no host-integration slot; links and files stay on their side"),
            }
        });
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
    let tx = Mutex::new(Some(tx));
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
        |why| say(&format!("audio: the sound ring ended: {why}")),
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

/// The launcher's socket: one request line per connection, the answer back,
/// then the connection is closed.
fn serve_socket(path: PathBuf, ctl: Arc<Ctl>) {
    let _ = std::fs::remove_file(&path);
    let listener = match wc_transport::Listener::bind(&path) {
        Ok(l) => l,
        Err(e) => {
            say(&format!("session: cannot listen on {}: {e}; the launcher cannot reach the host", path.display()));
            return;
        }
    };
    say(&format!("session: control socket {}", path.display()));
    loop {
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
}

/// One connection of a launcher: request lines, each answered in order, until
/// it closes. A launcher sends one; `--ping N` sends N on one connection.
fn answer(mut conn: wc_transport::Stream, ctl: &Ctl) {
    let _ = conn.set_nonblocking(false);
    let _ = conn.set_read_timeout(Some(Duration::from_secs(60)));
    let mut buf = Vec::with_capacity(256);
    let mut tmp = [0u8; 4096];
    loop {
        let line = loop {
            if let Some(i) = buf.iter().position(|b| *b == b'\n') {
                let l: Vec<u8> = buf.drain(..=i).collect();
                break String::from_utf8_lossy(&l[..l.len() - 1]).trim().to_string();
            }
            if buf.len() > 16 * 1024 {
                return;
            }
            match conn.read(&mut tmp) {
                Ok(0) | Err(_) => return,
                Ok(n) => buf.extend_from_slice(&tmp[..n]),
            }
        };
        // `ping` is the lease's word on the ring; a launcher's bare `ping` is
        // answered here, from this side. `rtt` crosses (§105.13).
        let out = if line == "ping" {
            b"ok session up\n".to_vec()
        } else {
            match ctl.request(&line) {
                Ok(v) => v,
                Err(e) => format!("err {e}\n").into_bytes(),
            }
        };
        if conn.write_all(&out).is_err() {
            return;
        }
    }
}
