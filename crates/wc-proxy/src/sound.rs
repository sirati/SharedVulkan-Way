//! §103.16: the session side of the sound ring (`sv_transport::soundring`).
//!
//! The device side (the Mac) announces a stream per device and direction.
//! For each, this makes a PulseAudio device the session's applications see
//! (on the session's own server, or with `WC_LAUNCH_AUDIO=system` the user's
//! PipeWire or PulseAudio):
//!
//! * an **output** stream is `module-pipe-sink` writing into a FIFO this
//!   process reads. A pipe sink is paced by its reader, so when the Mac has
//!   granted no credit this process stops reading, the FIFO fills, the sink
//!   stops, and the application's stream waits: backpressure from the Mac's
//!   speakers to the Linux application, and nothing dropped;
//! * an **input** stream is `module-pipe-source` reading from a FIFO this
//!   process writes what the Mac recorded into, within the credit it grants.
//!   Capture is asked for (`Start`) only while something records from it
//!   (`pactl list short source-outputs`, once a second while there are input
//!   streams), and stopped (`Stop`) when nothing does;
//! * `Default` makes the Mac's default the session's (`set-default-sink`,
//!   `set-default-source`).
//!
//! Modules loaded are listed in `<dir>/sound.modules`, so `wc-session down`
//! can unload them from a server that outlives the session.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::process::Command;
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use sv_transport::control::Control;
use sv_transport::soundring::{Credits, Dir, Link, Msg, VERSION};
use sv_transport::wlbytes::ByteChannel;

/// How the session's sound server is reached.
#[derive(Clone, Debug, Default)]
pub struct SoundCfg {
    pub pactl: PathBuf,
    /// `PULSE_SERVER`, or `None` for the user's default server.
    pub server: Option<String>,
    /// The session's directory, for the FIFOs and the module list.
    pub dir: PathBuf,
}

/// What an input stream's FIFO is granted at a time.
const IN_CREDIT: u32 = 32 * 1024;

struct Stream {
    dir: Dir,
    name: String,
    module: Option<u32>,
    source: String,
    capturing: bool,
    gone: Arc<std::sync::atomic::AtomicBool>,
    /// Input: what the Mac recorded, for the FIFO writer.
    to_fifo: Option<std::sync::mpsc::Sender<Vec<u8>>>,
}

struct Shared {
    cfg: SoundCfg,
    credits: Mutex<Credits>,
    credit_cv: Condvar,
    streams: Mutex<HashMap<u16, Stream>>,
}

fn pactl(cfg: &SoundCfg, args: &[&str]) -> Result<String, String> {
    let mut c = Command::new(&cfg.pactl);
    c.args(args);
    if let Some(s) = &cfg.server {
        c.env("PULSE_SERVER", s);
    }
    let o = c.output().map_err(|e| format!("{}: {e}", cfg.pactl.display()))?;
    if o.status.success() {
        Ok(String::from_utf8_lossy(&o.stdout).trim().to_string())
    } else {
        Err(format!("pactl {}: {}", args.first().unwrap_or(&""), String::from_utf8_lossy(&o.stderr).trim()))
    }
}

fn mkfifo(path: &std::path::Path) -> Result<(), String> {
    let _ = std::fs::remove_file(path);
    let c = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).map_err(|e| e.to_string())?;
    // SAFETY: a NUL-terminated path.
    if unsafe { libc::mkfifo(c.as_ptr(), 0o600) } != 0 {
        return Err(format!("mkfifo {}: {}", path.display(), std::io::Error::last_os_error()));
    }
    Ok(())
}

/// A device's name as a PulseAudio property value: no quotes, no newlines.
fn description(name: &str) -> String {
    name.chars().filter(|c| !matches!(c, '\'' | '"' | '\\' | '\n' | '\r')).collect()
}

/// Serve the sound ring until it ends.
pub fn serve(control: &Arc<Control>, ch: ByteChannel, cfg: SoundCfg) {
    let shared = Arc::new(Shared {
        cfg,
        credits: Mutex::new(Credits::default()),
        credit_cv: Condvar::new(),
        streams: Mutex::new(HashMap::new()),
    });
    let sh = Arc::clone(&shared);
    let sh_end = Arc::clone(&shared);
    let link = sv_transport::soundring::spawn(
        ch,
        control,
        crate::rings::service_slice(),
        move |m, link| on_msg(&sh, m, link),
        move |why| {
            println!("[proxy] session sound: the ring ended ({why})");
            for s in sh_end.streams.lock().unwrap_or_else(|e| e.into_inner()).values() {
                s.gone.store(true, std::sync::atomic::Ordering::SeqCst);
            }
            sh_end.credit_cv.notify_all();
        },
    );
    let link = match link {
        Ok(l) => l,
        Err(e) => {
            println!("[proxy] session sound: {e}");
            return;
        }
    };
    link.send(Msg::Hello { version: VERSION });
    // Capture on demand: Start while something records from an input
    // stream's source, Stop when nothing does.
    let sh = Arc::clone(&shared);
    std::thread::Builder::new()
        .name("sound-capture-watch".into())
        .spawn(move || loop {
            std::thread::sleep(Duration::from_secs(1));
            let inputs: Vec<(u16, String, bool)> = sh
                .streams
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .iter()
                .filter(|(_, s)| s.dir == Dir::In && s.module.is_some())
                .map(|(id, s)| (*id, s.source.clone(), s.capturing))
                .collect();
            if inputs.is_empty() {
                continue;
            }
            let (Ok(sources), Ok(outs)) =
                (pactl(&sh.cfg, &["list", "short", "sources"]), pactl(&sh.cfg, &["list", "short", "source-outputs"]))
            else {
                continue;
            };
            let index_of = |name: &str| {
                sources.lines().find_map(|l| {
                    let f: Vec<&str> = l.split('\t').collect();
                    (f.get(1) == Some(&name)).then(|| f[0].to_string())
                })
            };
            for (id, source, was) in inputs {
                let Some(ix) = index_of(&source) else { continue };
                let now = outs.lines().any(|l| l.split('\t').nth(1) == Some(ix.as_str()));
                if now != was {
                    link.send(if now { Msg::Start { stream: id } } else { Msg::Stop { stream: id } });
                    if let Some(s) = sh.streams.lock().unwrap_or_else(|e| e.into_inner()).get_mut(&id) {
                        s.capturing = now;
                    }
                    println!("[proxy] session sound: {} capture {}", source, if now { "started" } else { "stopped" });
                }
            }
        })
        .ok();
}

fn on_msg(sh: &Arc<Shared>, m: Msg, link: &Link) {
    match m {
        Msg::Hello { version } => println!("[proxy] session sound: the device side speaks version {version}"),
        Msg::Device { stream, dir, channels, rate, name } => add(sh, link, stream, dir, channels, rate, &name),
        Msg::Gone { stream } => {
            let s = sh.streams.lock().unwrap_or_else(|e| e.into_inner()).remove(&stream);
            if let Some(s) = s {
                s.gone.store(true, std::sync::atomic::Ordering::SeqCst);
                sh.credit_cv.notify_all();
                if let Some(m) = s.module {
                    let _ = pactl(&sh.cfg, &["unload-module", &m.to_string()]);
                }
                println!("[proxy] session sound: {} is gone", s.name);
            }
            sh.credits.lock().unwrap_or_else(|e| e.into_inner()).forget(stream);
        }
        Msg::Default { stream, dir } => {
            let name = sh.streams.lock().unwrap_or_else(|e| e.into_inner()).get(&stream).map(|s| s.source.clone());
            if let Some(n) = name {
                let verb = if dir == Dir::Out { "set-default-sink" } else { "set-default-source" };
                match pactl(&sh.cfg, &[verb, &n]) {
                    Ok(_) => println!("[proxy] session sound: default {} is now {n}", if dir == Dir::Out { "output" } else { "input" }),
                    Err(e) => println!("[proxy] session sound: {e}"),
                }
            }
        }
        Msg::Credit { stream, bytes } => {
            sh.credits.lock().unwrap_or_else(|e| e.into_inner()).grant(stream, bytes);
            sh.credit_cv.notify_all();
        }
        Msg::Data { stream, pcm } => {
            let tx = sh.streams.lock().unwrap_or_else(|e| e.into_inner()).get(&stream).and_then(|s| s.to_fifo.clone());
            if let Some(tx) = tx {
                let _ = tx.send(pcm);
            }
        }
        Msg::Start { .. } | Msg::Stop { .. } => {}
    }
}

fn add(sh: &Arc<Shared>, link: &Link, stream: u16, dir: Dir, channels: u8, rate: u32, name: &str) {
    let tag = format!("wc_mac_{}_{stream}", if dir == Dir::Out { "out" } else { "in" });
    let fifo = sh.cfg.dir.join(format!("sound-{stream}.fifo"));
    if let Err(e) = mkfifo(&fifo) {
        println!("[proxy] session sound: {e}");
        return;
    }
    let desc = description(name);
    let common = format!("file={} format=s16le rate={rate} channels={channels}", fifo.display());
    // One argument string after the module's name: pactl hands it to the
    // module as is, so the quoted description may hold spaces.
    let (module_name, args) = match dir {
        Dir::Out => ("module-pipe-sink", format!("{common} sink_name={tag} sink_properties=\"device.description='{desc}'\"")),
        Dir::In => ("module-pipe-source", format!("{common} source_name={tag} source_properties=\"device.description='{desc}'\"")),
    };
    let module = match pactl(&sh.cfg, &["load-module", module_name, &args]) {
        Ok(m) => m.parse::<u32>().ok(),
        Err(e) => {
            println!("[proxy] session sound: {name}: {e}");
            return;
        }
    };
    if let Some(m) = module
        && let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(sh.cfg.dir.join("sound.modules"))
    {
        let _ = writeln!(f, "{m}");
    }
    let gone = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let mut s = Stream { dir, name: name.to_string(), module, source: tag.clone(), capturing: false, gone: Arc::clone(&gone), to_fifo: None };
    match dir {
        Dir::Out => {
            let (sh2, link2) = (Arc::clone(sh), link.clone());
            std::thread::Builder::new().name(format!("sound-out-{stream}")).spawn(move || pump_out(&sh2, &link2, stream, &fifo, &gone)).ok();
        }
        Dir::In => {
            let (tx, rx) = std::sync::mpsc::channel::<Vec<u8>>();
            s.to_fifo = Some(tx);
            let link2 = link.clone();
            std::thread::Builder::new().name(format!("sound-in-{stream}")).spawn(move || pump_in(&link2, stream, &fifo, &rx)).ok();
            link.send(Msg::Credit { stream, bytes: IN_CREDIT });
        }
    }
    println!("[proxy] session sound: {} {tag} for \"{name}\" ({rate} Hz, {channels} ch)", if dir == Dir::Out { "sink" } else { "source" });
    sh.streams.lock().unwrap_or_else(|e| e.into_inner()).insert(stream, s);
}

/// The pipe sink's FIFO to the ring, never ahead of the Mac's credit.
fn pump_out(sh: &Shared, link: &Link, stream: u16, fifo: &std::path::Path, gone: &std::sync::atomic::AtomicBool) {
    // The sink holds the FIFO open for writing, so this open returns.
    let mut f = match std::fs::File::open(fifo) {
        Ok(f) => f,
        Err(e) => {
            println!("[proxy] session sound: {}: {e}", fifo.display());
            return;
        }
    };
    let mut buf = vec![0u8; CHUNK];
    loop {
        let n = {
            let mut c = sh.credits.lock().unwrap_or_else(|e| e.into_inner());
            loop {
                if gone.load(std::sync::atomic::Ordering::SeqCst) {
                    return;
                }
                let n = c.available(stream, buf.len()) & !3;
                if n > 0 {
                    break n;
                }
                // No credit: do not read, so the FIFO fills and the sink waits.
                c = sh.credit_cv.wait_timeout(c, Duration::from_millis(500)).map(|r| r.0).unwrap_or_else(|e| e.into_inner().0);
            }
        };
        match f.read(&mut buf[..n]) {
            Ok(0) | Err(_) => return,
            Ok(got) => {
                let got = got & !3;
                if got == 0 {
                    continue;
                }
                sh.credits.lock().unwrap_or_else(|e| e.into_inner()).spend(stream, got);
                if !link.send(Msg::Data { stream, pcm: buf[..got].to_vec() }) {
                    return;
                }
            }
        }
    }
}

/// What the Mac recorded, into the pipe source's FIFO; the credit goes back
/// as the source takes it.
fn pump_in(link: &Link, stream: u16, fifo: &std::path::Path, rx: &std::sync::mpsc::Receiver<Vec<u8>>) {
    let mut f = match std::fs::OpenOptions::new().write(true).open(fifo) {
        Ok(f) => f,
        Err(e) => {
            println!("[proxy] session sound: {}: {e}", fifo.display());
            return;
        }
    };
    while let Ok(pcm) = rx.recv() {
        if f.write_all(&pcm).is_err() {
            return;
        }
        link.send(Msg::Credit { stream, bytes: pcm.len() as u32 });
    }
}

/// Bytes per read from a pipe sink: 20 ms of s16le stereo at 48 kHz.
pub const CHUNK: usize = 3840;
