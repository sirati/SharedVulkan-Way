//! `wc-hostint-probe`: what host integration would tell Linux about this
//! Windows, printed, and nothing changed. Lists the active audio endpoints
//! and the two console defaults, and the applications per type. Plays and
//! records nothing, changes no default and writes no registry key unless
//! asked:
//!
//!   wc-hostint-probe                       endpoints, defaults, handlers
//!   wc-hostint-probe --watch SECS          also print each audio change
//!   wc-hostint-probe --reg-test DIR        register a test application under
//!                                          HKCU, read it back, uninstall it
//!                                          through the manifest in DIR, and
//!                                          check that nothing is left
//!   wc-hostint-probe --render-test ID MS   play SILENCE to one endpoint
//!   wc-hostint-probe --capture-test ID MS  record from one (turns a mic on)

#[cfg(windows)]
fn main() -> std::process::ExitCode {
    win::main()
}

#[cfg(not(windows))]
fn main() -> std::process::ExitCode {
    eprintln!("wc-hostint-probe is a Windows program");
    std::process::ExitCode::from(2)
}

#[cfg(windows)]
mod win {
    use std::process::ExitCode;
    use std::time::{Duration, Instant};

    use wc_hostint::assoc::{self, LinuxApp, RegData};
    use wc_hostint::audio::{plan, State};
    use wc_hostplat::win::{audio, handlers, registry};

    fn print_state(s: &State) {
        for d in &s.devices {
            let mark = if Some(&d.id) == s.default_output.as_ref() || Some(&d.id) == s.default_input.as_ref() {
                " (default)"
            } else {
                ""
            };
            println!("endpoint {} {:?} {} ch {} Hz{} id={}", d.dir.wire(), d.description, d.channels, d.rate, mark, d.id);
        }
        println!(
            "count: {} output(s), {} input(s)",
            s.devices.iter().filter(|d| d.dir == wc_hostint::audio::Dir::Output).count(),
            s.devices.iter().filter(|d| d.dir == wc_hostint::audio::Dir::Input).count()
        );
        for n in plan(s, "win") {
            println!("node {} stream {} {}ch {}Hz", n.node_name, n.stream, n.channels, n.rate);
        }
    }

    pub fn main() -> ExitCode {
        let args: Vec<String> = std::env::args().skip(1).collect();
        if let Some(i) = args.iter().position(|a| a == "--reg-test") {
            return reg_test(args.get(i + 1).map(String::as_str).unwrap_or("."));
        }
        if let Some(i) = args.iter().position(|a| a == "--render-test" || a == "--capture-test") {
            let id = args.get(i + 1).cloned().unwrap_or_default();
            let ms: u64 = args.get(i + 2).and_then(|s| s.parse().ok()).unwrap_or(1000);
            return stream_test(args[i] == "--render-test", &id, ms);
        }
        let t = Instant::now();
        match audio::enumerate() {
            Ok(s) => print_state(&s),
            Err(e) => println!("audio: {e}"),
        }
        println!("audio enumerated in {} ms", t.elapsed().as_millis());
        let t = Instant::now();
        for f in handlers::found() {
            println!("type {:?}: {} recommended, default {:?}", f.key, f.apps.len(), f.default.as_deref().map(base));
            for a in &f.apps {
                println!("    {:?} {}", a.name, base(&a.id));
            }
        }
        let hs = handlers::handlers();
        for h in &hs {
            println!(
                "handler {} {:?} mimes={} schemes={} default_for={} exe={}",
                h.offer.token,
                h.offer.name,
                h.offer.mimes.len(),
                h.offer.schemes.join(","),
                h.offer.default_for.join(","),
                base(&h.app)
            );
        }
        println!("{} handler(s) in {} ms", hs.len(), t.elapsed().as_millis());
        if let Some(i) = args.iter().position(|a| a == "--watch") {
            let secs: u64 = args.get(i + 1).and_then(|s| s.parse().ok()).unwrap_or(30);
            let (tx, rx) = std::sync::mpsc::channel();
            let w = audio::watch(tx, |l| println!("{l}"));
            match w {
                Ok(_w) => {
                    let end = Instant::now() + Duration::from_secs(secs);
                    while let Some(left) = end.checked_duration_since(Instant::now()) {
                        if let Ok(wc_hostint::host::Event::Audio(s)) = rx.recv_timeout(left) {
                            println!("-- audio state at {} ms", t.elapsed().as_millis());
                            print_state(&s);
                        }
                    }
                }
                Err(e) => println!("watch: {e}"),
            }
        }
        ExitCode::SUCCESS
    }

    /// The executable's file name only: paths can carry user names.
    fn base(p: &str) -> &str {
        p.rsplit(['\\', '/']).next().unwrap_or(p)
    }

    /// Render SILENCE (all-zero samples) to one endpoint, or capture from
    /// one, for `ms` at 48 kHz stereo, and print the stream's counters.
    /// Not for a machine somebody is using: a capture turns the microphone
    /// on.
    fn stream_test(render: bool, id: &str, ms: u64) -> ExitCode {
        use std::io::{Read, Write};
        use wc_hostplat::win::stream::{Capture, Format, Render, Sizing};
        let fmt = Format { rate: 48_000, channels: 2 };
        let t = Instant::now();
        if render {
            let r = match Render::start(id, fmt, Sizing::default(), |l| println!("{l}")) {
                Ok(r) => r,
                Err(e) => {
                    println!("render: {e}");
                    return ExitCode::from(1);
                }
            };
            let mut w = r.writer();
            let chunk = vec![0u8; 480 * 4];
            while t.elapsed() < Duration::from_millis(ms) {
                if w.write(&chunk).is_err() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            println!("render: {}", r.stats().line(fmt.rate));
        } else {
            let c = match Capture::start(id, fmt, Sizing::default(), |l| println!("{l}")) {
                Ok(c) => c,
                Err(e) => {
                    println!("capture: {e}");
                    return ExitCode::from(1);
                }
            };
            let mut rd = c.reader();
            rd.set_timeout(Some(Duration::from_millis(200)));
            let mut buf = vec![0u8; 4096];
            let mut got = 0usize;
            while t.elapsed() < Duration::from_millis(ms) {
                match rd.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => got += n,
                    Err(_) => {}
                }
            }
            println!("capture: {got} bytes read; {}", c.stats().line(fmt.rate));
        }
        ExitCode::SUCCESS
    }

    fn reg_test(dir: &str) -> ExitCode {
        let app = LinuxApp {
            section: "probe".into(),
            name: "Probe".into(),
            mimes: vec!["text/markdown".into()],
            schemes: vec!["https".into(), "wcprobe-test".into()],
        };
        let launcher = std::env::current_exe().map(|e| e.display().to_string()).unwrap_or_default();
        let p = assoc::windows_plan(&app, &launcher, &launcher, "wcprobe", &registry::scheme_exists);
        println!("scheme_exists(https)={} scheme_exists(wcprobe-test)={}", registry::scheme_exists("https"), registry::scheme_exists("wcprobe-test"));
        let manifest = std::path::Path::new(dir).join("wcprobe-assoc.manifest");
        if let Err(e) = std::fs::write(&manifest, assoc::manifest(&p.undo)) {
            println!("manifest: {e}");
            return ExitCode::from(1);
        }
        let mut ok = true;
        match registry::apply(&p) {
            Ok(n) => println!("applied {n} value(s)"),
            Err(e) => {
                println!("apply: {e}");
                ok = false;
            }
        }
        let mut present = 0;
        for v in &p.values {
            let got = registry::read(&v.key, &v.name);
            let want = match &v.data {
                RegData::Str(s) => Some(s.clone()),
                RegData::Empty => Some(String::new()),
            };
            if got == want {
                present += 1;
            } else {
                println!("MISMATCH {}\\{:?}: {got:?}", v.key, v.name);
                ok = false;
            }
        }
        println!("read back {present}/{} value(s)", p.values.len());
        let text = std::fs::read_to_string(&manifest).unwrap_or_default();
        let undo = assoc::parse_manifest(&text);
        match registry::remove(&undo) {
            Ok(n) => println!("removed {n} item(s)"),
            Err(e) => {
                println!("remove: {e:?}");
                ok = false;
            }
        }
        let mut left = 0;
        for v in &p.values {
            if registry::read(&v.key, &v.name).is_some() && (v.key.contains("Waycompose") || v.name.contains("Waycompose") || v.key.ends_with("wcprobe-test") || v.key.contains("wcprobe-test\\")) {
                println!("LEFT {}\\{:?}", v.key, v.name);
                left += 1;
            }
        }
        for k in ["Software\\Waycompose\\wcprobe", "Software\\Classes\\wcprobe-test"] {
            if registry::read(k, "").is_some() || registry::key_exists(k) {
                println!("LEFT key {k}");
                left += 1;
            }
        }
        let _ = std::fs::remove_file(&manifest);
        println!("after uninstall: {left} item(s) left");
        if ok && left == 0 { ExitCode::SUCCESS } else { ExitCode::from(1) }
    }
}
