//! §105: the Linux end of host integration, end to end on one machine with no
//! VM. Run by `scripts/wc-hostint-local.sh`, which starts a private PipeWire
//! stack (`wc-session pw-stack`) and passes its socket.
//!
//! A fake host owns an in-memory control region and opens the `OWNER_HOSTINT`
//! slot that `wc_proxy::hostint::start` claims, exactly as the Windows
//! compositor or the macOS main process will. It pushes a device set, moves
//! the default, plugs a device in, and answers `wc-host-open`'s requests.
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
    use std::collections::HashSet;
    use std::io::Read;
    use std::path::{Path, PathBuf};
    use std::process::{Command, Stdio};
    use std::sync::mpsc;
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    use sv_transport::control::{Control, Geometry, ProtocolIds};
    use sv_transport::region::SharedRegion;
    use sv_transport::wlbytes::{ByteChannel, Endpoint, ReadOutcome, OWNER_HOSTINT};
    use wc_hostint::audio::{Device, Dir, State};
    use wc_hostint::wire::{b64, Handler, LineBuf, Msg, PathMap, Req, Target};
    use wc_proxy::hostint;

    struct Args {
        dir: PathBuf,
        pipewire: PathBuf,
        pw_bin: Option<PathBuf>,
        opener: PathBuf,
        pacat: Option<PathBuf>,
        shim: Option<PathBuf>,
    }

    fn args() -> Args {
        let mut a = Args { dir: PathBuf::new(), pipewire: PathBuf::new(), pw_bin: None, opener: PathBuf::new(), pacat: None, shim: None };
        let mut it = std::env::args().skip(1);
        while let Some(k) = it.next() {
            let v = it.next().unwrap_or_default();
            match k.as_str() {
                "--dir" => a.dir = v.into(),
                "--pipewire" => a.pipewire = v.into(),
                "--pw-bin" => a.pw_bin = Some(v.into()),
                "--opener" => a.opener = v.into(),
                "--pacat" => a.pacat = Some(v.into()),
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

    /// A `pw-*` command, its environment checked before it runs: everything
    /// points into the test's own directory and nothing at the user's session.
    fn pw(a: &Args, name: &str, args: &[String]) -> String {
        let mut c = hostint::pw_command(a.pw_bin.as_deref(), name, &a.pipewire);
        c.args(args);
        let mine = a.dir.to_string_lossy().into_owned();
        for (k, v) in c.get_envs() {
            let k = k.to_string_lossy();
            if ["PIPEWIRE_REMOTE", "PIPEWIRE_RUNTIME_DIR", "XDG_RUNTIME_DIR"].contains(&k.as_ref()) {
                let v = v.map(|v| v.to_string_lossy().into_owned()).unwrap_or_default();
                assert!(v.starts_with(&mine) && !v.starts_with("/run/user/"), "REFUSING {name}: {k}={v} is not the test's PipeWire");
            }
        }
        let out = c.stderr(Stdio::null()).output().expect(name);
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    fn nodes(a: &Args) -> (HashSet<String>, String) {
        let j = pw(a, "pw-dump", &[]);
        (wc_hostint::audio::pw_dump_node_names(&j), j)
    }

    fn json_escaped(s: &str) -> String {
        s.chars().map(|c| if c.is_ascii() { c.to_string() } else { format!("\\u{:04x}", c as u32) }).collect()
    }

    fn configured(a: &Args, dir: &str) -> Option<String> {
        let out = pw(a, "pw-metadata", &["-n".into(), "default".into()]);
        let key = format!("key:'default.configured.audio.{dir}'");
        let l = out.lines().find(|l| l.contains(&key))?;
        let v = l.split("value:'").nth(1)?;
        let v = v.split("\"name\":\"").nth(1)?;
        Some(v.split('"').next()?.to_string())
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

    fn dev(dir: Dir, id: &str, d: &str, ch: u16) -> Device {
        Device { dir, id: id.into(), description: d.into(), channels: ch, rate: 48_000 }
    }

    const SPK: &str = "{0.0.0.00000000}.{aaaa}";
    const PHONES: &str = "{0.0.0.00000000}.{bbbb}";
    const MIC: &str = "{0.0.1.00000000}.{cccc}";
    const USB: &str = "{0.0.0.00000000}.{dddd}";

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

    fn host_open(a: &Args, args: &[&str]) -> (i32, String) {
        run_open(a, &a.opener, args)
    }

    fn run_open(a: &Args, prog: &Path, args: &[&str]) -> (i32, String) {
        let o = Command::new(prog)
            .args(args)
            .env("WC_HOST_OPEN", &a.opener)
            .env("WC_HOSTINT_SOCK", a.dir.join("hostint.sock"))
            .output()
            .expect("wc-host-open");
        let err = String::from_utf8_lossy(&o.stderr).into_owned();
        say(&format!("wc-host-open {args:?} -> exit {:?}, stderr {:?}", o.status.code(), err.trim()));
        (o.status.code().unwrap_or(-1), err)
    }

    /// Plays noise into the session's default sink through pipewire-pulse for
    /// a second, while reading `audio.sock`: the default output's PCM on the
    /// existing single sound channel.
    fn monitor_carries_sound(a: &Args) -> Option<usize> {
        let pacat = a.pacat.as_ref()?;
        let pulse = a.dir.join("xdg/pulse/native");
        let mut s = std::os::unix::net::UnixStream::connect(a.dir.join("audio.sock")).ok()?;
        s.set_read_timeout(Some(Duration::from_millis(200))).ok()?;
        let mut child = Command::new(pacat)
            .args(["--raw", "--format=s16le", "--rate=48000", "--channels=2", "/dev/urandom"])
            .env("PULSE_SERVER", format!("unix:{}", pulse.display()))
            .env_remove("PIPEWIRE_REMOTE")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .ok()?;
        let mut nonzero = 0usize;
        let mut buf = [0u8; 4096];
        let end = Instant::now() + Duration::from_millis(1500);
        while Instant::now() < end {
            if let Ok(n) = s.read(&mut buf) {
                nonzero += buf[..n].iter().filter(|b| **b != 0).count();
            }
        }
        let _ = child.kill();
        let _ = child.wait();
        Some(nonzero)
    }

    pub fn main() -> std::process::ExitCode {
        let a = args();
        let _ = t0();
        assert!(!a.pipewire.starts_with("/run/user"), "this test never uses a session's own PipeWire");
        say(&format!("test PipeWire {} ; run dir {}", a.pipewire.display(), a.dir.display()));

        // The control region, as the host's process owns it, and the proxy's view.
        let g = Geometry::small();
        let r = Arc::new(SharedRegion::heap(g.control_size as usize).unwrap());
        let ids = ProtocolIds::new(1, sv_proto::PROTOCOL_HASH);
        let owner = Control::create(Arc::clone(&r), g, ids, 1).unwrap();
        let linux = Arc::new(Control::attach(r, ids).unwrap());
        let cfg = hostint::Config {
            dir: a.dir.clone(),
            pipewire: Some(a.pipewire.clone()),
            pw_bin: a.pw_bin.clone(),
            opener: Some(a.opener.clone()),
        };
        let handle = hostint::start(&linux, cfg).expect("service");

        let ch = poll(Duration::from_secs(5), || {
            ByteChannel::live_connections(&owner)
                .ok()?
                .into_iter()
                .find(|s| owner.ring_owner(*s).ok() == Some(OWNER_HOSTINT))
        })
        .map(|(s, _)| ByteChannel::open(&owner, s, Endpoint::Peer).unwrap())
        .expect("the proxy claimed no OWNER_HOSTINT slot");
        let seen = Arc::new(Mutex::new(Vec::<String>::new()));
        let (tx, rx) = mpsc::channel();
        let s2 = Arc::clone(&seen);
        std::thread::spawn(move || host(ch, rx, s2));
        check(
            poll(Duration::from_secs(3), || seen.lock().unwrap().iter().any(|l| l == "hello 1").then_some(())).is_some(),
            "Linux says `hello 1` on the slot",
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

        // ---- audio: two outputs and a microphone.
        let mut st = State {
            devices: vec![
                dev(Dir::Output, SPK, "Speakers (Realtek(R) Audio)", 2),
                dev(Dir::Output, PHONES, "Kopfhörer (WH-1000XM5 Stereo)", 2),
                dev(Dir::Input, MIC, "Microphone Array (Intel® Smart Sound Technology for Digital Microphones)", 1),
            ],
            default_output: Some(SPK.into()),
            default_input: Some(MIC.into()),
        };
        let want = [
            ("win.sink.speakers_realtek_r_audio", "Speakers (Realtek(R) Audio)"),
            ("win.sink.kopfh_rer_wh-1000xm5_stereo", "Kopfhörer (WH-1000XM5 Stereo)"),
            (
                "win.source.microphone_array_intel_smart_sound_technology_for_digital_microphones",
                "Microphone Array (Intel® Smart Sound Technology for Digital Microphones)",
            ),
        ];
        let sent = Instant::now();
        tx.send(Msg::Audio(st.clone())).unwrap();
        let up = poll(Duration::from_secs(10), || {
            let (n, j) = nodes(&a);
            want.iter().all(|(w, _)| n.contains(*w)).then_some(j)
        });
        check(up.is_some(), "three host devices appear as three PipeWire nodes");
        if let Some((j, _)) = &up {
            say(&format!("nodes up {} ms after the push", sent.elapsed().as_millis()));
            for (name, desc) in want {
                let ok = j.contains(&format!("\"node.description\": \"{desc}\"")) || j.contains(&format!("\"node.description\": \"{}\"", json_escaped(desc)));
                check(j.contains(name) && ok, &format!("node {name} described as {desc:?}"));
            }
            check(j.contains("\"media.class\": \"Audio/Source\""), "the microphone is an Audio/Source");
        }
        let d = poll(Duration::from_secs(3), || {
            (configured(&a, "sink").as_deref() == Some("win.sink.speakers_realtek_r_audio")).then_some(())
        });
        check(d.is_some(), "default.configured.audio.sink is the host's default output");
        check(
            configured(&a, "source").as_deref() == Some(want[2].0),
            "default.configured.audio.source is the host's default input",
        );

        // ---- the host's default output changes.
        st.default_output = Some(PHONES.into());
        tx.send(Msg::Audio(st.clone())).unwrap();
        let sw = poll(Duration::from_secs(3), || {
            (configured(&a, "sink").as_deref() == Some("win.sink.kopfh_rer_wh-1000xm5_stereo")).then_some(())
        });
        match sw {
            Some((_, t)) => {
                say(&format!("default switch seen by pw-metadata after {} ms", t.as_millis()));
                check(t <= Duration::from_secs(1), "a default change reaches pw-metadata within 1 s");
            }
            None => check(false, "a default change reaches pw-metadata within 1 s"),
        }
        say(&format!("pw-metadata -n default:\n{}", pw(&a, "pw-metadata", &["-n".into(), "default".into()])));

        // ---- the default output's sound on the single existing channel.
        match monitor_carries_sound(&a) {
            Some(n) => check(n > 10_000, &format!("audio.sock carries the default output's sound ({n} non-zero bytes in 1.5 s)")),
            None => say("no pacat: the monitor check is skipped"),
        }

        // ---- hot-plug: a USB headset.
        st.devices.push(dev(Dir::Output, USB, "USB Headset", 2));
        tx.send(Msg::Audio(st.clone())).unwrap();
        let hp = poll(Duration::from_secs(10), || {
            let (n, _) = nodes(&a);
            (n.contains("win.sink.usb_headset") && want.iter().all(|(w, _)| n.contains(*w))).then_some(())
        });
        check(hp.is_some(), "a plugged-in device becomes a fourth node, the others stay");
        let kept = poll(Duration::from_secs(3), || {
            (configured(&a, "sink").as_deref() == Some("win.sink.kopfh_rer_wh-1000xm5_stereo")).then_some(())
        });
        check(kept.is_some(), "the default survives the restart for the new device set");

        // ---- links and files.
        let before = seen.lock().unwrap().len();
        let (code, _) = host_open(&a, &["https://example.org"]);
        let got = seen.lock().unwrap()[before..].to_vec();
        let want_line = format!(" url {}", b64(b"https://example.org"));
        check(got.iter().any(|l| l.starts_with("req ") && l.ends_with(&want_line)), "wc-host-open https://example.org sends `req N url …` to the host");
        check(code == 0, "and exits 0 on the host's ok");

        if let Some(shim) = a.shim.clone() {
            let before = seen.lock().unwrap().len();
            let (code, _) = run_open(&a, &shim, &["https://example.org/via-xdg-open"]);
            let want_line = format!(" url {}", b64(b"https://example.org/via-xdg-open"));
            check(
                code == 0 && seen.lock().unwrap()[before..].iter().any(|l| l.ends_with(&want_line)),
                "the xdg-open shim (scripts/wc-hostint/xdg-open) is wc-host-open",
            );
        }

        let before = seen.lock().unwrap().len();
        let (code, err) = host_open(&a, &[only.to_str().unwrap()]);
        check(code == 2 && err.contains("not in a folder the Windows desktop can see"), "a Linux-only file is refused here, with the reason");
        check(seen.lock().unwrap().len() == before, "and nothing is sent to the host for it");

        let (code, err) = host_open(&a, &[share.join("doc.txt").to_str().unwrap()]);
        check(
            seen.lock().unwrap().iter().any(|l| Msg::decode(l).ok() == Some(Msg::Req { n: 0, req: Req::Path("Z:\\doc.txt".into()) }) || l.contains(&b64(b"Z:\\doc.txt"))),
            "a shared file is sent as its host path Z:\\doc.txt",
        );
        check(code == 1 && err.contains("refused") && err.contains("test refusal"), "the host's refusal is printed and the exit status is 1");

        let (code, _) = host_open(&a, &["--with", "n0", only.to_str().unwrap()]);
        let with = seen.lock().unwrap().iter().rev().find_map(|l| match Msg::decode(l) {
            Ok(Msg::Req { req: Req::With { token, target: Target::HostPath(p) }, .. }) if token == "n0" => Some(p),
            _ => None,
        });
        let copied = with.as_ref().and_then(|p| {
            let rel = p.strip_prefix("Z:\\waycompose-open\\")?.replace('\\', "/");
            let f = share.join("waycompose-open").join(rel);
            Some(std::fs::read(&f).ok()? == b"linux only\n")
        });
        check(code == 0 && copied == Some(true), &format!("--with n0 on a Linux-only file copies it to the drop directory and sends {with:?}"));

        let apps = a.dir.join("hostint/xdg-data/applications");
        let e = std::fs::read_to_string(apps.join("wc-host-n0.desktop")).unwrap_or_default();
        check(e.contains("Name=Notepad (Windows)") && e.contains("--with n0 %U"), "Notepad is a desktop entry, Exec= wc-host-open --with n0");
        check(apps.join("wc-host-browser.desktop").exists(), "the host's browser is a desktop entry");
        let m = std::fs::read_to_string(a.dir.join("hostint/xdg-config/mimeapps.list")).unwrap_or_default();
        check(m.contains("x-scheme-handler/https=wc-host-browser.desktop") && m.contains("text/plain=wc-host-n0.desktop"), "mimeapps.list: https to the host browser, text/plain to Notepad");
        let roots = std::fs::read_to_string(a.dir.join("hostint/roots")).unwrap_or_default();
        check(roots.trim() == sl, "the share's Linux root is written for wc-session open");

        // ---- every device unplugged: the nodes go.
        tx.send(Msg::Audio(State::default())).unwrap();
        let gone = poll(Duration::from_secs(5), || {
            let (n, _) = nodes(&a);
            (!n.iter().any(|x| x.starts_with("win."))).then_some(())
        });
        check(gone.is_some(), "with no host devices the nodes are gone");

        handle.stop();
        drop(tx);
        let f = *FAILS.lock().unwrap();
        say(&format!("{} failure(s)", f));
        std::process::ExitCode::from(f.min(255) as u8)
    }
}
