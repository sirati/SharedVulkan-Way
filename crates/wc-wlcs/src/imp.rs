//! The integration itself. See the crate header for the shape of it.

use std::ffi::{CStr, CString, c_char, c_int, c_void};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::io::IntoRawFd;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

// ---------------------------------------------------------------------------
// The C ABI, transcribed from wlcs/display_server.h
// ---------------------------------------------------------------------------
//
// Transcribed rather than bindgen'd. It is four structs of function pointers
// that have changed twice in eight years, and a build-time dependency on
// libclang to read a header this size would cost more than it saves. The
// version constants below are the ones the header defines; a mismatch is a
// loud runtime error from WLCS rather than a silent misbehaviour, and the
// harness asserts on the suite's version for the same reason.

#[repr(C)]
pub struct WlcsExtensionDescriptor {
    name: *const c_char,
    version: u32,
}
// Safety: the only instances are built from `&'static CStr`s below, so the
// pointer is to immutable static data for the life of the process.
unsafe impl Sync for WlcsExtensionDescriptor {}

#[repr(C)]
pub struct WlcsIntegrationDescriptor {
    version: u32,
    num_extensions: usize,
    supported_extensions: *const WlcsExtensionDescriptor,
}

#[repr(C)]
pub struct WlcsPointer {
    version: u32,
    move_absolute: unsafe extern "C" fn(*mut WlcsPointer, i32, i32),
    move_relative: unsafe extern "C" fn(*mut WlcsPointer, i32, i32),
    button_up: unsafe extern "C" fn(*mut WlcsPointer, c_int),
    button_down: unsafe extern "C" fn(*mut WlcsPointer, c_int),
    destroy: unsafe extern "C" fn(*mut WlcsPointer),
}

#[repr(C)]
pub struct WlcsTouch {
    version: u32,
    touch_down: unsafe extern "C" fn(*mut WlcsTouch, i32, i32),
    touch_move: unsafe extern "C" fn(*mut WlcsTouch, i32, i32),
    touch_up: unsafe extern "C" fn(*mut WlcsTouch),
    destroy: unsafe extern "C" fn(*mut WlcsTouch),
}

#[repr(C)]
pub struct WlcsDisplayServer {
    version: u32,
    start: Option<unsafe extern "C" fn(*mut WlcsDisplayServer)>,
    stop: Option<unsafe extern "C" fn(*mut WlcsDisplayServer)>,
    create_client_socket: Option<unsafe extern "C" fn(*mut WlcsDisplayServer) -> c_int>,
    position_window_absolute:
        Option<unsafe extern "C" fn(*mut WlcsDisplayServer, *mut c_void, *mut c_void, c_int, c_int)>,
    create_pointer: Option<unsafe extern "C" fn(*mut WlcsDisplayServer) -> *mut WlcsPointer>,
    create_touch: Option<unsafe extern "C" fn(*mut WlcsDisplayServer) -> *mut WlcsTouch>,
    get_descriptor:
        Option<unsafe extern "C" fn(*const WlcsDisplayServer) -> *const WlcsIntegrationDescriptor>,
    start_on_this_thread: Option<unsafe extern "C" fn(*mut WlcsDisplayServer, *mut c_void)>,
}

#[repr(C)]
pub struct WlcsServerIntegration {
    version: u32,
    create_server: unsafe extern "C" fn(c_int, *const *const c_char) -> *mut WlcsDisplayServer,
    destroy_server: unsafe extern "C" fn(*mut WlcsDisplayServer),
}
unsafe impl Sync for WlcsServerIntegration {}

/// The symbol WLCS looks up. Everything else in this file hangs off it.
#[unsafe(no_mangle)]
pub static wlcs_server_integration: WlcsServerIntegration = WlcsServerIntegration {
    version: 1,
    create_server,
    destroy_server,
};

// ---------------------------------------------------------------------------
// libwayland-client, by dlsym
// ---------------------------------------------------------------------------

/// The two entry points `position_window_absolute` needs, looked up in the
/// process that loaded this library.
///
/// `RTLD_DEFAULT` rather than `dlopen("libwayland-client.so")`: the runner is
/// already linked against libwayland-client — it *is* the Wayland client in
/// every test — so the symbols are there, and asking for them by name in the
/// global scope cannot pick up a second copy of the library with its own
/// object tables.
mod wayland {
    use std::ffi::{CStr, c_int, c_void};
    use std::sync::OnceLock;

    type GetFd = unsafe extern "C" fn(*mut c_void) -> c_int;
    type GetId = unsafe extern "C" fn(*mut c_void) -> u32;

    fn sym(name: &CStr) -> Option<*mut c_void> {
        // Safety: a plain lookup in the global symbol scope; the name is a
        // NUL-terminated literal.
        let p = unsafe { libc::dlsym(libc::RTLD_DEFAULT, name.as_ptr()) };
        (!p.is_null()).then_some(p)
    }

    /// Which file descriptor is this connection?
    ///
    /// This shim opened every client socket WLCS has, so an fd is the one
    /// handle that identifies a connection on both sides of the API.
    pub fn display_fd(display: *mut c_void) -> Option<c_int> {
        static F: OnceLock<Option<usize>> = OnceLock::new();
        let f = (*F.get_or_init(|| sym(c"wl_display_get_fd").map(|p| p as usize)))?;
        // Safety: the address came from dlsym for exactly this signature, and
        // `display` is the pointer WLCS handed us.
        Some(unsafe { std::mem::transmute::<usize, GetFd>(f)(display) })
    }

    /// Which protocol object id is this proxy?
    pub fn proxy_id(proxy: *mut c_void) -> Option<u32> {
        static F: OnceLock<Option<usize>> = OnceLock::new();
        let f = (*F.get_or_init(|| sym(c"wl_proxy_get_id").map(|p| p as usize)))?;
        // Safety: as above.
        Some(unsafe { std::mem::transmute::<usize, GetId>(f)(proxy) })
    }
}

// ---------------------------------------------------------------------------
// What extensions to claim
// ---------------------------------------------------------------------------

/// The globals this compositor advertises, as measured against the tree at the
/// time this file was written.
///
/// # Why the table exists and why it is not authoritative
///
/// WLCS uses this list to decide whether a missing global is a **skip** or a
/// **failure** (`in_process_server.cpp`, `bind_if_supported`): an extension
/// that is absent *and* undeclared is `ExtensionExpectedlyNotSupported`, which
/// is the suite's own word for "you told me you do not implement this, so this
/// test does not apply". That is exactly the shape a justified exclusion needs
/// — and it is also exactly the shape a *dishonest* one needs, which is why the
/// list must not be a wish.
///
/// So `WC_WLCS_EXTENSIONS` overrides it with a file of `name version` lines,
/// and `scripts/wc-wlcs.sh` generates that file by running `wayland-info` — a
/// third-party client — against the compositor under test, immediately before
/// the suite runs. The table below is the fallback for a hand-run, and the
/// harness fails if the two disagree. A global that somebody adds tomorrow
/// therefore turns its tests on by itself, and one that is removed turns them
/// into skips, without anybody editing this file.
const DEFAULT_EXTENSIONS: &[(&str, u32)] = &[
    ("wl_compositor", 5),
    ("wl_subcompositor", 1),
    ("wl_shm", 2),
    ("wl_seat", 9),
    ("wl_output", 4),
    ("wl_data_device_manager", 3),
    ("xdg_wm_base", 6),
    ("zxdg_decoration_manager_v1", 1),
    ("org_kde_kwin_server_decoration_manager", 1),
    ("xdg_toplevel_icon_manager_v1", 1),
    ("wc_icon_manager_v1", 1),
    ("wc_clipboard_manager_v1", 1),
    ("wp_cursor_shape_manager_v1", 2),
    ("zwp_idle_inhibit_manager_v1", 1),
    ("xdg_activation_v1", 1),
    ("zwp_keyboard_shortcuts_inhibit_manager_v1", 1),
];

/// Parse the `name version` file `WC_WLCS_EXTENSIONS` points at.
///
/// Blank lines and `#` comments are skipped so the harness can say in the file
/// where it came from. A malformed line is dropped rather than fatal: the
/// alternative is a suite that will not start because a comment moved.
pub fn parse_extensions(text: &str) -> Vec<(String, u32)> {
    text.lines()
        .map(|l| l.split('#').next().unwrap_or("").trim())
        .filter(|l| !l.is_empty())
        .filter_map(|l| {
            let mut it = l.split_whitespace();
            let name = it.next()?;
            let version = it.next()?.parse().ok()?;
            Some((name.to_owned(), version))
        })
        .collect()
}

fn extensions() -> Vec<(String, u32)> {
    if let Ok(path) = std::env::var("WC_WLCS_EXTENSIONS") {
        match std::fs::read_to_string(&path) {
            Ok(text) => {
                let list = parse_extensions(&text);
                if list.is_empty() {
                    eprintln!("wc-wlcs: {path} named no extensions; falling back to the table");
                } else {
                    return list;
                }
            }
            Err(e) => eprintln!("wc-wlcs: could not read WC_WLCS_EXTENSIONS={path}: {e}"),
        }
    }
    DEFAULT_EXTENSIONS
        .iter()
        .map(|(n, v)| ((*n).to_owned(), *v))
        .collect()
}

// ---------------------------------------------------------------------------
// The server
// ---------------------------------------------------------------------------

/// One compositor process, and everything needed to talk to it.
///
/// `#[repr(C)]` with the vtable first, because WLCS passes back the
/// `WlcsDisplayServer*` it was given and every hook casts it straight back to
/// this.
#[repr(C)]
struct Server {
    vtable: WlcsDisplayServer,
    rundir: PathBuf,
    keep: bool,
    child: Option<Child>,
    control: Option<UnixStream>,
    /// `(fd, ordinal)` for every socket handed to WLCS, so that a
    /// `wl_display*` can be turned back into "the nth client this compositor
    /// accepted". See `crate::imp::Server::client_of`.
    clients: Vec<(c_int, u32)>,
    descriptor: WlcsIntegrationDescriptor,
    /// Kept alive because `descriptor` points into them.
    _names: Vec<CString>,
    _extensions: Vec<WlcsExtensionDescriptor>,
}

unsafe extern "C" fn create_server(
    _argc: c_int,
    _argv: *const *const c_char,
) -> *mut WlcsDisplayServer {
    // Arguments are accepted and ignored. WLCS forwards whatever followed its
    // own options on the command line, on the assumption that a compositor
    // library wants them; ours is configured by environment because the thing
    // being configured is *which program to run*, and the harness sets it once
    // for the whole suite rather than per test.
    let rundir = match mkdtemp() {
        Ok(d) => d,
        Err(e) => {
            eprintln!("wc-wlcs: could not make a run directory: {e}");
            return std::ptr::null_mut();
        }
    };

    let list = extensions();
    let names: Vec<CString> = list
        .iter()
        .filter_map(|(n, _)| CString::new(n.as_str()).ok())
        .collect();
    let exts: Vec<WlcsExtensionDescriptor> = names
        .iter()
        .zip(list.iter())
        .map(|(n, (_, v))| WlcsExtensionDescriptor {
            name: n.as_ptr(),
            version: *v,
        })
        .collect();

    let mut server = Box::new(Server {
        vtable: WlcsDisplayServer {
            // 3 rather than 2: the struct laid out above is the version-3 one,
            // and `start` is provided, which is what WLCS prefers when both
            // are present (`make_context_if_needed`). Declaring 2 would work
            // and would also mean `start_on_this_thread` is not part of the
            // struct WLCS reads, which is a lie about the layout.
            version: 3,
            start: Some(start),
            stop: Some(stop),
            create_client_socket: Some(create_client_socket),
            position_window_absolute: Some(position_window_absolute),
            create_pointer: Some(create_pointer),
            create_touch: Some(create_touch),
            get_descriptor: Some(get_descriptor),
            start_on_this_thread: None,
        },
        rundir,
        keep: std::env::var_os("WC_WLCS_KEEP").is_some(),
        child: None,
        control: None,
        clients: Vec::new(),
        descriptor: WlcsIntegrationDescriptor {
            version: 1,
            num_extensions: exts.len(),
            supported_extensions: std::ptr::null(),
        },
        _names: names,
        _extensions: exts,
    });
    server.descriptor.supported_extensions = server._extensions.as_ptr();
    Box::into_raw(server).cast()
}

unsafe extern "C" fn destroy_server(raw: *mut WlcsDisplayServer) {
    if raw.is_null() {
        return;
    }
    // Safety: `raw` is what `create_server` returned, which is a leaked Box.
    let mut server = unsafe { Box::from_raw(raw.cast::<Server>()) };
    server.shutdown();
    if !server.keep {
        let _ = std::fs::remove_dir_all(&server.rundir);
    } else {
        eprintln!("wc-wlcs: kept {}", server.rundir.display());
    }
}

/// Recover the `Server` a hook was called on.
///
/// # Safety
/// `raw` must be a pointer `create_server` returned and `destroy_server` has
/// not yet been called on.
unsafe fn server<'a>(raw: *mut WlcsDisplayServer) -> &'a mut Server {
    unsafe { &mut *raw.cast::<Server>() }
}

unsafe extern "C" fn start(raw: *mut WlcsDisplayServer) {
    let s = unsafe { server(raw) };
    if let Err(e) = s.launch() {
        // There is no error channel in the API: `start` returns void, and a
        // compositor that failed to come up shows up as every subsequent test
        // failing to connect. Saying why, once, on stderr is the only place
        // the reason can be recorded.
        eprintln!("wc-wlcs: could not start the compositor: {e}");
        eprintln!("wc-wlcs: log is {}", s.rundir.join("compositor.log").display());
    }
}

unsafe extern "C" fn stop(raw: *mut WlcsDisplayServer) {
    unsafe { server(raw) }.shutdown();
}

unsafe extern "C" fn create_client_socket(raw: *mut WlcsDisplayServer) -> c_int {
    let s = unsafe { server(raw) };
    match s.connect_client() {
        Ok(fd) => fd,
        Err(e) => {
            eprintln!("wc-wlcs: could not connect a client: {e}");
            -1
        }
    }
}

unsafe extern "C" fn get_descriptor(
    raw: *const WlcsDisplayServer,
) -> *const WlcsIntegrationDescriptor {
    // Safety: `raw` is the vtable, which is the first field of `Server`.
    let s = unsafe { &*raw.cast::<Server>() };
    &s.descriptor
}

unsafe extern "C" fn position_window_absolute(
    raw: *mut WlcsDisplayServer,
    display: *mut c_void,
    surface: *mut c_void,
    x: c_int,
    y: c_int,
) {
    let s = unsafe { server(raw) };
    let (Some(fd), Some(id)) = (wayland::display_fd(display), wayland::proxy_id(surface)) else {
        eprintln!("wc-wlcs: libwayland-client's wl_display_get_fd/wl_proxy_get_id are not in scope");
        return;
    };
    let Some(client) = s.client_of(fd) else {
        eprintln!("wc-wlcs: a wl_display on fd {fd} that this shim never handed out");
        return;
    };
    let _ = s.request(&format!("position {client} {id} {x} {y}"));
}

// ---------------------------------------------------------------------------
// Input devices
// ---------------------------------------------------------------------------

/// A pointer, which is a handle onto the server's control socket.
#[repr(C)]
struct Pointer {
    vtable: WlcsPointer,
    server: *mut Server,
    /// Where the suite last put it, in compositor coordinates. Kept here
    /// because `move_relative` is defined against it and the control protocol
    /// is absolute — a relative protocol would need the two sides to agree on
    /// a starting point that neither of them chose.
    x: f64,
    y: f64,
}

unsafe extern "C" fn create_pointer(raw: *mut WlcsDisplayServer) -> *mut WlcsPointer {
    let p = Box::new(Pointer {
        vtable: WlcsPointer {
            version: 1,
            move_absolute: pointer_move_absolute,
            move_relative: pointer_move_relative,
            button_up: pointer_button_up,
            button_down: pointer_button_down,
            destroy: pointer_destroy,
        },
        server: raw.cast(),
        x: 0.0,
        y: 0.0,
    });
    Box::into_raw(p).cast()
}

/// `wl_fixed_t` is a 24.8 fixed-point signed integer.
fn from_fixed(v: i32) -> f64 {
    v as f64 / 256.0
}

unsafe fn pointer<'a>(raw: *mut WlcsPointer) -> &'a mut Pointer {
    unsafe { &mut *raw.cast::<Pointer>() }
}

unsafe extern "C" fn pointer_move_absolute(raw: *mut WlcsPointer, x: i32, y: i32) {
    let p = unsafe { pointer(raw) };
    p.x = from_fixed(x);
    p.y = from_fixed(y);
    let (px, py) = (p.x, p.y);
    unsafe { &mut *p.server }.request_ok(&format!("pointer-move {px} {py}"));
}

unsafe extern "C" fn pointer_move_relative(raw: *mut WlcsPointer, dx: i32, dy: i32) {
    let p = unsafe { pointer(raw) };
    p.x += from_fixed(dx);
    p.y += from_fixed(dy);
    let (px, py) = (p.x, p.y);
    unsafe { &mut *p.server }.request_ok(&format!("pointer-move {px} {py}"));
}

unsafe extern "C" fn pointer_button_down(raw: *mut WlcsPointer, button: c_int) {
    let p = unsafe { pointer(raw) };
    unsafe { &mut *p.server }.request_ok(&format!("pointer-button {button} 1"));
}

unsafe extern "C" fn pointer_button_up(raw: *mut WlcsPointer, button: c_int) {
    let p = unsafe { pointer(raw) };
    unsafe { &mut *p.server }.request_ok(&format!("pointer-button {button} 0"));
}

unsafe extern "C" fn pointer_destroy(raw: *mut WlcsPointer) {
    // Deliberately not a `pointer-leave`. WLCS destroys the device at the end
    // of a test and a compositor whose pointer vanishes when the mouse is
    // unplugged is a different thing from one whose pointer moved away; the
    // suite does not ask for the second, so inventing it would be inventing
    // events the tests then have to tolerate.
    drop(unsafe { Box::from_raw(raw.cast::<Pointer>()) });
}

/// A single touch contact, driven down the control socket.
///
/// # What this used to be, and why the comment mattered more than the code
///
/// Until §40 the methods here were empty, and the comment above them said the
/// device did nothing "because there is no touch anywhere in this project" —
/// `wl_seat` advertised no touch capability, `FromUi` had no touch event, and
/// `win32.rs` handled no `WM_POINTER*` message. Every one of those three
/// statements was true when it was written and none of them was true by §24:
/// `has_touch` gates a real `add_touch`, `FromUi` carries four touch variants,
/// and `win32.rs` has a `WM_POINTER*` handler with a `pointerId`-to-slot map.
///
/// **A stub is only honest for as long as its reason is checked**, and this one
/// outlived its reason by sixteen stages while continuing to explain, in
/// confident detail, why roughly a hundred failing tests were somebody else's
/// problem. It is worth naming as a failure mode rather than quietly deleting:
/// the comment was what stopped anybody looking.
///
/// # One contact, and why that is not a shortcut
///
/// WLCS's device is single-contact — `touch_down`, `touch_move`, `touch_up`,
/// with no slot argument — so this reports slot 0 and nothing else. The
/// multi-contact bookkeeping (`crate::input::touch::Slots` in the compositor)
/// maps Windows' sparse `pointerId`s onto the protocol's dense slots, and it is
/// exercised by the Win32 path rather than by this one. Driving two synthetic
/// contacts from here would be testing the harness against itself.
///
/// WLCS dereferences whatever `create_touch` returns before it does anything
/// else (`in_process_server.cpp`'s `Touch::Impl`, which reads `touch->version`),
/// so returning null is a segfault rather than a skip.
#[repr(C)]
struct Touch {
    vtable: WlcsTouch,
    server: *mut Server,
}

unsafe extern "C" fn create_touch(raw: *mut WlcsDisplayServer) -> *mut WlcsTouch {
    Box::into_raw(Box::new(Touch {
        vtable: WlcsTouch {
            version: 1,
            touch_down,
            touch_move,
            touch_up,
            destroy: touch_destroy,
        },
        server: raw.cast(),
    }))
    .cast()
}

unsafe fn touch<'a>(raw: *mut WlcsTouch) -> &'a mut Touch {
    unsafe { &mut *raw.cast::<Touch>() }
}

/// **`touch_down` and `touch_move` take whole pixels, not `wl_fixed_t`, and the
/// header says otherwise.**
///
/// `wlcs/touch.h` declares both as `void (*)(WlcsTouch*, wl_fixed_t x,
/// wl_fixed_t y)`, and `wlcs::Pointer` really does send fixed-point — so the
/// obvious thing, and the thing this shim did first, is to divide by 256 in
/// both places. But `in_process_server.cpp`'s `Touch::Impl` is
///
/// ```c++
/// void down_at(int x, int y) { touch->touch_down(touch.get(), x, y); }
/// ```
///
/// with no `wl_fixed_from_int` anywhere on the path, while `Pointer::Impl`'s
/// `move_to` does convert. The types disagree with the calls, and the calls are
/// what runs.
///
/// It cost a measurement to find, and it is worth recording *how*: every touch
/// test failed with the contact landing on no surface at all, which looks
/// exactly like "touch is not wired up" — the conclusion the stub this replaced
/// had already asserted. One `eprintln!` of the received coordinates showed
/// `0.355, 0.059` where `91, 15` was expected, and 91/256 = 0.355 named the bug
/// immediately. A wrong unit is invisible in a pass/fail scoreboard and obvious
/// in one printed number.
unsafe extern "C" fn touch_down(raw: *mut WlcsTouch, x: i32, y: i32) {
    let t = unsafe { touch(raw) };
    unsafe { &mut *t.server }.request_ok(&format!("touch-down {x} {y}"));
}

unsafe extern "C" fn touch_move(raw: *mut WlcsTouch, x: i32, y: i32) {
    let t = unsafe { touch(raw) };
    unsafe { &mut *t.server }.request_ok(&format!("touch-move {x} {y}"));
}

unsafe extern "C" fn touch_up(raw: *mut WlcsTouch) {
    let t = unsafe { touch(raw) };
    unsafe { &mut *t.server }.request_ok("touch-up");
}

unsafe extern "C" fn touch_destroy(raw: *mut WlcsTouch) {
    // Deliberately not a `touch-up`, for the same reason `pointer_destroy` is
    // not a `pointer-leave`: unplugging a digitiser is not a finger lifting,
    // and the suite does not ask for the second.
    drop(unsafe { Box::from_raw(raw.cast::<Touch>()) });
}

// ---------------------------------------------------------------------------
// Process supervision and the control socket
// ---------------------------------------------------------------------------

impl Server {
    fn wayland_socket(&self) -> PathBuf {
        self.rundir.join(SOCKET)
    }

    fn control_socket(&self) -> PathBuf {
        self.rundir.join("control")
    }

    fn launch(&mut self) -> std::io::Result<()> {
        let log = std::fs::File::create(self.rundir.join("compositor.log"))?;
        let errlog = log.try_clone()?;

        // Two ways to start something, and the second is the one that makes
        // this shim topology-independent.
        //
        // `WC_WLCS_COMPOSITOR` names a `wc-compositor` and this builds its
        // command line, which is the ordinary case. `WC_WLCS_LAUNCH` is a
        // whole shell command, run with `XDG_RUNTIME_DIR` and `WC_WLCS_SOCKET`
        // in its environment and nothing appended — because the Wine leg is
        // not one process but three (an X server's worth of desktop, a
        // Windows binary inside it, and `wc-proxy` in front), and no amount of
        // argument-building here can express that. See
        // `scripts/wc-wlcs-wine.sh`.
        let mut cmd = match std::env::var("WC_WLCS_LAUNCH") {
            Ok(line) => {
                let mut c = Command::new("sh");
                c.arg("-c").arg(line);
                c
            }
            Err(_) => {
                let program = std::env::var("WC_WLCS_COMPOSITOR")
                    .unwrap_or_else(|_| "target/debug/wc-compositor".to_owned());
                let mut c = Command::new(&program);
                c.arg("--socket")
                    .arg(SOCKET)
                    .arg("--control")
                    .arg(self.control_socket())
                    // Long enough that no single test can hit it, short enough
                    // that a process orphaned by a crashed runner is gone
                    // within the hour. `stop` is what normally ends it.
                    .arg("--timeout")
                    .arg("600")
                    // A desktop big enough that WLCS's own coordinates — which
                    // go up to a few hundred pixels — are always on it.
                    .arg("--output")
                    .arg("1280x1024");
                if let Ok(extra) = std::env::var("WC_WLCS_ARGS") {
                    c.args(extra.split_whitespace());
                }
                // **Not `--simulate-wm`.** That flag makes the headless
                // platform resize, activate and close windows on a schedule of
                // its own, which against a test suite is a second window
                // manager fighting the first.
                c
            }
        };
        // `XDG_RUNTIME_DIR` is how the socket name becomes a path: smithay's
        // `ListeningSocket::bind` resolves a name against it. Pointing it at
        // this server's own directory is what keeps 1,228 sequential
        // compositors from colliding, and what makes a leaked one findable.
        cmd.env("XDG_RUNTIME_DIR", &self.rundir)
            .env("WC_WLCS_SOCKET", SOCKET)
            .env("WC_WLCS_CONTROL", self.control_socket())
            .stdin(Stdio::null())
            .stdout(Stdio::from(log))
            .stderr(Stdio::from(errlog));
        let child = cmd.spawn()?;
        self.child = Some(child);

        // Wait for both sockets. The Wayland one has to exist before a client
        // can connect and the control one before a pointer can move; they are
        // created by different threads in the compositor and neither order is
        // guaranteed, so both are waited for.
        //
        // `WC_WLCS_NO_CONTROL` drops the second half of that wait, and it is
        // the Wine leg's flag rather than a debugging convenience:
        // `crate::control` is `#[cfg(unix)]`, so a `wc-compositor.exe` has no
        // control socket to bind and never will. What that costs is stated
        // where it is paid — `Server::request` — rather than hidden behind a
        // stub that answers "ok".
        let no_control = std::env::var_os("WC_WLCS_NO_CONTROL").is_some();
        let deadline = Instant::now() + Duration::from_secs(60);
        let wayland = self.wayland_socket();
        while Instant::now() < deadline {
            if wayland.exists() && no_control {
                return Ok(());
            }
            if wayland.exists()
                && let Ok(control) = UnixStream::connect(self.control_socket())
            {
                control.set_read_timeout(Some(Duration::from_secs(30)))?;
                self.control = Some(control);
                return Ok(());
            }
            if let Some(child) = self.child.as_mut()
                && let Ok(Some(status)) = child.try_wait()
            {
                return Err(std::io::Error::other(format!(
                    "the compositor exited with {status} before it was listening"
                )));
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        Err(std::io::Error::other(
            "the compositor did not listen within 60s",
        ))
    }

    fn connect_client(&mut self) -> std::io::Result<c_int> {
        // **Retried on `ECONNREFUSED`, and that is a race rather than
        // defensiveness.** `bind` creates the filesystem node and `listen`
        // makes it accept; between the two, `connect` is refused. `launch`
        // waits for the node to *exist*, which does not close that window —
        // and the control socket, which it also waits for, is bound before the
        // Wayland thread even starts, so it is no guard either.
        //
        // Measured: one test in 1,225 failed this way on a loaded machine, and
        // it read as a compositor change because it moved a SKIP to a FAIL
        // across a merge. A scoreboard whose noise floor is one test is a
        // scoreboard that cannot report a one-test regression, which is the
        // resolution this whole stage is trying to have.
        let deadline = Instant::now() + Duration::from_secs(5);
        let stream = loop {
            match UnixStream::connect(self.wayland_socket()) {
                Ok(s) => break s,
                Err(e)
                    if e.kind() == std::io::ErrorKind::ConnectionRefused
                        && Instant::now() < deadline =>
                {
                    std::thread::sleep(Duration::from_millis(5));
                }
                Err(e) => return Err(e),
            }
        };
        let fd = stream.into_raw_fd();
        // The ordinal the compositor will give this connection. Accept order,
        // counted from 1, and it matches because the `connect` above has
        // already put this connection in the listen queue ahead of any other
        // this shim will make.
        let ordinal = self.clients.len() as u32 + 1;
        self.clients.push((fd, ordinal));
        Ok(fd)
    }

    fn client_of(&self, fd: c_int) -> Option<u32> {
        // Backwards, so that a reused fd number — WLCS closes the ones it owns
        // — resolves to the most recent connection rather than a dead one.
        self.clients
            .iter()
            .rev()
            .find(|(f, _)| *f == fd)
            .map(|(_, o)| *o)
    }

    /// Send one request and read one answer.
    ///
    /// Blocking on purpose: the reply means the compositor has *done* the
    /// thing, and WLCS's next line is usually a wait for the client to see the
    /// consequence. See `control.rs`'s `converse` for the other end of the
    /// same argument.
    fn request(&mut self, line: &str) -> std::io::Result<Vec<String>> {
        let Some(control) = self.control.as_mut() else {
            // The Wine leg. Named rather than silent: a test that needed a
            // pointer and got nothing must fail with a sentence saying which
            // half of the rig was missing, not with a ten-second timeout that
            // reads as a compositor bug.
            return Err(std::io::Error::other(
                "no control socket: this leg cannot place windows or move a pointer \
                 (crate::control is cfg(unix); see docs/waycompose-design.md §26.5)",
            ));
        };
        control.write_all(line.as_bytes())?;
        control.write_all(b"\n")?;
        control.flush()?;
        let mut reader = BufReader::new(control.try_clone()?);
        let mut out = Vec::new();
        loop {
            let mut answer = String::new();
            if reader.read_line(&mut answer)? == 0 {
                return Err(std::io::Error::other("the control socket closed"));
            }
            let answer = answer.trim_end().to_owned();
            let done = answer.starts_with("ok") || answer.starts_with("err");
            let failed = answer.starts_with("err");
            out.push(answer);
            if done {
                if failed {
                    return Err(std::io::Error::other(out.join("; ")));
                }
                return Ok(out);
            }
        }
    }

    fn request_ok(&mut self, line: &str) {
        if let Err(e) = self.request(line) {
            eprintln!("wc-wlcs: control request {line:?} failed: {e}");
        }
    }

    /// Stop the compositor and block until it is gone.
    ///
    /// WLCS requires this: "*this should block until the server's mainloop has
    /// been torn down, so that it does not persist into later tests*". With
    /// 1,228 tests and a compositor per test, a `stop` that returned early
    /// would leave a drift of live processes behind a long run — which on a
    /// shared machine is somebody else's problem, and docs/waycompose-design.md §16.8 is
    /// about exactly that.
    fn shutdown(&mut self) {
        // Drop the control connection first: the compositor's listener thread
        // is blocked reading it, and closing it is how that thread learns the
        // run is over.
        self.control = None;
        let Some(mut child) = self.child.take() else {
            return;
        };
        // SIGTERM rather than SIGKILL, so the compositor runs its own exit
        // path and its log ends with the summary lines the harness reads.
        // Identity is a pid — this pid, the one we spawned — and never an
        // image name (docs/waycompose-design.md §16.8): there are other agents on this
        // machine and some of them are running compositors too.
        let pid = child.id() as libc::pid_t;
        // Safety: `pid` is our own child, and it has not been reaped — `child`
        // is still owned here.
        unsafe { libc::kill(pid, libc::SIGTERM) };
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match child.try_wait() {
                Ok(Some(_)) => return,
                Ok(None) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(5))
                }
                _ => break,
            }
        }
        let _ = child.kill();
        let _ = child.wait();
    }
}

/// The socket name inside the run directory.
///
/// Not `wayland-0`: a name that no other Wayland compositor would choose makes
/// a stray `WAYLAND_DISPLAY` in the environment unable to point a client at
/// the wrong server by accident.
const SOCKET: &str = "wc-wlcs";

/// A private directory for one compositor, under `WC_WLCS_RUNDIR` or `/tmp`.
///
/// `mkdtemp` rather than a counter, because two runners on one machine is the
/// normal case here — ten worktrees share this host — and a counter collides
/// silently while `mkdtemp` cannot.
fn mkdtemp() -> std::io::Result<PathBuf> {
    let base = std::env::var("WC_WLCS_RUNDIR").unwrap_or_else(|_| "/tmp".to_owned());
    let template = format!("{base}/wc-wlcs-XXXXXX\0");
    let mut buf = template.into_bytes();
    // Safety: `buf` is a NUL-terminated mutable C string of the right shape,
    // and `mkdtemp` writes only within the six trailing Xs.
    let p = unsafe { libc::mkdtemp(buf.as_mut_ptr().cast::<c_char>()) };
    if p.is_null() {
        return Err(std::io::Error::last_os_error());
    }
    // Safety: on success `mkdtemp` returned `buf`, NUL-terminated.
    let s = unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned();
    Ok(PathBuf::from(s))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fixed_point_coordinate_becomes_pixels() {
        assert_eq!(from_fixed(256), 1.0);
        assert_eq!(from_fixed(-128), -0.5);
        assert_eq!(from_fixed(0), 0.0);
    }

    #[test]
    fn the_extension_file_is_name_and_version() {
        let list = parse_extensions("wl_shm 2\nxdg_wm_base 6\n");
        assert_eq!(
            list,
            vec![("wl_shm".to_owned(), 2), ("xdg_wm_base".to_owned(), 6)]
        );
    }

    #[test]
    fn comments_and_blank_lines_are_ignored_so_the_harness_can_say_where_it_came_from() {
        let list = parse_extensions("# generated by wayland-info\n\nwl_seat 9 # the seat\n");
        assert_eq!(list, vec![("wl_seat".to_owned(), 9)]);
    }

    #[test]
    fn a_malformed_line_is_dropped_rather_than_taking_the_run_with_it() {
        let list = parse_extensions("wl_shm\nwl_seat nine\nwl_output 4\n");
        assert_eq!(list, vec![("wl_output".to_owned(), 4)]);
    }

    #[test]
    fn the_default_table_is_not_empty_and_names_the_shell() {
        assert!(DEFAULT_EXTENSIONS.iter().any(|(n, _)| *n == "xdg_wm_base"));
    }

    /// The list WLCS is told about must never contain something the compositor
    /// does not advertise, because that is the difference between a skip and a
    /// lie. This checks the direction that matters: nothing in the table is a
    /// protocol WLCS tests and we do not have.
    #[test]
    fn the_table_claims_nothing_from_the_excluded_list() {
        for absent in [
            "zwlr_layer_shell_v1",
            "zwp_pointer_constraints_v1",
            "zwp_relative_pointer_manager_v1",
            "zwp_linux_dmabuf_v1",
            "wp_presentation",
            "wp_viewporter",
            "wp_fractional_scale_manager_v1",
            "zxdg_output_manager_v1",
            "zwp_primary_selection_device_manager_v1",
        ] {
            assert!(
                !DEFAULT_EXTENSIONS.iter().any(|(n, _)| *n == absent),
                "the table claims {absent}, which this compositor does not advertise"
            );
        }
    }
}
