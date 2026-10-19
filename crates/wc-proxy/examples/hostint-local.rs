//! §106 and §103.16, end to end on one machine with no VM. Run by
//! `scripts/wc-hostint-local.sh`, which starts a private PipeWire stack
//! (`wc-session pw-stack`, the `WC_LAUNCH_AUDIO=pipewire` configuration) and
//! passes its pulse socket.
//!
//! One in-memory control region; the fake host owns it, the proxy's half is
//! the real code:
//!
//! * **sound**: `wc_proxy::sound::serve` on the `OWNER_AUDIO` slot, against a
//!   fake DEVICE side (`sv_transport::soundring`) that announces two outputs
//!   and a microphone with real Windows names, moves the defaults, grants
//!   credit, plugs a device in and out, and answers `Start` with samples;
//! * **links and files**: `wc_proxy::hostint::start` on `OWNER_HOSTINT`,
//!   against a fake host that answers `wc-host-open`'s requests.
//!
//! Every check prints `PASS` or `FAIL`; the exit status is the number of
//! failures.

#[cfg(not(unix))]
fn main() {}

#[cfg(unix)]
fn main() -> std::process::ExitCode {
    imp::main()
}

#[cfg(unix)]
mod imp {
    use std::collections::HashMap;
    use std::io::{Read, Write};
    use std::path::{Path, PathBuf};
    use std::process::{Child, Command, Stdio};
    use std::sync::mpsc;
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    use sv_transport::control::{Control, Geometry, ProtocolIds};
    use sv_transport::region::SharedRegion;
    use sv_transport::soundring::{self, Dir, Msg as Snd, FORMAT_S16LE, VERSION};
    use sv_transport::wlbytes::{ByteChannel, Endpoint, ReadOutcome, OWNER_AUDIO, OWNER_HOSTINT};
    use wc_hostint::wire::{b64, Handler, LineBuf, Msg, PathMap, Req, Target};
    use wc_proxy::hostint;
    use wc_proxy::sound::{self, SoundCfg};

    struct Args {
        dir: PathBuf,
        pulse: PathBuf,
        pipewire: PathBuf,
        pactl: PathBuf,
        pacat: PathBuf,
        parec: PathBuf,
        pw_metadata: Option<PathBuf>,
        opener: PathBuf,
        shim: Option<PathBuf>,
    }

    fn args() -> Args {
        let mut a = Args {
            dir: PathBuf::new(),
            pulse: PathBuf::new(),
            pipewire: PathBuf::new(),
            pactl: "pactl".into(),
            pacat: "pacat".into(),
            parec: "parec".into(),
            pw_metadata: None,
            opener: PathBuf::new(),
            shim: None,
        };
        let mut it = std::env::args().skip(1);
        while let Some(k) = it.next() {
            let v = it.next().unwrap_or_default();
            match k.as_str() {
                "--dir" => a.dir = v.into(),
                "--pulse" => a.pulse = v.into(),
                "--pipewire" => a.pipewire = v.into(),
                "--pactl" => a.pactl = v.into(),
                "--pacat" => a.pacat = v.into(),
                "--parec" => a.parec = v.into(),
                "--pw-metadata" => a.pw_metadata = Some(v.into()),
                "--opener" => a.opener = v.into(),
                "--shim" => a.shim = Some(v.into()),
                _ => panic!("unknown {k}"),
            }
        }
        a
    }

    static FAILS: Mutex<u32> = Mutex::new(0);

    fn check(ok: bool, what: &str) {
        println!("{} {what}", if ok { "PASS" } else { "FAIL" });
        if !ok {
            *FAILS.lock().unwrap() += 1;
        }
    }

    fn t0() -> &'static Instant {
        static T: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
        T.get_or_init(Instant::now)
    }

    fn say(s: &str) {
        println!("[{:7.3}] {s}", t0().elapsed().as_secs_f64());
    }

    /// Everything a sound command is pointed at must be the test's own
    /// server, never the user's session.
    fn guard(a: &Args, what: &str, p: &Path) {
        let mine = a.dir.to_string_lossy().into_owned();
        let s = p.to_string_lossy();
        assert!(s.starts_with(&mine) && !s.starts_with("/run/user/"), "REFUSING {what}: {s} is not the test's server");
    }

    /// A pulse client (pactl, pacat, parec) aimed at the test's server only.
    fn pulse_cmd(a: &Args, prog: &Path) -> Command {
        guard(a, &prog.display().to_string(), &a.pulse);
        let mut c = Command::new(prog);
        c.env("PULSE_SERVER", format!("unix:{}", a.pulse.display()))
            .env_remove("PIPEWIRE_REMOTE")
            .env("XDG_RUNTIME_DIR", a.dir.join("pw"))
            .env("PULSE_RUNTIME_PATH", a.dir.join("pw/pulse-client"))
            .stdin(Stdio::null());
        c
    }

    fn pactl(a: &Args, args: &[&str]) -> String {
        let o = pulse_cmd(a, &a.pactl).args(args).stderr(Stdio::null()).output().expect("pactl");
        String::from_utf8_lossy(&o.stdout).into_owned()
    }

    fn pw_metadata(a: &Args) -> String {
        let Some(p) = &a.pw_metadata else { return String::new() };
        guard(a, "pw-metadata", &a.pipewire);
        let rt = a.pipewire.parent().unwrap();
        let o = Command::new(p)
            .args(["-n", "default"])
            .env("PIPEWIRE_REMOTE", &a.pipewire)
            .env("PIPEWIRE_RUNTIME_DIR", rt)
            .env("XDG_RUNTIME_DIR", rt)
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
            .expect("pw-metadata");
        String::from_utf8_lossy(&o.stdout).into_owned()
    }

    fn sinks(a: &Args, kind: &str) -> Vec<String> {
        pactl(a, &["list", "short", kind]).lines().filter_map(|l| l.split('\t').nth(1).map(String::from)).collect()
    }

    /// `Name:` to `Description:` from `pactl list sinks|sources`.
    fn descriptions(a: &Args, kind: &str) -> HashMap<String, String> {
        let mut out = HashMap::new();
        let mut name = None;
        for l in pactl(a, &["list", kind]).lines() {
            let l = l.trim();
            if let Some(n) = l.strip_prefix("Name: ") {
                name = Some(n.to_string());
            } else if let (Some(d), Some(n)) = (l.strip_prefix("Description: "), name.take()) {
                out.insert(n, d.to_string());
            }
        }
        out
    }

    fn poll<T>(limit: Duration, mut f: impl FnMut() -> Option<T>) -> Option<(T, Duration)> {
        let s = Instant::now();
        loop {
            if let Some(v) = f() {
                return Some((v, s.elapsed()));
            }
            if s.elapsed() > limit {
                return None;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// The fake device side's view: what arrived, and what it granted.
    #[derive(Default)]
    struct Dev {
        granted: HashMap<u16, u64>,
        got: HashMap<u16, u64>,
        nonzero: HashMap<u16, u64>,
        over: u32,
        /// Credit the session granted for input streams.
        in_credit: HashMap<u16, u64>,
        starts: Vec<u16>,
        stops: Vec<u16>,
        hello: bool,
    }

    const SPK: u16 = 1;
    const PHONES: u16 = 2;
    const MIC: u16 = 3;
    const USB: u16 = 4;
    const SPK_N: &str = "win.sink.speakers_realtek_r_audio";
    const PHONES_N: &str = "win.sink.kopfh_rer_wh-1000xm5_stereo";
    const MIC_N: &str = "win.source.microphone_array_intel_smart_sound_technology_for_digital_microphones";
    const USB_N: &str = "win.sink.usb_headset";

    fn tone(secs: f32, channels: usize) -> Vec<u8> {
        let n = (48_000.0 * secs) as usize;
        let mut v = Vec::with_capacity(n * channels * 2);
        for i in 0..n {
            let s = ((i as f32 * 440.0 * std::f32::consts::TAU / 48_000.0).sin() * 8000.0) as i16;
            for _ in 0..channels {
                v.extend_from_slice(&s.to_le_bytes());
            }
        }
        v
    }

    fn sound_part(a: &Args, owner: &Arc<Control>, linux: &Arc<Control>) {
        // The proxy's half, exactly as macsvc starts it.
        let ch = ByteChannel::claim(linux, OWNER_AUDIO).expect("claim OWNER_AUDIO");
        let cfg = SoundCfg { pactl: a.pactl.clone(), server: Some(format!("unix:{}", a.pulse.display())), dir: a.dir.clone(), prefix: "win".into() };
        guard(a, "the sound ring's pactl", &a.pulse);
        let (l2, c2) = (Arc::clone(linux), cfg);
        std::thread::spawn(move || sound::serve(&l2, ch, c2));

        // The device side opens the slot the proxy claimed.
        let slot = poll(Duration::from_secs(5), || {
            ByteChannel::live_connections(owner).ok()?.into_iter().find(|s| owner.ring_owner(*s).ok() == Some(OWNER_AUDIO))
        })
        .expect("no OWNER_AUDIO slot")
        .0;
        let dch = ByteChannel::open(owner, slot, Endpoint::Peer).unwrap();
        let dev = Arc::new(Mutex::new(Dev::default()));
        let d2 = Arc::clone(&dev);
        let link = soundring::spawn(
            dch,
            owner,
            Duration::from_millis(5),
            move |m, _l| {
                let mut d = d2.lock().unwrap();
                match m {
                    Snd::Hello { .. } => d.hello = true,
                    Snd::Data { stream, pcm } => {
                        *d.got.entry(stream).or_default() += pcm.len() as u64;
                        *d.nonzero.entry(stream).or_default() += pcm.iter().filter(|b| **b != 0).count() as u64;
                        if d.got.get(&stream) > d.granted.get(&stream).or(Some(&0)) {
                            d.over += 1;
                        }
                    }
                    Snd::Credit { stream, bytes } => *d.in_credit.entry(stream).or_default() += u64::from(bytes),
                    Snd::Start { stream } => d.starts.push(stream),
                    Snd::Stop { stream } => d.stops.push(stream),
                    _ => {}
                }
            },
            |why| println!("[device] the ring ended: {why}"),
        )
        .expect("device link");
        let grant = |s: u16, b: u32| {
            *dev.lock().unwrap().granted.entry(s).or_default() += u64::from(b);
            link.send(Snd::Credit { stream: s, bytes: b });
        };
        link.send(Snd::Hello { version: VERSION });
        let device = |stream: u16, dir: Dir, ch: u8, name: &str| Snd::Device { stream, dir, channels: ch, rate: 48_000, name: name.into() };
        let _ = FORMAT_S16LE;
        let t = Instant::now();
        link.send(device(SPK, Dir::Out, 2, "Speakers (Realtek(R) Audio)"));
        link.send(device(PHONES, Dir::Out, 2, "Kopfhörer (WH-1000XM5 Stereo)"));
        link.send(device(MIC, Dir::In, 1, "Microphone Array (Intel® Smart Sound Technology for Digital Microphones)"));
        link.send(Snd::Default { stream: SPK, dir: Dir::Out });
        link.send(Snd::Default { stream: MIC, dir: Dir::In });
        check(poll(Duration::from_secs(3), || dev.lock().unwrap().hello.then_some(())).is_some(), "the session side says Hello on the sound ring");

        let up = poll(Duration::from_secs(10), || {
            let (s, r) = (sinks(a, "sinks"), sinks(a, "sources"));
            (s.iter().any(|x| x == SPK_N) && s.iter().any(|x| x == PHONES_N) && r.iter().any(|x| x == MIC_N)).then_some(())
        });
        check(up.is_some(), "two outputs and a microphone become sinks and a source named by their slugs");
        say(&format!("devices up {} ms after Device", t.elapsed().as_millis()));
        let (ds, dr) = (descriptions(a, "sinks"), descriptions(a, "sources"));
        for (n, want, m) in [
            (SPK_N, "Speakers (Realtek(R) Audio)", &ds),
            (PHONES_N, "Kopfhörer (WH-1000XM5 Stereo)", &ds),
            (MIC_N, "Microphone Array (Intel® Smart Sound Technology for Digital Microphones)", &dr),
        ] {
            check(m.get(n).map(String::as_str) == Some(want), &format!("{n} is described as {want:?} (got {:?})", m.get(n)));
        }
        let d = poll(Duration::from_secs(2), || (pactl(a, &["get-default-sink"]).trim() == SPK_N).then_some(()));
        check(d.is_some(), "the host's default output is the session's default sink");
        check(pactl(a, &["get-default-source"]).trim() == MIC_N, "the host's default input is the session's default source");

        // ---- the host's default output changes.
        let t = Instant::now();
        link.send(Snd::Default { stream: PHONES, dir: Dir::Out });
        let sw = poll(Duration::from_secs(3), || (pactl(a, &["get-default-sink"]).trim() == PHONES_N).then_some(()));
        let ms = t.elapsed();
        check(sw.is_some() && ms <= Duration::from_secs(1), &format!("a Default switch reaches `pactl get-default-sink` within 1 s ({} ms)", ms.as_millis()));
        if a.pw_metadata.is_some() {
            let md = poll(Duration::from_secs(1), || pw_metadata(a).contains(&format!("\"name\":\"{PHONES_N}\"")).then_some(()));
            check(md.is_some(), "and pw-metadata -n default shows it");
            say(&format!("pw-metadata -n default:\n{}", pw_metadata(a)));
        }

        // ---- a tone into a sink arrives as Data, never beyond the credit.
        const FIRST: u32 = 48_000; // 250 ms of 48 kHz stereo s16
        grant(PHONES, FIRST);
        let tonef = a.dir.join("tone.raw");
        std::fs::write(&tonef, tone(1.5, 2)).unwrap();
        let mut play: Child = pulse_cmd(a, &a.pacat)
            .args(["--raw", "--format=s16le", "--rate=48000", "--channels=2", "--latency-msec=20"])
            .arg(format!("--device={PHONES_N}"))
            .arg(&tonef)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("pacat");
        let first = poll(Duration::from_secs(5), || {
            let d = dev.lock().unwrap();
            (d.got.get(&PHONES).copied().unwrap_or(0) >= u64::from(FIRST)).then_some(())
        });
        check(first.is_some(), "a tone played by pacat into the sink arrives as Data on its stream");
        std::thread::sleep(Duration::from_millis(std::env::var("WC_TEST_HOLD_MS").ok().and_then(|v| v.parse().ok()).unwrap_or(400)));
        {
            let d = dev.lock().unwrap();
            let got = d.got.get(&PHONES).copied().unwrap_or(0);
            check(got <= u64::from(FIRST) && d.over == 0, &format!("and no more than its credit: {got} bytes for {FIRST} granted, while nothing more is granted"));
            check(d.nonzero.get(&PHONES).copied().unwrap_or(0) > u64::from(FIRST) / 2, "the Data is the tone, not silence");
        }
        // The rest of the tone and then some: every byte pacat wrote must arrive.
        let total = tone(1.5, 2).len() as u64;
        grant(PHONES, 8 * FIRST);
        let all = poll(Duration::from_secs(6), || {
            (dev.lock().unwrap().got.get(&PHONES).copied().unwrap_or(0) >= total).then_some(())
        });
        let got = dev.lock().unwrap().got.get(&PHONES).copied().unwrap_or(0);
        check(all.is_some(), &format!("with more credit the whole tone arrives, nothing dropped while it waited ({got} of {total} bytes)"));
        let _ = play.wait();
        check(dev.lock().unwrap().over == 0, "the session never sent Data beyond the device side's credit");

        // ---- capture only while something records.
        let mut rec: Child = pulse_cmd(a, &a.parec)
            .args(["--raw", "--format=s16le", "--rate=48000", "--channels=1", "--latency-msec=20"])
            .arg(format!("--device={MIC_N}"))
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("parec");
        let st = poll(Duration::from_secs(4), || dev.lock().unwrap().starts.contains(&MIC).then_some(()));
        check(st.is_some(), &format!("Start for the microphone when parec records from it ({:?})", st.map(|s| s.1)));
        // The device side records: a tone, within the session's credit.
        let mic = tone(0.5, 1);
        let mut sent = 0usize;
        let end = Instant::now() + Duration::from_secs(3);
        while sent < mic.len() && Instant::now() < end {
            let room = (dev.lock().unwrap().in_credit.get(&MIC).copied().unwrap_or(0) as usize).saturating_sub(sent);
            let n = room.min(1920).min(mic.len() - sent) & !1;
            if n == 0 {
                std::thread::sleep(Duration::from_millis(5));
                continue;
            }
            link.send(Snd::Data { stream: MIC, pcm: mic[sent..sent + n].to_vec() });
            sent += n;
        }
        let mut out = rec.stdout.take().unwrap();
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let mut buf = vec![0u8; 65536];
            let mut nz = 0usize;
            let end = Instant::now() + Duration::from_millis(1500);
            while Instant::now() < end {
                match out.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => nz += buf[..n].iter().filter(|b| **b != 0).count(),
                }
            }
            let _ = tx.send(nz);
        });
        let nz = rx.recv_timeout(Duration::from_secs(4)).unwrap_or(0);
        check(sent == mic.len() && nz > 1000, &format!("what the device side records reaches parec ({sent} bytes sent within credit, {nz} non-zero bytes read)"));
        let _ = rec.kill();
        let _ = rec.wait();
        let sp = poll(Duration::from_secs(4), || dev.lock().unwrap().stops.contains(&MIC).then_some(()));
        check(sp.is_some(), "Stop for the microphone once nothing records from it");

        // ---- hot-plug.
        link.send(device(USB, Dir::Out, 2, "USB Headset"));
        let hp = poll(Duration::from_secs(5), || sinks(a, "sinks").iter().any(|x| x == USB_N).then_some(()));
        check(hp.is_some(), "a plugged-in device becomes a new sink");
        link.send(Snd::Gone { stream: USB });
        let gone = poll(Duration::from_secs(5), || (!sinks(a, "sinks").iter().any(|x| x == USB_N)).then_some(()));
        check(gone.is_some(), "and its sink goes when it is unplugged");
        check(sinks(a, "sinks").iter().any(|x| x == PHONES_N) && pactl(a, &["get-default-sink"]).trim() == PHONES_N, "the other devices and the default stay");
        // A second device with the same name gets `_2`.
        link.send(device(5, Dir::Out, 2, "Speakers (Realtek(R) Audio)"));
        let dup = poll(Duration::from_secs(5), || sinks(a, "sinks").iter().any(|x| x == "win.sink.speakers_realtek_r_audio_2").then_some(()));
        check(dup.is_some(), "a second device of the same name is ..._2");
        for s in [SPK, PHONES, MIC, 5] {
            link.send(Snd::Gone { stream: s });
        }
        let none = poll(Duration::from_secs(5), || {
            (!sinks(a, "sinks").iter().chain(sinks(a, "sources").iter()).any(|x| x.starts_with("win."))).then_some(())
        });
        check(none.is_some(), "with every device gone, no win.* sink or source is left");
    }

    // ------------------------------------------------------ links and files --

    /// The host thread: owns its end of the slot, sends what `out` says,
    /// records and answers every request (URLs and `with` opened, paths
    /// refused, so that both answers are seen by wc-host-open).
    fn host(mut ch: ByteChannel, out: mpsc::Receiver<Msg>, seen: Arc<Mutex<Vec<String>>>) {
        let mut lines = LineBuf::default();
        let mut tmp = [0u8; 8192];
        let send = |ch: &mut ByteChannel, m: &Msg| {
            let mut b = m.encode().into_bytes();
            b.push(b'\n');
            let mut r = &b[..];
            while !r.is_empty() {
                match ch.write(r) {
                    Ok(n) if n > 0 => r = &r[n..],
                    Ok(_) => std::thread::sleep(Duration::from_millis(1)),
                    Err(_) => return,
                }
            }
        };
        loop {
            loop {
                match out.try_recv() {
                    Ok(m) => send(&mut ch, &m),
                    Err(mpsc::TryRecvError::Empty) => break,
                    Err(mpsc::TryRecvError::Disconnected) => return,
                }
            }
            match ch.read(&mut tmp) {
                Ok(ReadOutcome::Bytes(n)) => lines.push(&tmp[..n]),
                Ok(ReadOutcome::Eof) | Err(_) => return,
                Ok(_) => std::thread::sleep(Duration::from_millis(2)),
            }
            while let Some(l) = lines.next_line() {
                say(&format!("host <- {l}"));
                seen.lock().unwrap().push(l.clone());
                if let Ok(Msg::Req { n, req }) = Msg::decode(&l) {
                    let result = match req {
                        Req::Path(p) => Err(format!("test refusal: the fake host opens no path ({p})")),
                        _ => Ok(()),
                    };
                    let r = Msg::Rep { n, result };
                    say(&format!("host -> {}", r.encode()));
                    send(&mut ch, &r);
                }
            }
        }
    }

    fn run_open(a: &Args, prog: &Path, args: &[&str]) -> (i32, String) {
        let o = Command::new(prog)
            .args(args)
            .env("WC_HOST_OPEN", &a.opener)
            .env("WC_HOSTINT_SOCK", a.dir.join("hostint.sock"))
            .output()
            .expect("wc-host-open");
        let err = String::from_utf8_lossy(&o.stderr).into_owned();
        say(&format!("{} {args:?} -> exit {:?}, stderr {:?}", prog.display(), o.status.code(), err.trim()));
        (o.status.code().unwrap_or(-1), err)
    }

    fn links_part(a: &Args, owner: &Arc<Control>, linux: &Arc<Control>) {
        let cfg = hostint::Config { dir: a.dir.clone(), opener: Some(a.opener.clone()) };
        let handle = hostint::start(linux, cfg).expect("hostint service");
        let ch = poll(Duration::from_secs(5), || {
            ByteChannel::live_connections(owner).ok()?.into_iter().find(|s| owner.ring_owner(*s).ok() == Some(OWNER_HOSTINT))
        })
        .map(|(s, _)| ByteChannel::open(owner, s, Endpoint::Peer).unwrap())
        .expect("the proxy claimed no OWNER_HOSTINT slot");
        let seen = Arc::new(Mutex::new(Vec::<String>::new()));
        let (tx, rx) = mpsc::channel();
        let s2 = Arc::clone(&seen);
        std::thread::spawn(move || host(ch, rx, s2));
        check(
            poll(Duration::from_secs(3), || seen.lock().unwrap().iter().any(|l| l == "hello 1").then_some(())).is_some(),
            "Linux says `hello 1` on the host-integration slot",
        );
        let share = a.dir.join("share");
        std::fs::create_dir_all(share.join("waycompose-open")).unwrap();
        std::fs::write(share.join("doc.txt"), b"shared\n").unwrap();
        let only = a.dir.join("linux-only.txt");
        std::fs::write(&only, b"linux only\n").unwrap();
        let sl = share.to_string_lossy().into_owned();
        tx.send(Msg::Hello { version: 1, platform: Some("windows".into()) }).unwrap();
        tx.send(Msg::Paths(PathMap {
            shares: vec![("Z:\\".into(), sl.clone())],
            drop: Some(("Z:\\waycompose-open".into(), format!("{sl}/waycompose-open"))),
        }))
        .unwrap();
        tx.send(Msg::Handlers(vec![Handler {
            token: "n0".into(),
            name: "Notepad".into(),
            mimes: vec!["text/plain".into()],
            default_for: vec!["text/plain".into()],
            schemes: vec![],
        }]))
        .unwrap();
        let apps = a.dir.join("hostint/xdg-data/applications");
        let _ = poll(Duration::from_secs(3), || apps.join("wc-host-n0.desktop").exists().then_some(()));

        let before = seen.lock().unwrap().len();
        let (code, _) = run_open(a, &a.opener, &["https://example.org"]);
        let want_line = format!(" url {}", b64(b"https://example.org"));
        check(
            code == 0 && seen.lock().unwrap()[before..].iter().any(|l| l.starts_with("req ") && l.ends_with(&want_line)),
            "wc-host-open https://example.org sends `req N url …` and exits 0 on the host's ok",
        );
        if let Some(shim) = a.shim.clone() {
            let before = seen.lock().unwrap().len();
            let (code, _) = run_open(a, &shim, &["https://example.org/via-xdg-open"]);
            let want_line = format!(" url {}", b64(b"https://example.org/via-xdg-open"));
            check(
                code == 0 && seen.lock().unwrap()[before..].iter().any(|l| l.ends_with(&want_line)),
                "the xdg-open shim (scripts/wc-hostint/xdg-open) is wc-host-open",
            );
        }
        let before = seen.lock().unwrap().len();
        let (code, err) = run_open(a, &a.opener, &[only.to_str().unwrap()]);
        check(code == 2 && err.contains("not in a folder the Windows desktop can see"), "a Linux-only file is refused here, with the reason");
        check(seen.lock().unwrap().len() == before, "and nothing is sent to the host for it");
        let (code, err) = run_open(a, &a.opener, &[share.join("doc.txt").to_str().unwrap()]);
        check(seen.lock().unwrap().iter().any(|l| l.contains(&format!("path {}", b64(b"Z:\\doc.txt")))), "a shared file is sent as its host path Z:\\doc.txt");
        check(code == 1 && err.contains("refused") && err.contains("test refusal"), "the host's refusal is printed and the exit status is 1");
        let (code, _) = run_open(a, &a.opener, &["--with", "n0", only.to_str().unwrap()]);
        let with = seen.lock().unwrap().iter().rev().find_map(|l| match Msg::decode(l) {
            Ok(Msg::Req { req: Req::With { token, target: Target::HostPath(p) }, .. }) if token == "n0" => Some(p),
            _ => None,
        });
        let copied = with.as_ref().and_then(|p| {
            let rel = p.strip_prefix("Z:\\waycompose-open\\")?.replace('\\', "/");
            Some(std::fs::read(share.join("waycompose-open").join(rel)).ok()? == b"linux only\n")
        });
        check(code == 0 && copied == Some(true), &format!("--with n0 on a Linux-only file copies it to the drop directory and sends {with:?}"));
        let e = std::fs::read_to_string(apps.join("wc-host-n0.desktop")).unwrap_or_default();
        check(e.contains("Name=Notepad (Windows)") && e.contains("--with n0 %U"), "Notepad is a desktop entry, Exec= wc-host-open --with n0");
        check(apps.join("wc-host-browser.desktop").exists(), "the host's browser is a desktop entry");
        let m = std::fs::read_to_string(a.dir.join("hostint/xdg-config/mimeapps.list")).unwrap_or_default();
        check(m.contains("x-scheme-handler/https=wc-host-browser.desktop") && m.contains("text/plain=wc-host-n0.desktop"), "mimeapps.list: https to the host browser, text/plain to Notepad");
        let roots = std::fs::read_to_string(a.dir.join("hostint/roots")).unwrap_or_default();
        check(roots.trim() == sl, "the share's Linux root is written for wc-session open");
        handle.stop();
        drop(tx);
    }

    pub fn main() -> std::process::ExitCode {
        let a = args();
        let _ = t0();
        guard(&a, "the pulse socket", &a.pulse);
        guard(&a, "the PipeWire socket", &a.pipewire);
        say(&format!("test servers: pulse {} , PipeWire {} ; run dir {}", a.pulse.display(), a.pipewire.display(), a.dir.display()));
        let g = Geometry::small();
        let r = Arc::new(SharedRegion::heap(g.control_size as usize).unwrap());
        let ids = ProtocolIds::new(1, sv_proto::PROTOCOL_HASH);
        let owner = Arc::new(Control::create(Arc::clone(&r), g, ids, 1).unwrap());
        let linux = Arc::new(Control::attach(r, ids).unwrap());
        say("---- sound ring");
        sound_part(&a, &owner, &linux);
        say("---- links and files");
        links_part(&a, &owner, &linux);
        let _ = std::io::stdout().flush();
        let f = *FAILS.lock().unwrap();
        say(&format!("{f} failure(s)"));
        std::process::ExitCode::from(f.min(255) as u8)
    }
}
