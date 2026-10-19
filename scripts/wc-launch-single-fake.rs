// §102.14: a stand-in for ssh.exe, the compositor and the relay, so that the
// real wc-launch.exe can be driven under Wine by scripts/wc-launch-single-wine.sh;
// and the probes that harness reads windows and dialogs with. Raw Win32, no
// crates: the harness builds it as a one-file package in its scratch directory.
#![allow(non_snake_case, clippy::missing_safety_doc)]
use std::ffi::c_void;
use std::io::Write;
use std::path::PathBuf;
use std::time::{Duration, Instant};

type HWND = *mut c_void;
#[repr(C)]
struct WNDCLASSW {
    style: u32,
    proc_: unsafe extern "system" fn(HWND, u32, usize, isize) -> isize,
    cls: i32,
    wnd: i32,
    inst: *mut c_void,
    icon: *mut c_void,
    cursor: *mut c_void,
    bg: *mut c_void,
    menu: *const u16,
    name: *const u16,
}
#[repr(C)]
struct MSG {
    h: HWND,
    m: u32,
    w: usize,
    l: isize,
    t: u32,
    x: i32,
    y: i32,
    p: u32,
}
#[link(name = "user32")]
unsafe extern "system" {
    fn RegisterClassW(c: *const WNDCLASSW) -> u16;
    fn CreateWindowExW(ex: u32, cls: *const u16, t: *const u16, s: u32, x: i32, y: i32, w: i32, h: i32, p: HWND, m: *mut c_void, i: *mut c_void, l: *mut c_void) -> HWND;
    fn DefWindowProcW(h: HWND, m: u32, w: usize, l: isize) -> isize;
    fn ShowWindow(h: HWND, c: i32) -> i32;
    fn PeekMessageW(m: *mut MSG, h: HWND, a: u32, b: u32, r: u32) -> i32;
    fn TranslateMessage(m: *const MSG) -> i32;
    fn DispatchMessageW(m: *const MSG) -> isize;
    fn EnumWindows(f: unsafe extern "system" fn(HWND, isize) -> i32, l: isize) -> i32;
    fn EnumChildWindows(p: HWND, f: unsafe extern "system" fn(HWND, isize) -> i32, l: isize) -> i32;
    fn GetWindowTextW(h: HWND, b: *mut u16, n: i32) -> i32;
    fn GetClassNameW(h: HWND, b: *mut u16, n: i32) -> i32;
    fn GetWindowThreadProcessId(h: HWND, p: *mut u32) -> u32;
    fn IsIconic(h: HWND) -> i32;
    fn IsWindowVisible(h: HWND) -> i32;
    fn GetForegroundWindow() -> HWND;
    fn PostMessageW(h: HWND, m: u32, w: usize, l: isize) -> i32;
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}
fn dir() -> PathBuf {
    PathBuf::from(std::env::var("FAKE_DIR").expect("FAKE_DIR"))
}
fn note(file: &str, line: &str) {
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(dir().join(file)) {
        let _ = writeln!(f, "{line}");
    }
}
fn read(file: &str) -> String {
    std::fs::read_to_string(dir().join(file)).unwrap_or_default().trim().to_string()
}
fn stop() -> bool {
    dir().join("stop").exists()
}

unsafe extern "system" fn wndproc(h: HWND, m: u32, w: usize, l: isize) -> isize {
    unsafe { DefWindowProcW(h, m, w, l) }
}

fn pump() {
    let mut m = MSG { h: std::ptr::null_mut(), m: 0, w: 0, l: 0, t: 0, x: 0, y: 0, p: 0 };
    unsafe {
        while PeekMessageW(&mut m, std::ptr::null_mut(), 0, 0, 1) != 0 {
            TranslateMessage(&m);
            DispatchMessageW(&m);
        }
    }
}

fn title(h: HWND) -> String {
    let mut b = [0u16; 512];
    let n = unsafe { GetWindowTextW(h, b.as_mut_ptr(), 512) };
    String::from_utf16_lossy(&b[..n.max(0) as usize])
}
fn class(h: HWND) -> String {
    let mut b = [0u16; 256];
    let n = unsafe { GetClassNameW(h, b.as_mut_ptr(), 256) };
    String::from_utf16_lossy(&b[..n.max(0) as usize])
}
fn tops() -> Vec<HWND> {
    unsafe extern "system" fn each(h: HWND, l: isize) -> i32 {
        unsafe { (*(l as *mut Vec<HWND>)).push(h) };
        1
    }
    let mut v: Vec<HWND> = Vec::new();
    unsafe { EnumWindows(each, &mut v as *mut _ as isize) };
    v
}
fn children(p: HWND) -> Vec<HWND> {
    unsafe extern "system" fn each(h: HWND, l: isize) -> i32 {
        unsafe { (*(l as *mut Vec<HWND>)).push(h) };
        1
    }
    let mut v: Vec<HWND> = Vec::new();
    unsafe { EnumChildWindows(p, each, &mut v as *mut _ as isize) };
    v
}
fn titled(t: &str) -> Vec<HWND> {
    tops().into_iter().filter(|h| unsafe { IsWindowVisible(*h) } != 0 && title(*h) == t).collect()
}

fn compositor(args: &[String]) {
    let log = args.iter().position(|a| a == "--log").map(|i| args[i + 1].clone()).unwrap();
    let pid = std::process::id();
    note("roles.log", &format!("compositor pid {pid}"));
    let write = |s: &str| {
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&log) {
            let _ = writeln!(f, "{s}");
        }
    };
    write("wayland rings: none (fake)");
    write("header published: run fake1, 0 pages");
    let cls = wide("wcfake");
    unsafe {
        RegisterClassW(&WNDCLASSW {
            style: 0,
            proc_: wndproc,
            cls: 0,
            wnd: 0,
            inst: std::ptr::null_mut(),
            icon: std::ptr::null_mut(),
            cursor: std::ptr::null_mut(),
            bg: std::ptr::null_mut(),
            menu: std::ptr::null(),
            name: cls.as_ptr(),
        })
    };
    let mut windows = 0;
    while !stop() {
        let trig = dir().join("show-window");
        if trig.exists() {
            let _ = std::fs::remove_file(&trig);
            std::thread::sleep(Duration::from_millis(read("window-delay").parse().unwrap_or(0)));
            let t = wide(&read("title"));
            // WS_OVERLAPPEDWINDOW, hidden until shown, like the compositor.
            let h = unsafe {
                CreateWindowExW(0, cls.as_ptr(), t.as_ptr(), 0x00CF_0000, 100, 100, 400, 300, std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null_mut())
            };
            unsafe { ShowWindow(h, 5) };
            windows += 1;
            write(&format!("toplevel {windows} mapped (fake)"));
            note("roles.log", &format!("compositor showed window {windows} hwnd {:x}", h as usize));
        }
        pump();
        std::thread::sleep(Duration::from_millis(30));
    }
}

fn ssh(args: &[String]) {
    if args.iter().any(|a| a == "-N") {
        note("ssh.log", "tunnel");
        while !stop() {
            std::thread::sleep(Duration::from_millis(100));
        }
        return;
    }
    let host = args.iter().position(|a| a == "u@fake").unwrap();
    let verb = &args[host + 1..];
    note("ssh.log", &verb.join(" "));
    match verb[0].as_str() {
        "run" => {
            std::thread::sleep(Duration::from_millis(read("run-delay").parse().unwrap_or(0)));
            match read("run-mode").as_str() {
                "refuse" => {
                    eprintln!("wc-session: run: a nested session is already running");
                    std::process::exit(1);
                }
                "nowindow" => println!("started (no window will come)"),
                _ => {
                    std::fs::write(dir().join("show-window"), "").unwrap();
                    println!("started");
                }
            }
        }
        "up" => println!("up ok (fake)"),
        "proxy" => println!("proxy ok (fake)"),
        "down" => println!("down"),
        _ => {
            eprintln!("fake: no {}", verb[0]);
            std::process::exit(1);
        }
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("wayland") => compositor(&args),
        Some("-i") => ssh(&args),
        // the relay: `port socket --timeout 0`
        Some(a) if a.parse::<u16>().is_ok() => {
            while !stop() {
                std::thread::sleep(Duration::from_millis(100));
            }
        }
        // probes
        Some("windows") => {
            let fg = unsafe { GetForegroundWindow() };
            for h in titled(&args[1]) {
                let mut pid = 0;
                unsafe { GetWindowThreadProcessId(h, &mut pid) };
                println!("window hwnd={:x} pid={pid} iconic={} foreground={}", h as usize, unsafe { IsIconic(h) }, h == fg);
            }
        }
        Some("minimize") => {
            for h in titled(&args[1]) {
                unsafe { ShowWindow(h, 6) };
                println!("minimized {:x}", h as usize);
            }
        }
        // Wait for a dialog captioned args[1], print its text, close it.
        Some("dialog") => {
            let secs: u64 = args[2].parse().unwrap();
            let t = Instant::now();
            loop {
                let ds: Vec<HWND> = titled(&args[1]).into_iter().filter(|h| class(*h) == "#32770").collect();
                if let Some(d) = ds.first() {
                    let text: Vec<String> = children(*d).into_iter().filter(|c| class(*c).eq_ignore_ascii_case("static")).map(title).filter(|s| !s.is_empty()).collect();
                    println!("dialogs={} text={:?}", ds.len(), text.join(" | "));
                    for d in &ds {
                        unsafe { PostMessageW(*d, 0x0010, 0, 0) };
                    }
                    return;
                }
                if t.elapsed() > Duration::from_secs(secs) {
                    println!("dialogs=0");
                    return;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
        }
        _ => {
            eprintln!("wcfake: {args:?}");
            std::process::exit(2);
        }
    }
}
