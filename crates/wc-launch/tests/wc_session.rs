//! `scripts/wc-session`, the forced command of the guest's key, run with the
//! requests a hostile holder of that key could send. docs/waycompose-design.md
//! §94, docs/security-model.md R33.
//!
//! # Why this is a test
//!
//! Until §94 `wc-session run` took a base64'd command line from the guest and
//! ran it, so whoever could read the key on the Windows guest could run any
//! program on the VM host as its user. The fix is a set of refusals — an
//! application is a section of the host's `apps.ini`, every word of the request
//! is checked against what its verb takes — and a refusal nobody exercises can
//! be softened by accident (the argument of `sv-testkit`'s `kill_safety`).
//!
//! So each test here runs the real script, in a directory of its own, as the
//! guest reaches it: the request in `$SSH_ORIGINAL_COMMAND` and nothing in
//! argv. The pieces it calls are stand-ins that write down what they were
//! given — the application, `wc-proxy desktop` — except the profile renderer,
//! which is this crate's own binary, as on the host. `$WC_LAUNCH_ROOTC`, the
//! command the script pipes its root steps to, is set to one that leaves a
//! file behind, so a request that reached root at all is seen.
//!
//! What is asserted for every hostile request: the script exits non-zero and
//! says `wc-session:` why; the application never ran; nothing ran as "root";
//! and no file named by the request appeared.
#![cfg(unix)]

use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

fn b64(bytes: &[u8]) -> String {
    const A: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for c in bytes.chunks(3) {
        let b = [c[0], *c.get(1).unwrap_or(&0), *c.get(2).unwrap_or(&0)];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        for i in 0..4 {
            out.push(if i <= c.len() { A[(n >> (18 - 6 * i)) as usize & 63] as char } else { '=' });
        }
    }
    out
}

/// One host: a base directory with `apps.ini` and `bin/`, a home, a run root
/// with one live session (`t94`, its `wl-proxy` a bound socket) and the
/// stand-ins.
struct Host {
    root: PathBuf,
    _proxy: UnixListener,
}

fn script() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../scripts/wc-session")
}

fn write_exe(path: &Path, body: &str) {
    std::fs::write(path, body).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

impl Host {
    fn new(tag: &str) -> Host {
        let root = std::env::temp_dir().join(format!("wc-session-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        for d in ["base/bin", "home/.config/gtk-3.0", "run/t94", "out"] {
            std::fs::create_dir_all(root.join(d)).unwrap();
        }
        let r = root.display().to_string();
        std::os::unix::fs::symlink(env!("CARGO_BIN_EXE_wc-launch"), root.join("base/bin/wc-launch")).unwrap();
        // `wc-proxy desktop`: the argv it got, and the answer the real one
        // gives for an entry whose program is `app` with one action.
        write_exe(
            &root.join("base/bin/wc-proxy"),
            &format!(
                "#!/bin/sh\nprintf '%s\\n' \"$@\" >{r}/out/wc-proxy.argv\n\
                 [ \"$1\" = desktop ] || exit 2; shift\n\
                 [ \"$1\" = --dirs ] && shift 2\n\
                 case \"$1\" in\n\
                   info) echo 'desktop /x/app.desktop'; echo 'program app'; echo 'url yes' ;;\n\
                   action) [ \"$3\" = new-window ] || {{ echo \"no action $3\" >&2; exit 1; }}\n\
                           echo 'program app'; echo 'args --new-window' ;;\n\
                   open) echo 'program app'; echo \"args $3\" ;;\n\
                   *) exit 2 ;;\n\
                 esac\n"
            ),
        );
        // The locator, so that the guest-RAM `up` would get as far as the
        // script it pipes to $ROOTC if the request were let through.
        write_exe(&root.join("base/bin/wc-arena-host"), "#!/bin/sh\nexit 0\n");
        // The application: what it was started with, and its environment.
        write_exe(
            &root.join("app"),
            &format!("#!/bin/sh\nenv >{r}/out/app.env\n: >{r}/out/app.argv.tmp; for a in \"$@\"; do printf '%s\\n' \"$a\" >>{r}/out/app.argv.tmp; done\nmv {r}/out/app.argv.tmp {r}/out/app.argv\n"),
        );
        std::fs::write(root.join("home/.config/gtk-3.0/settings.ini"), "[Settings]\ngtk-decoration-layout=menu:close\n").unwrap();
        let host = Host { _proxy: UnixListener::bind(root.join("run/t94/wl-proxy")).unwrap(), root };
        host.apps(&format!(
            "# test\n[app]\nexec     = {r}/app\nargs     = --fixed one\nprofile  = {r}/home/.wc-launch/p\n\
             prefs    = middlemouse.paste=false\ntitlebar = tabs\ndesktop  = app\ngpu      = on\nenv      = WC_TEST_ENV=1\n\
             [plain]\nexec = {r}/app\ntitlebar = none\n"
        ));
        host
    }

    fn apps(&self, text: &str) {
        std::fs::write(self.root.join("base/apps.ini"), text).unwrap();
    }

    fn p(&self, rel: &str) -> PathBuf {
        self.root.join(rel)
    }

    fn cmd(&self) -> Command {
        let mut c = Command::new("bash");
        c.arg(script())
            .current_dir(&self.root)
            .env("HOME", self.p("home"))
            .env("XDG_CONFIG_HOME", self.p("home/.config"))
            .env("WC_LAUNCH_BASE", self.p("base"))
            .env("WC_LAUNCH_RUN", self.p("run"))
            .env("WC_LAUNCH_ARENA", self.p("no-arena"))
            .env("WC_LAUNCH_ROOTC", format!("touch {}", self.p("out/ROOTED").display()))
            .env_remove("WC_LAUNCH_GUEST_RAM")
            .env_remove("WC_LAUNCH_APPS")
            .env_remove("SSH_ORIGINAL_COMMAND");
        c
    }

    /// The request as the guest sends it. (exit ok, stdout, stderr)
    fn guest(&self, request: &str) -> (bool, String, String) {
        let out = self.cmd().env("SSH_ORIGINAL_COMMAND", request).output().expect("run bash");
        (
            out.status.success(),
            String::from_utf8_lossy(&out.stdout).into_owned(),
            String::from_utf8_lossy(&out.stderr).into_owned(),
        )
    }

    fn operator(&self, args: &[&str]) -> (bool, String, String) {
        let out = self.cmd().args(args).output().expect("run bash");
        (
            out.status.success(),
            String::from_utf8_lossy(&out.stdout).into_owned(),
            String::from_utf8_lossy(&out.stderr).into_owned(),
        )
    }

    fn app_argv(&self) -> Option<Vec<String>> {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Ok(t) = std::fs::read_to_string(self.p("out/app.argv")) {
                return Some(t.lines().map(str::to_string).collect());
            }
            if Instant::now() > deadline {
                return None;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// The request is refused, and nothing it could have wanted happened.
    fn refused(&self, request: &str) {
        let (ok, so, se) = self.guest(request);
        assert!(!ok, "ACCEPTED {request:?}\nstdout: {so}\nstderr: {se}");
        assert!(se.contains("wc-session:"), "{request:?} failed without saying why: {se}");
        std::thread::sleep(Duration::from_millis(30));
        assert!(!self.p("out/app.argv").exists(), "{request:?} started the application");
        assert!(!self.p("out/ROOTED").exists(), "{request:?} reached the root step");
        for f in ["CANARY", "out/CANARY", "home/CANARY"] {
            assert!(!self.p(f).exists(), "{request:?} created {f}");
        }
    }
}

impl Drop for Host {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

// ----------------------------------------------------------- what works ---

/// §94: the guest names the application and two numbers; the argv, the
/// profile's files, the title bar and the environment are the definition's.
#[test]
fn run_starts_the_defined_application_with_the_definition_s_argv_and_files() {
    let h = Host::new("run");
    let (ok, so, se) = h.guest("run t94 app 144 45");
    assert!(ok, "{so}{se}");
    assert!(so.contains("run ok") && so.contains("titlebar=tabs"), "{so}");
    let prof = h.p("home/.wc-launch/p").display().to_string();
    assert_eq!(h.app_argv().expect("the application ran"), ["--profile", &prof, "--fixed", "one"]);
    let js = std::fs::read_to_string(h.p("home/.wc-launch/p/user.js")).unwrap();
    assert!(js.contains("user_pref(\"middlemouse.paste\", false);"), "{js}");
    assert!(js.contains("user_pref(\"ui.GtkCSDCloseButton\", 0);"), "{js}");
    assert!(js.contains("toolkit.startup.max_resumed_crashes"), "{js}");
    // 144 dpi: Windows' buttons are 219 px wide; the caption the guest measured is 45.
    let css = std::fs::read_to_string(h.p("home/.wc-launch/p/chrome/userChrome.css")).unwrap();
    assert!(css.contains("min-width: 219px") && css.contains("--tab-min-height: 43px"), "{css}");
    let env = std::fs::read_to_string(h.p("out/app.env")).unwrap();
    for want in ["MOZ_GTK_TITLEBAR_DECORATION=client", "WC_TEST_ENV=1", "MOZ_ENABLE_WAYLAND=1"] {
        assert!(env.lines().any(|l| l == want), "{want} not in\n{env}");
    }
    let gtk = std::fs::read_to_string(h.p("run/t94/xdg-config/gtk-3.0/gtk.css")).unwrap();
    assert!(gtk.contains("padding-right: 219px"), "{gtk}");
}

/// An application whose definition opts out gets `none`, no sheet, and no
/// `--profile`; an old sheet of ours in a profile is not left behind.
#[test]
fn a_definition_without_a_profile_or_tabs_gets_neither() {
    let h = Host::new("plain");
    let (ok, so, se) = h.guest("run t94 plain 96 0");
    assert!(ok, "{so}{se}");
    assert_eq!(h.app_argv().expect("ran"), Vec::<String>::new());
    let env = std::fs::read_to_string(h.p("out/app.env")).unwrap();
    assert!(env.lines().any(|l| l == "MOZ_GTK_TITLEBAR_DECORATION=none"), "{env}");
    assert!(!env.contains("/xdg-config"), "no GTK overlay without tabs:\n{env}");
}

/// A jump-list task: the action id goes to `wc-proxy desktop` after its
/// options, and the entry's argv follows the definition's.
#[test]
fn action_runs_the_entry_s_action_after_the_definition_s_argv() {
    let h = Host::new("action");
    let (ok, so, se) = h.guest("action t94 app new-window 96 0");
    assert!(ok, "{so}{se}");
    let prof = h.p("home/.wc-launch/p").display().to_string();
    assert_eq!(h.app_argv().expect("ran"), ["--profile", &prof, "--fixed", "one", "--new-window"]);
    let wp: Vec<String> = std::fs::read_to_string(h.p("out/wc-proxy.argv")).unwrap().lines().map(String::from).collect();
    assert_eq!((wp[0].as_str(), wp[1].as_str()), ("desktop", "--dirs"));
    assert_eq!(wp[3..], ["action", "app", "new-window"]);
}

/// A frequent page: the URL is one argument, character for character —
/// `?`, `*`, `[` and `&` are not patterns and not separators.
#[test]
fn open_passes_the_url_as_one_argument_unexpanded() {
    let h = Host::new("open");
    std::fs::write(h.p("aq"), "").unwrap();
    let url = "https://example.org/a?q=[a]*&x=%41#f";
    let (ok, so, se) = h.guest(&format!("open t94 app {} 96 0", b64(url.as_bytes())));
    assert!(ok, "{so}{se}");
    assert_eq!(h.app_argv().expect("ran").last().map(String::as_str), Some(url));
}

#[test]
fn frequent_reads_only_the_definition_s_profile() {
    let h = Host::new("freq");
    let (ok, so, se) = h.guest("frequent app");
    assert!(ok, "{so}{se}");
    assert_eq!(so.trim(), "prefs none", "no places.sqlite yet");
    let (ok, so, _) = h.guest("frequent plain");
    assert!(ok && so.trim() == "prefs none", "{so}");
}

/// The operator's verbs work from argv and are refused to the guest.
#[test]
fn the_operator_verbs_are_not_the_guest_s() {
    let h = Host::new("op");
    let (ok, so, se) = h.operator(&["apps"]);
    assert!(ok && so.contains("apps ok 2"), "{so}{se}");
    h.refused("apps");
    h.refused("resolve sh");
    h.refused("resolve bash");
}

// ------------------------------------------------- what is refused, §94 ---

/// The form `run` had until §94, and what it was for.
#[test]
fn a_command_line_is_refused_in_every_shape() {
    let h = Host::new("old");
    let r = h.root.display().to_string();
    let cmd = b64(format!("/bin/sh -c 'touch {r}/CANARY'").as_bytes());
    let prof = b64(format!("{r}/home/.wc-launch/p").as_bytes());
    let js = b64(b"user_pref(\"x\", 1);");
    for req in [
        format!("run t94 {cmd}"),
        format!("run t94 {cmd} - - -"),
        format!("run t94 {cmd} {prof} {js} {}", b64(b"client")),
        format!("run t94 {cmd} {prof} {js} {} {} {}", b64(b"client"), b64(b"x"), b64(b"y")),
        // The application id as a base64'd program, in the new shape.
        format!("run t94 {} 96 0", b64(b"/bin/sh").trim_end_matches('=')),
        format!("run t94 {} 96 0", b64(b"sh")),
        "run t94 sh 96 0".into(),
        "run t94 bash 96 0".into(),
        // The old `action` and `open`, which also carried the executable.
        format!("action t94 app new-window {} {prof} {js} {}", b64(b"/bin/sh"), b64(b"client")),
        format!("open t94 app {} {} {prof} {js} {}", b64(b"https://e.org/"), b64(b"/bin/sh"), b64(b"client")),
        // `app` and `icon` with the executable they used to take.
        format!("app app {} light", b64(b"/bin/sh")),
        format!("icon app {}", b64(b"/bin/sh")),
        format!("frequent {prof}"),
    ] {
        h.refused(&req);
    }
}

#[test]
fn shell_metacharacters_are_refused_before_any_verb() {
    let h = Host::new("meta");
    let r = h.root.display().to_string();
    for req in [
        format!("run t94 app 96 0; touch {r}/CANARY"),
        format!("run t94 app 96 0 && touch {r}/CANARY"),
        "run t94 app 96 0 | sh".to_string(),
        format!("run t94 $(touch {r}/CANARY) 96 0"),
        format!("run t94 `touch {r}/CANARY` 96 0"),
        format!("run t94 app 96 0\ntouch {r}/CANARY"),
        "run t94 app 96 0\r".into(),
        "run\tt94 app 96 0".into(),
        "run t94 app* 96 0".into(),
        "run t94 ap? 96 0".into(),
        "run t94 [a]pp 96 0".into(),
        "run t94 ~ 96 0".into(),
        "run t94 'app' 96 0".into(),
        "run t94 \"app\" 96 0".into(),
        "run t94 app 96 0 > /tmp/x".into(),
        "run t94 ${HOME} 96 0".into(),
        "status t94\\".into(),
        "run t94 äpp 96 0".into(),
        "status t94\0x".replace('\0', "\u{1}"),
    ] {
        h.refused(&req);
    }
}

#[test]
fn dot_dot_and_paths_are_refused() {
    let h = Host::new("dots");
    for req in [
        "run t94 .. 96 0",
        "run t94 a..b 96 0",
        "run t94 ../app 96 0",
        "run t94 /app 96 0",
        "run .. app 96 0",
        "run t94/.. app 96 0",
        "status ..",
        "status ../..",
        "logs ..",
        "down ..",
        "down ../../tmp",
        "frequent ..",
        "frequent ../p",
        "icon ..",
        "icon ../../etc/passwd",
        "app ../x light",
        "app .app light",
        "gpu .. 1",
        "audio ..",
    ] {
        h.refused(req);
    }
    assert!(h.p("run/t94/wl-proxy").exists(), "a refused down removed the session");
}

#[test]
fn dash_leading_words_are_refused() {
    let h = Host::new("dash");
    for req in [
        "run -t94 app 96 0",
        "run t94 -app 96 0",
        "run t94 --dirs 96 0",
        "run t94 app -96 0",
        "run t94 app 96 -1",
        "action t94 app --new-window 96 0",
        "action t94 app -new-window 96 0",
        "status -rf",
        "down --help",
        "logs -f",
        "frequent -x",
        "icon --dirs",
        "app -x light",
        "app app --dark",
        "gpu -x 1",
        "gpu t94 -1",
        "audio --raw",
        "proxy -t94",
        "up -t94 0x1 ivshmem",
    ] {
        h.refused(req);
    }
}

#[test]
fn numbers_outside_their_range_are_refused() {
    let h = Host::new("num");
    for req in [
        "run t94 app 96000 0",
        "run t94 app 47 0",
        "run t94 app 961 0",
        "run t94 app 010 0",
        "run t94 app 0x60 0",
        "run t94 app 96 513",
        "run t94 app 96 00",
        "run t94 app 96 1e3",
        "run t94 app 96",
        "gpu t94 9",
        "gpu t94 0",
    ] {
        h.refused(req);
    }
}

#[test]
fn urls_that_are_not_http_or_file_or_not_one_word_are_refused() {
    let h = Host::new("url");
    for url in [
        "--new-window",
        "-private",
        "--dirs",
        "javascript:alert(1)",
        "data:text/html,x",
        "file:/etc/passwd",
        "about:config",
        "chrome://browser/content/browser.xhtml",
        "https://e.org/ x",
        "https://e.org/\nx",
        "https://e.org/\"",
        "https://e.org/\\",
        "https://e.org/\u{e4}",
        "https://e.org/\u{7}",
    ] {
        h.refused(&format!("open t94 app {} 96 0", b64(url.as_bytes())));
    }
    h.refused(&format!("open t94 app {} 96 0", b64(format!("https://e.org/{}", "a".repeat(4096)).as_bytes())));
    h.refused("open t94 app not!base64 96 0");
    h.refused("open t94 app aHR0cHM6Ly9lLm9yZy8 96 0 extra");
}

#[test]
fn oversized_requests_and_unknown_verbs_are_refused() {
    let h = Host::new("big");
    h.refused(&"a".repeat(20000));
    h.refused(&format!("run t94 {} 96 0", "a".repeat(17000)));
    h.refused("status t94 a b c d e f g h i j k l");
    h.refused(&format!("status {}", "a".repeat(65)));
    h.refused(&format!("run t94 {} 96 0", "a".repeat(65)));
    for verb in [
        "", " ", "sh", "bash -c id", "exec id", "eval id", "scp -t /tmp", "rsync --server -e.LsfxC . /tmp",
        "sftp", "internal-sftp", "RUN t94 app 96 0", "run_ t94", "id", "ls", "cat /etc/passwd", "ping x",
    ] {
        h.refused(verb);
    }
    let (ok, so, _) = h.guest("ping");
    assert!(ok && so.starts_with("wc-session ok "), "{so}");
}

/// The guest-RAM path runs as root and nothing of ours asks for it: refused
/// unless the host's own environment opts in, whatever the request.
#[test]
fn the_root_steps_are_out_of_the_guest_s_reach() {
    let h = Host::new("root");
    for req in [
        // Master's hole: a run id without `0x` passed, and went into the root
        // script with `%s`. With $ROOTC a command that leaves a file, master
        // leaves the file.
        "up t94 1;id 16384 1048576",
        "up t94 1 16384 1048576",
        "up t94 0x1 16384 1048576",
        "up t94 0x1 16384 1048576 frames",
        "up t94 12345 ivshmem",
        "up t94 0x ivshmem",
        "up t94 0x12345678901234567 ivshmem",
        "up t94 0xg ivshmem",
        "up t94 0x1 ivshmem extra",
        "up t94 0x1 16384 1048576 notframes",
    ] {
        h.refused(req);
    }
    assert!(h.p("run/t94/wl-proxy").exists(), "a refused up removed the session");
    // A session with a plan in it, as only the guest-RAM `up` writes one.
    std::fs::create_dir_all(h.p("run/t95")).unwrap();
    std::fs::write(h.p("run/t95/arena.plan"), "").unwrap();
    let _up = UnixListener::bind(h.p("run/t95/up.sock")).unwrap();
    h.refused("proxy t95");
    let (ok, _, se) = h.guest("down t95");
    assert!(ok, "{se}");
    assert!(!h.p("out/ROOTED").exists(), "down of a guest-RAM session reached the root step");
}

/// The host's definitions are checked as strictly as the requests: a
/// definition that is not what the file's rules say runs nothing.
#[test]
fn a_definition_outside_the_rules_runs_nothing() {
    let h = Host::new("defs");
    let r = h.root.display().to_string();
    for (what, def) in [
        ("relative exec", "exec = app\n".to_string()),
        ("missing exec", "exec = /nonexistent/app\n".to_string()),
        ("exec is a directory", format!("exec = {r}/base\n")),
        ("profile outside", format!("exec = {r}/app\nprofile = {r}/elsewhere\n")),
        ("profile with ..", format!("exec = {r}/app\nprofile = {r}/home/.wc-launch/../x\n")),
        ("profile nested", format!("exec = {r}/app\nprofile = {r}/home/.wc-launch/a/b\n")),
        ("unknown key", format!("exec = {r}/app\nexecc = /bin/sh\n")),
        ("bad titlebar", format!("exec = {r}/app\ntitlebar = tab\n")),
        ("bad gpu", format!("exec = {r}/app\ngpu = yes\n")),
        ("bad env", format!("exec = {r}/app\nenv = 1X=y\n")),
        ("bad desktop", format!("exec = {r}/app\ndesktop = ../x\n")),
    ] {
        h.apps(&format!("[app]\n{def}"));
        let (ok, so, se) = h.guest("run t94 app 96 0");
        assert!(!ok, "{what}: accepted\n{so}{se}");
        assert!(!h.p("out/app.argv").exists(), "{what}: the application ran");
    }
    std::fs::remove_file(h.p("base/apps.ini")).unwrap();
    let (ok, _, se) = h.guest("run t94 app 96 0");
    assert!(!ok && se.contains("apps.ini is missing"), "{se}");
}
