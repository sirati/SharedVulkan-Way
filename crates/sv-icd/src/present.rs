//! Where a presented frame goes once the app side holds its pixels.
//!
//! docs/design.md §10 stage 2 ends with "the app-side WSI then blits from the arena
//! to the window with `wl_shm` or `XShm` — a straight memcpy from shared memory
//! into a window buffer, with no GPU involvement and no second copy of our own".
//! This is that step, and it is deliberately the *only* part of presentation
//! that knows what a window is. [`crate::wsi`] produces a rectangle of BGRA
//! bytes in the arena and hands it here.
//!
//! # Three targets
//!
//! - [`Target::Headless`] displays nothing. That is not a stub: it is what
//!   `VK_EXT_headless_surface` means, it is what Wine maps `VK_KHR_win32_surface`
//!   onto when there is no display (docs/design.md §0.5), and it is what DXVK needs
//!   in order to create a device at all. `SV_PRESENT_DUMP` writes the frames out
//!   so that a harness can judge presentation on pixels rather than on
//!   `vkQueuePresentKHR` returning `VK_SUCCESS`.
//! - [`Target::X11`] puts the pixels in a window with `XPutImage`.
//! - [`Target::Wayland`] attaches a `wl_shm` buffer to the application's own
//!   `wl_surface`, on the application's own connection. It is the newest and
//!   the one with a measured reason to exist; see [`wayland`].
//!
//! This module's rule is the smallest one that is honest — **make something an
//! application can present to and a human can see**, and do not build a
//! compositor.
//!
//! # Why Xlib is `dlopen`ed rather than linked
//!
//! Three reasons, in order of weight. The ICD is a `cdylib` the Vulkan loader
//! `dlopen`s into arbitrary processes, and a hard `DT_NEEDED` on `libX11.so.6`
//! would make it unloadable on a machine that has no X at all — including the
//! Windows app side the brief requires. The dev shell has `xvfb` but no X
//! development libraries, so linking would mean adding one. And the ICD is
//! loaded *into* a process that already has libX11 open whenever the surface is
//! an X surface, so `dlopen` finds the copy that is already there.
//!
//! # Why we open our own connection instead of using the application's
//!
//! The loader hands us a `Display*` (or an `xcb_connection_t*`) in the
//! `VkIcdSurface`. We ignore it. Xlib is only thread-safe if the application
//! called `XInitThreads` — glxgears does not — and an ICD that issues requests
//! on somebody else's connection from a thread they did not expect is a
//! use-after-free waiting to happen. X window ids are global to the display, so
//! a second connection can draw into the same window with no cooperation from
//! the first. The cost is one extra socket to the X server and the assumption
//! that `$DISPLAY` names the same server the application connected to, which is
//! true of every case that is not deliberately adversarial.

use sv_icd_entry::PresentTarget;

/// A live display target, opened once per swapchain.
pub enum Target {
    Headless(Dump),
    #[cfg(unix)]
    X11(x11::Window),
    #[cfg(target_os = "linux")]
    Wayland(Box<wayland::Surface>),
}

impl Target {
    /// Open the target a swapchain will present to.
    ///
    /// Failure to reach the window system is **not** an error the application
    /// sees: it falls back to headless, which drops the frame. A swapchain that
    /// refuses to be created because `$DISPLAY` was wrong turns a black window
    /// into an application that will not start, and the first is much easier to
    /// diagnose than the second.
    pub fn open(target: PresentTarget, width: u32, height: u32) -> Target {
        // The size is only consulted by a target that has a window, and on a
        // Windows app side there is not one yet. Said here rather than by
        // renaming the parameters, because the signature is the same on both.
        #[cfg(not(unix))]
        let _ = (width, height);
        match target {
            PresentTarget::Headless => Target::Headless(Dump::from_env()),
            #[cfg(unix)]
            PresentTarget::X11 { window } => match x11::Window::open(window, width, height) {
                Some(w) => Target::X11(w),
                None => {
                    sv_icd_entry::report_transport_failure(&format_args!(
                        "presentation: cannot reach the X server for window {window:#x}; \
                         frames will be dropped"
                    ));
                    Target::Headless(Dump::from_env())
                }
            },
            #[cfg(not(unix))]
            PresentTarget::X11 { .. } => Target::Headless(Dump::from_env()),
            #[cfg(target_os = "linux")]
            PresentTarget::Wayland { display, surface } => {
                match wayland::Surface::open(display, surface, width, height) {
                    Some(s) => Target::Wayland(Box::new(s)),
                    None => {
                        sv_icd_entry::report_transport_failure(&format_args!(
                            "presentation: cannot set up wl_shm on display {display:#x} for \
                             surface {surface:#x}; frames will be dropped"
                        ));
                        Target::Headless(Dump::from_env())
                    }
                }
            }
            #[cfg(not(target_os = "linux"))]
            PresentTarget::Wayland { .. } => Target::Headless(Dump::from_env()),
        }
    }

    /// One frame. `pixels` is `width * height * 4` bytes read straight out of
    /// the arena; `swap_rb` says the application chose an `R8G8B8A8` format and
    /// the bytes are therefore RGBA rather than the BGRA both targets want.
    ///
    /// `generation` is the Wayland surface's shim generation as it was inside the
    /// `vkQueuePresentKHR` of this frame ([`Target::wl_slot`], §100), and
    /// `None` for a frame shown inside that call.
    pub fn present(&mut self, pixels: &[u8], width: u32, height: u32, swap_rb: bool, generation: Option<u32>) {
        #[cfg(not(target_os = "linux"))]
        let _ = generation;
        match self {
            Target::Headless(d) => d.write(pixels, width, height, swap_rb),
            #[cfg(unix)]
            Target::X11(w) => w.put(pixels, width, height, swap_rb),
            #[cfg(target_os = "linux")]
            Target::Wayland(w) => w.put(pixels, width, height, swap_rb, generation),
        }
    }

    /// A Wayland target's slot in `libsv_wlshim.so` (§100): `Some` means a
    /// frame may be shown after `vkQueuePresentKHR` has returned.
    pub fn wl_slot(&self) -> Option<i32> {
        match self {
            #[cfg(target_os = "linux")]
            Target::Wayland(w) => w.slot(),
            _ => None,
        }
    }

    /// The size the surface currently is, or `None` for "no size of its own".
    ///
    /// `None` for Wayland is not a shortcut: a `wl_surface` genuinely has no
    /// size until a buffer is attached to it, which is why the `VK_KHR_surface`
    /// specification requires `currentExtent` to be `(0xFFFFFFFF, 0xFFFFFFFF)`
    /// there and lets the swapchain choose. It is the X11 answer that is the
    /// odd one, and it is the window system that makes it so.
    pub fn extent(target: PresentTarget) -> Option<(u32, u32)> {
        match target {
            PresentTarget::Headless => None,
            #[cfg(unix)]
            PresentTarget::X11 { window } => x11::geometry(window),
            #[cfg(not(unix))]
            PresentTarget::X11 { .. } => None,
            PresentTarget::Wayland { .. } => None,
        }
    }
}

/// `SV_PRESENT_DUMP`: write presented frames out as binary PPM.
///
/// This is the headless target's whole reason to exist beyond "accept and
/// discard". A swapchain that returns `VK_SUCCESS` and drops every frame passes
/// every check an application can make; the only way to know the image was
/// rendered, copied and arrived is to look at it, which is the standard
/// `gl-probe` sets (docs/design.md §0.4).
pub struct Dump {
    prefix: Option<String>,
    max: u32,
    written: u32,
}

impl Dump {
    fn from_env() -> Dump {
        Dump {
            prefix: std::env::var("SV_PRESENT_DUMP").ok().filter(|s| !s.is_empty()),
            // A capped default because a GL program presents sixty times a
            // second and the frames are 8 MB each.
            max: std::env::var("SV_PRESENT_DUMP_MAX")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(4),
            written: 0,
        }
    }

    fn write(&mut self, pixels: &[u8], width: u32, height: u32, swap_rb: bool) {
        let Some(prefix) = self.prefix.as_deref() else { return };
        if self.written >= self.max {
            return;
        }
        let path = format!("{prefix}{:04}.ppm", self.written);
        let mut out = Vec::with_capacity(pixels.len() / 4 * 3 + 32);
        out.extend_from_slice(format!("P6\n{width} {height}\n255\n").as_bytes());
        // BGRA in the arena, RGB in a PPM. Written a pixel at a time rather
        // than with a clever transform because this is a diagnostic path and a
        // wrong channel order here would look exactly like a wrong channel
        // order in the driver.
        let (r, b) = if swap_rb { (0, 2) } else { (2, 0) };
        for px in pixels.chunks_exact(4) {
            out.push(px[r]);
            out.push(px[1]);
            out.push(px[b]);
        }
        if std::fs::write(&path, &out).is_ok() {
            self.written += 1;
        }
    }
}

#[cfg(unix)]
pub mod x11 {
    //! Xlib, through `dlopen`, and only the eleven symbols a `XPutImage` needs.

    use std::ffi::{c_char, c_int, c_uint, c_ulong, c_void};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Mutex, OnceLock};

    /// `ZPixmap`, the only image format anything has used this century.
    const Z_PIXMAP: c_int = 2;

    type Display = c_void;
    type Visual = c_void;
    type XImage = c_void;
    type Gc = *mut c_void;

    struct Lib {
        open_display: unsafe extern "C" fn(*const c_char) -> *mut Display,
        default_screen: unsafe extern "C" fn(*mut Display) -> c_int,
        default_visual: unsafe extern "C" fn(*mut Display, c_int) -> *mut Visual,
        default_depth: unsafe extern "C" fn(*mut Display, c_int) -> c_int,
        create_gc: unsafe extern "C" fn(*mut Display, c_ulong, c_ulong, *mut c_void) -> Gc,
        free_gc: unsafe extern "C" fn(*mut Display, Gc) -> c_int,
        #[allow(clippy::type_complexity)]
        create_image: unsafe extern "C" fn(
            *mut Display,
            *mut Visual,
            c_uint,
            c_int,
            c_int,
            *mut c_char,
            c_uint,
            c_uint,
            c_int,
            c_int,
        ) -> *mut XImage,
        destroy_image: unsafe extern "C" fn(*mut XImage) -> c_int,
        #[allow(clippy::type_complexity)]
        put_image: unsafe extern "C" fn(
            *mut Display,
            c_ulong,
            Gc,
            *mut XImage,
            c_int,
            c_int,
            c_int,
            c_int,
            c_uint,
            c_uint,
        ) -> c_int,
        flush: unsafe extern "C" fn(*mut Display) -> c_int,
        #[allow(clippy::type_complexity)]
        get_geometry: unsafe extern "C" fn(
            *mut Display,
            c_ulong,
            *mut c_ulong,
            *mut c_int,
            *mut c_int,
            *mut c_uint,
            *mut c_uint,
            *mut c_uint,
            *mut c_uint,
        ) -> c_int,
        set_error_handler: unsafe extern "C" fn(ErrorHandler) -> ErrorHandler,
        init_threads: unsafe extern "C" fn() -> c_int,
    }

    type ErrorHandler = Option<unsafe extern "C" fn(*mut Display, *mut c_void) -> c_int>;

    /// Swallow an X error rather than letting Xlib exit the process.
    ///
    /// **This is not optional and it is not defensive coding.** Xlib's default
    /// error handler calls `exit(1)`, and every request this module makes names
    /// a window the *application* owns and may already have destroyed — which is
    /// not a rare race but the ordinary shape of a GL program: `glxinfo` creates
    /// and destroys a window per config it probes, and the first run of this
    /// code killed it with `BadDrawable` from `XCreateGC`. A driver that aborts
    /// the application because a frame could not be shown is worse than one that
    /// drops the frame.
    /// **Ours only.** An error on the application's own connection is passed to
    /// whatever handler the application had, unchanged.
    ///
    /// Xlib's error handler is process-global rather than per-`Display`, which
    /// is the one genuinely awkward thing about talking X from inside a driver.
    /// The handler is nonetheless the only place the *display* is knowable, and
    /// that is enough: `XErrorEvent`'s first member is the `Display*` the error
    /// arrived on, and Xlib passes the same pointer as the first argument. So
    /// "swallow errors this module caused" is expressible exactly, without ever
    /// hiding one of the application's.
    ///
    /// **It takes no lock** (docs/design.md §0.27). Xlib calls it synchronously,
    /// on the thread whose request is reading the connection — and on our
    /// connection that thread is always inside [`Window::put`], [`geometry`] or
    /// a `Drop`, holding the mutex on [`CONN`]. The version that read the
    /// display through that mutex deadlocked on the first error of ours it
    /// handled: Mesa's kopper present thread waiting on itself in
    /// `XFlush` → `_XError` → here, the WebRender thread waiting on kopper's
    /// fence behind it, and Firefox's refresh driver stopped. That was the
    /// local reproduction of "Firefox on X11 through kopper stalls". The
    /// display is now read from [`OUR_DISPLAY`], which needs nothing.
    unsafe extern "C" fn ignore_error(dpy: *mut Display, event: *mut c_void) -> c_int {
        if !dpy.is_null() && OUR_DISPLAY.load(Ordering::Acquire) == dpy as usize {
            // Swallowed, but **not silently**: the stall above was invisible for
            // as long as it was partly because nothing ever said an error had
            // happened. The first few are named, and no more, because a
            // persistent one arrives once per frame.
            if ERRORS_SAID.fetch_add(1, Ordering::Relaxed) < 4 && !event.is_null() {
                // SAFETY: Xlib passes an `XErrorEvent*` as the second argument,
                // valid for the duration of this call.
                let e = unsafe { &*(event as *const XErrorEvent) };
                eprintln!(
                    "sv-icd: X error {} on request {}.{} for resource {:#x}; that frame is dropped",
                    e.error_code, e.request_code, e.minor_code, e.resourceid
                );
            }
            return 0;
        }
        match PREVIOUS.get().copied().flatten() {
            // SAFETY: a plain function pointer `XSetErrorHandler` returned,
            // called with the two arguments Xlib passed us and on the error's
            // own display — which is exactly the call Xlib would have made had
            // this module never installed a handler.
            Some(prev) => unsafe { prev(dpy, event) },
            None => 0,
        }
    }

    /// The handler that was installed when this module first needed one.
    ///
    /// A `OnceLock` and not a save/restore pair, which is the change this
    /// carries and the reason for it: the previous version set the handler,
    /// ran the request, **`XSync`ed**, and restored — because an error is
    /// delivered while its own connection is being read, so the sync was what
    /// made the swap narrow enough to be safe. That sync is a full round trip
    /// to the X server **on every presented frame**, on top of the round trip
    /// to the GPU side that the frame has already cost, and under an Xwayland
    /// that is itself a Wayland client the round trip is not cheap.
    ///
    /// Installing once and dispatching on the display removes the swap, and so
    /// removes the reason for the sync, and is strictly safer than what it
    /// replaces: the old code hid *every* X error in the process for the
    /// duration of each call, including the application's.
    ///
    /// **The one thing it gives up, said plainly**: an application that installs
    /// its own handler *after* this module has installed its own gets its
    /// handler on top of ours, and ours then delegates to whatever was there
    /// before — not to the new one. The old code had the same hole and a
    /// narrower window for it. It is not closable from inside a driver: Xlib
    /// offers no way to observe a handler being replaced, and there is exactly
    /// one global slot. The consequence is bounded — one of *our* errors could
    /// reach an application handler installed after us, which is the behaviour
    /// an application would have had if this module did nothing at all.
    ///
    /// Removing the `XSync` was measured and is **not** why this is here: on
    /// devhost it is worth nothing at all (`docs/firefox-accel.md`, "The
    /// stall, decomposed"). The safety argument above is the whole reason.
    static PREVIOUS: OnceLock<ErrorHandler> = OnceLock::new();

    /// Run `f` with this module's X errors swallowed.
    ///
    /// **This is not optional and it is not defensive coding.** Xlib's default
    /// error handler calls `exit(1)`, and every request this module makes names
    /// a window the *application* owns and may already have destroyed — which is
    /// not a rare race but the ordinary shape of a GL program: `glxinfo` creates
    /// and destroys a window per config it probes, and the first run of this
    /// code killed it with `BadDrawable` from `XCreateGC`. A driver that aborts
    /// the application because a frame could not be shown is worse than one that
    /// drops the frame.
    fn quietly<T>(l: &Lib, dpy: *mut Display, f: impl FnOnce() -> T) -> T {
        let _ = dpy;
        // SAFETY: `XSetErrorHandler` takes and returns a plain function pointer.
        // It runs at most once for the process, before any request this module
        // makes, which is what leaves nothing to restore afterwards.
        PREVIOUS.get_or_init(|| unsafe { (l.set_error_handler)(Some(ignore_error)) });
        f()
    }

    // SAFETY: every field is a function pointer into a library that is loaded
    // for the life of the process (`RTLD_NODELETE` is implied by never calling
    // `dlclose`). Xlib's own thread-safety is a property of the *Display*, not
    // of these symbols, and every use of a display below is behind the mutex on
    // `CONNECTION`.
    unsafe impl Send for Lib {}
    unsafe impl Sync for Lib {}

    // `dlsym` returns `*mut c_void` and the only way to a function pointer is a
    // transmute. Spelling out each target type would mean repeating a signature
    // that is already written once, on the field being assigned, and the second
    // copy is the one that could be wrong — which is the opposite of what the
    // lint is for. `sv-icd-entry` takes the same exemption for the same reason.
    #[allow(clippy::missing_transmute_annotations)]
    fn lib() -> Option<&'static Lib> {
        static LIB: OnceLock<Option<Lib>> = OnceLock::new();
        LIB.get_or_init(|| {
            // SAFETY: `dlopen`/`dlsym` with a literal NUL-terminated name. The
            // handle is never closed, so every symbol stays valid for the life
            // of the process — which is what makes the `'static` above true.
            unsafe {
                let h = libc::dlopen(c"libX11.so.6".as_ptr(), libc::RTLD_LAZY | libc::RTLD_LOCAL);
                if h.is_null() {
                    return None;
                }
                macro_rules! sym {
                    ($name:literal) => {{
                        let p = libc::dlsym(h, concat!($name, "\0").as_ptr() as *const c_char);
                        if p.is_null() {
                            return None;
                        }
                        core::mem::transmute(p)
                    }};
                }
                Some(Lib {
                    open_display: sym!("XOpenDisplay"),
                    default_screen: sym!("XDefaultScreen"),
                    default_visual: sym!("XDefaultVisual"),
                    default_depth: sym!("XDefaultDepth"),
                    create_gc: sym!("XCreateGC"),
                    free_gc: sym!("XFreeGC"),
                    create_image: sym!("XCreateImage"),
                    destroy_image: sym!("XDestroyImage"),
                    put_image: sym!("XPutImage"),
                    flush: sym!("XFlush"),
                    get_geometry: sym!("XGetGeometry"),
                    set_error_handler: sym!("XSetErrorHandler"),
                    init_threads: sym!("XInitThreads"),
                })
            }
        })
        .as_ref()
    }

    /// One connection for the process, opened lazily.
    ///
    /// `Display*` is not `Send`, which is exactly the fact this wrapper exists
    /// to respect: the pointer is kept as a `usize` behind a mutex and is only
    /// ever dereferenced by Xlib itself while that mutex is held.
    struct Connection {
        display: usize,
        screen: c_int,
    }

    /// Module-level rather than a local of [`connection`]. [`ignore_error`]
    /// needs the pointer inside it, and reads it from [`OUR_DISPLAY`] rather than
    /// through this mutex, because it runs while the mutex is held.
    static CONN: OnceLock<Option<Mutex<Connection>>> = OnceLock::new();

    /// The same pointer as `CONN`'s `display`, readable without its mutex,
    /// which is the one thing [`ignore_error`] may not take. Zero until the
    /// connection opens, and a null `Display*` is never ours.
    static OUR_DISPLAY: AtomicUsize = AtomicUsize::new(0);

    /// How many of our own X errors [`ignore_error`] has named.
    static ERRORS_SAID: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

    /// Xlib's `XErrorEvent`, as far as the fields this module reads.
    #[repr(C)]
    struct XErrorEvent {
        type_: c_int,
        display: *mut Display,
        resourceid: c_ulong,
        serial: c_ulong,
        error_code: u8,
        request_code: u8,
        minor_code: u8,
    }

    fn connection() -> Option<&'static Mutex<Connection>> {
        CONN.get_or_init(|| {
            let l = lib()?;
            // SAFETY: `XInitThreads` before the first `XOpenDisplay` is the
            // documented way to make a connection usable from more than one
            // thread, and it is what a library opening its own connection has
            // to do — the application's choice not to call it applies to the
            // application's connection, not to ours.
            let display = unsafe {
                (l.init_threads)();
                (l.open_display)(core::ptr::null())
            };
            if display.is_null() {
                return None;
            }
            // SAFETY: a live display.
            let screen = unsafe { (l.default_screen)(display) };
            OUR_DISPLAY.store(display as usize, Ordering::Release);
            Some(Mutex::new(Connection { display: display as usize, screen }))
        })
        .as_ref()
    }

    /// A window's current size, for `vkGetPhysicalDeviceSurfaceCapabilitiesKHR`.
    pub fn geometry(window: u64) -> Option<(u32, u32)> {
        let l = lib()?;
        let c = connection()?;
        let g = c.lock().unwrap_or_else(|e| e.into_inner());
        let dpy = g.display as *mut Display;
        let mut root: c_ulong = 0;
        let (mut x, mut y) = (0 as c_int, 0 as c_int);
        let (mut w, mut h, mut b, mut d) = (0 as c_uint, 0, 0, 0);
        // SAFETY: a live display and eight writable out-parameters. `XGetGeometry`
        // is used rather than `XGetWindowAttributes` precisely because its
        // answers come back in separate `int*`s: there is no struct whose
        // layout this crate would then have to know.
        let ok = quietly(l, dpy, || unsafe {
            (l.get_geometry)(
                dpy,
                window as c_ulong,
                &mut root,
                &mut x,
                &mut y,
                &mut w,
                &mut h,
                &mut b,
                &mut d,
            )
        });
        if ok == 0 || w == 0 || h == 0 {
            return None;
        }
        // `c_uint` is `u32` on every target this builds for, so the cast the
        // out-parameter's type would suggest is a no-op the lint objects to.
        Some((w, h))
    }

    /// A window plus the `XImage` and `GC` that put pixels into it.
    pub struct Window {
        id: c_ulong,
        gc: usize,
        image: usize,
        /// `malloc`ed rather than a `Vec`, because `XDestroyImage` frees the
        /// image's `data` with `XFree` and there is no supported way to tell it
        /// not to. Giving it memory it can free is simpler and less fragile than
        /// reaching into `XImage`'s layout to null the field first.
        pixels: *mut u8,
        len: usize,
        width: u32,
        height: u32,
    }

    // SAFETY: the two `usize` handles and the `malloc`ed buffer are only touched
    // under the `CONNECTION` mutex or by `&mut self`, and the buffer is owned
    // outright by this object until `Drop` hands it to `XDestroyImage`.
    unsafe impl Send for Window {}

    impl Window {
        pub fn open(window: u64, width: u32, height: u32) -> Option<Window> {
            let l = lib()?;
            let c = connection()?;
            let g = c.lock().unwrap_or_else(|e| e.into_inner());
            let dpy = g.display as *mut Display;
            let len = width as usize * height as usize * 4;
            // SAFETY: Xlib calls on a live display, with a buffer `XCreateImage`
            // takes ownership of. The visual is the screen's default, which on
            // every X server this can reach is 24-bit TrueColor with the
            // channels in the order `VK_FORMAT_B8G8R8A8_UNORM` already has them
            // — which is why the copy in `put` is a `memcpy` and not a shuffle.
            //
            // **The depth is the window's, not the screen's** (docs/design.md
            // §0.27). `XPutImage` requires the image's depth to equal the
            // drawable's, and Firefox's toplevel is not at the screen's default
            // depth: every frame was `BadMatch` (error 8) on `X_PutImage`
            // (request 72), swallowed by `ignore_error`, and the window stayed
            // black while everything behind it rendered. A 32-bit TrueColor
            // visual has the same channel masks as the 24-bit one with alpha in
            // the top byte, which is where `B8G8R8A8` keeps it, so the default
            // visual still describes the pixels.
            quietly(l, dpy, || unsafe {
                let pixels = libc::malloc(len.max(1)) as *mut u8;
                if pixels.is_null() {
                    return None;
                }
                let visual = (l.default_visual)(dpy, g.screen);
                let mut depth = (l.default_depth)(dpy, g.screen) as c_uint;
                let mut root: c_ulong = 0;
                let (mut x, mut y) = (0 as c_int, 0 as c_int);
                let (mut w, mut h, mut b, mut d) = (0 as c_uint, 0, 0, 0);
                let ok = (l.get_geometry)(
                    dpy,
                    window as c_ulong,
                    &mut root,
                    &mut x,
                    &mut y,
                    &mut w,
                    &mut h,
                    &mut b,
                    &mut d,
                );
                if ok != 0 && (d == 24 || d == 32) {
                    depth = d;
                }
                let image = (l.create_image)(
                    dpy,
                    visual,
                    depth,
                    Z_PIXMAP,
                    0,
                    pixels as *mut c_char,
                    width as c_uint,
                    height as c_uint,
                    32,
                    0,
                );
                if image.is_null() {
                    libc::free(pixels as *mut c_void);
                    return None;
                }
                let gc = (l.create_gc)(dpy, window as c_ulong, 0, core::ptr::null_mut());
                Some(Window {
                    id: window as c_ulong,
                    gc: gc as usize,
                    image: image as usize,
                    pixels,
                    len,
                    width,
                    height,
                })
            })
        }

        pub fn put(&mut self, src: &[u8], width: u32, height: u32, swap_rb: bool) {
            // A window resized between swapchain creation and this frame is the
            // application's problem to notice — the surface reports its new
            // `currentExtent` and a well-behaved one rebuilds — but it must not
            // be ours to fault on.
            if width != self.width || height != self.height || src.len() < self.len {
                return;
            }
            let (Some(l), Some(c)) = (lib(), connection()) else { return };
            let g = c.lock().unwrap_or_else(|e| e.into_inner());
            let dpy = g.display as *mut Display;
            // SAFETY: `self.pixels` is `self.len` bytes this object owns, and
            // `src` is at least that long by the check above.
            unsafe {
                if swap_rb {
                    for i in (0..self.len).step_by(4) {
                        *self.pixels.add(i) = src[i + 2];
                        *self.pixels.add(i + 1) = src[i + 1];
                        *self.pixels.add(i + 2) = src[i];
                        *self.pixels.add(i + 3) = src[i + 3];
                    }
                } else {
                    core::ptr::copy_nonoverlapping(src.as_ptr(), self.pixels, self.len);
                }
            }
            // `XFlush` and **not** `XSync`: the frame is pushed to the server
            // and this side does not wait to hear that it arrived. `quietly`
            // used to end in an `XSync` — a full round trip per frame, on top
            // of the round trip to the GPU side the frame has already cost —
            // because it swapped the process-global error handler around each
            // call and had to know its own errors had landed before swapping
            // back. Installing the handler once removed the swap and with it
            // the reason to wait. See [`PREVIOUS`].
            // SAFETY: handles this object created, on a live display.
            quietly(l, dpy, || unsafe {
                (l.put_image)(
                    dpy,
                    self.id,
                    self.gc as Gc,
                    self.image as *mut XImage,
                    0,
                    0,
                    0,
                    0,
                    self.width as c_uint,
                    self.height as c_uint,
                );
                (l.flush)(dpy);
            });
        }
    }

    impl Drop for Window {
        fn drop(&mut self) {
            let (Some(l), Some(c)) = (lib(), connection()) else { return };
            let g = c.lock().unwrap_or_else(|e| e.into_inner());
            let dpy = g.display as *mut Display;
            // SAFETY: handles this object created and has not yet released.
            // `XDestroyImage` frees `self.pixels`, which is why it is not freed
            // here as well.
            quietly(l, dpy, || unsafe {
                (l.free_gc)(dpy, self.gc as Gc);
                (l.destroy_image)(self.image as *mut XImage);
            })
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use std::sync::mpsc;
        use std::time::Duration;

        /// Xlib runs the error handler on the thread whose request is reading
        /// the connection, and on ours that thread holds the connection's
        /// mutex. So the handler has to answer "is this ours?" **while that
        /// mutex is held** (docs/design.md §0.27). The version that read the
        /// display through the mutex never returned from this test.
        ///
        /// The connection is a fake one — a pointer that is compared and never
        /// handed to Xlib — so the test needs no X server. Nothing else in this
        /// crate's unit tests opens an X connection, which is what makes
        /// setting `CONN` here safe.
        #[test]
        fn the_error_handler_answers_while_the_connection_is_locked() {
            let fake = 0x5eed_0000usize;
            let _ = CONN.set(Some(Mutex::new(Connection { display: fake, screen: 0 })));
            OUR_DISPLAY.store(fake, Ordering::Release);
            let (tx, rx) = mpsc::channel();
            std::thread::spawn(move || {
                let c = CONN.get().and_then(|c| c.as_ref()).expect("the fake connection");
                // What `Window::put` holds while `XFlush` delivers an error.
                let _held = c.lock().unwrap_or_else(|e| e.into_inner());
                // SAFETY: the handler only compares the pointer for a display
                // that is ours, and never dereferences `event` on that path.
                let r = unsafe { ignore_error(fake as *mut Display, core::ptr::null_mut()) };
                let _ = tx.send(r);
            });
            assert_eq!(
                rx.recv_timeout(Duration::from_secs(10)),
                Ok(0),
                "the handler did not return: it is waiting for the lock its own caller holds"
            );
        }
    }
}

#[cfg(target_os = "linux")]
pub mod wayland {
    //! `VK_KHR_wayland_surface`, as a `wl_shm` buffer on the application's own
    //! connection.
    //!
    //! # Why this is not the usual Vulkan Wayland WSI, and cannot be
    //!
    //! A Wayland WSI normally means the driver creates dmabufs and attaches
    //! them, so that the compositor imports the GPU's own pages and nothing is
    //! copied. **This driver may not do that and it is not a limitation to be
    //! removed.** docs/design.md §1a forbids any file descriptor crossing the VM
    //! boundary, and a dmabuf is both a kernel object and a descriptor; on the
    //! topology this exists for the GPU is in a Windows guest and the host has
    //! no DRM device at all, so there is nothing to make one from either.
    //!
    //! What is left is the path `wc-proxy` already offers and Firefox already
    //! uses in software: `wl_shm`. The frame's route is unchanged from
    //! [`super::x11`]'s in shape — the GPU side's `vkCmdCopyImageToBuffer` puts
    //! it in the arena, and this side copies it into a buffer the compositor
    //! reads — and the *point* of the extension is not the copy count at all.
    //! It is that Mesa's `zink_kopper` cannot make a `VkSurfaceKHR` on the
    //! Wayland platform without it, so Mesa loads swrast with Zink merely
    //! behind it, and Firefox's hardware WebRender rejects the EGL configs that
    //! produces (`docs/firefox-accel.md`).
    //!
    //! **No descriptor crosses the boundary here.** The `memfd` below is
    //! created by this process, sent over the *host's* Wayland socket to
    //! `wc-proxy`, which is another process on the same host, and consumed
    //! there: `wc_proxy::pool` copies its bytes into the arena and passes a
    //! `(slab, offset, len)` triple onward. The rule is about the boundary, not
    //! about descriptors as such (`wc_proxy::pool`'s own header says the same
    //! in the other direction).
    //!
    //! # Why the application's connection, and not one of our own
    //!
    //! [`super::x11`] opens its own connection precisely so that it is not
    //! inside somebody else's locking discipline, and that is possible because
    //! **an X window id is global to the display**. A `wl_surface` is not: it is
    //! a proxy, private to one connection, with no name a second connection
    //! could resolve. So there is no choice — the only way to put a frame on the
    //! application's surface is to speak on the application's `wl_display`.
    //!
    //! Two things make that safe, and they are the same two libwayland gives
    //! every library that has to do it:
    //!
    //! - **an event queue of our own** ([`wl_display_create_queue`]), so that
    //!   dispatching our `wl_buffer.release` events cannot consume an event the
    //!   application was waiting for. Every object created here is created
    //!   through a proxy wrapper assigned to that queue, so it inherits it.
    //! - **libwayland's own thread safety**, which covers `wl_proxy_marshal_*`
    //!   and the display, unlike Xlib's, which is conditional on the
    //!   application having called `XInitThreads`.
    //!
    //! # Why the protocol is written out by hand
    //!
    //! The request opcodes below are spelled as constants and the messages are
    //! sent with [`wl_proxy_marshal_array_flags`], rather than by linking
    //! `libwayland-client` and calling `wl_surface_attach`. Three reasons, the
    //! same three [`super::x11`] gives for `dlopen`ing Xlib and one more: the
    //! ICD is a `cdylib` the loader opens into arbitrary processes and must not
    //! carry a `DT_NEEDED` on a library a Windows or headless host has no
    //! reason to own; the functions that would be linked are `static inline` in
    //! `wayland-client-protocol.h`, so linking would not even provide them —
    //! only `wl_proxy_marshal_array_flags` is an exported symbol; and the ICD
    //! is loaded *into* a process that already has libwayland open whenever the
    //! surface is a Wayland surface, so `dlopen` finds the copy already there,
    //! which is the copy whose proxies these are.
    //!
    //! The array form is used rather than the variadic `wl_proxy_marshal_flags`
    //! because a `union wl_argument[]` is a thing Rust can build exactly, and a
    //! C variadic call is a thing it can only build by convention.

    use std::ffi::{c_char, c_int, c_void};
    use std::sync::OnceLock;
    use std::sync::atomic::{AtomicBool, Ordering};

    /// Request opcodes, from `wayland.xml`. These are protocol constants and
    /// cannot change: a Wayland interface may only ever *append* requests.
    const WL_DISPLAY_GET_REGISTRY: u32 = 1;
    const WL_REGISTRY_BIND: u32 = 0;
    const WL_SHM_CREATE_POOL: u32 = 0;
    const WL_SHM_POOL_CREATE_BUFFER: u32 = 0;
    const WL_SHM_POOL_DESTROY: u32 = 1;
    const WL_BUFFER_DESTROY: u32 = 0;
    const WL_SURFACE_ATTACH: u32 = 1;
    const WL_SURFACE_DAMAGE: u32 = 2;
    const WL_SURFACE_COMMIT: u32 = 6;
    const WL_SURFACE_DAMAGE_BUFFER: u32 = 9;
    /// `wl_surface.damage_buffer` arrived in `wl_surface` version 4.
    const DAMAGE_BUFFER_SINCE: u32 = 4;
    /// `WL_MARSHAL_FLAG_DESTROY`.
    const MARSHAL_FLAG_DESTROY: u32 = 1;
    /// `WL_SHM_FORMAT_XRGB8888`: little-endian `0xXXRRGGBB`, which is B, G, R,
    /// X in memory order — the byte order `VK_FORMAT_B8G8R8A8_UNORM` already
    /// has, so the copy below is a `memcpy`.
    ///
    /// Opaque rather than `ARGB8888` because a swapchain image's alpha channel
    /// is whatever the application's last draw left there, and a compositor
    /// that honours it turns an unrelated bug into a see-through window. Both
    /// formats are mandatory for every `wl_shm`, so neither has to be
    /// negotiated.
    const WL_SHM_FORMAT_XRGB8888: u32 = 1;

    /// How many `wl_shm` buffers a swapchain cycles through.
    ///
    /// Three, and the number is about the *compositor* rather than about this
    /// driver: frames are put one at a time (by the swapchain's presenter,
    /// `crate::wsi`), so one buffer would be correct were it not that a
    /// committed buffer stays the compositor's until it sends
    /// `wl_buffer.release`, and writing into it before then is a visible tear.
    /// Three is one being shown, one in flight and one to write. On the guest
    /// the release crosses ssh.exe and arrives a frame or so late
    /// (docs/waycompose-design.md §88); three still waited once in 960 frames.
    const BUFFERS: usize = 3;

    /// `union wl_argument`, one 64-bit slot per argument.
    ///
    /// Constructed through the helpers below rather than directly, so that
    /// every one of the eight bytes is written even when the argument is a
    /// 32-bit integer: libwayland reads only the member the message signature
    /// names, but a partially initialised value passed across FFI is a thing to
    /// avoid on principle rather than on evidence.
    #[repr(C)]
    #[derive(Clone, Copy)]
    union Arg {
        i: i32,
        u: u32,
        s: *const c_char,
        o: *mut c_void,
        h: i32,
    }

    fn zeroed() -> Arg {
        Arg { o: core::ptr::null_mut() }
    }
    fn arg_i(v: i32) -> Arg {
        let mut a = zeroed();
        a.i = v;
        a
    }
    fn arg_u(v: u32) -> Arg {
        let mut a = zeroed();
        a.u = v;
        a
    }
    fn arg_h(v: c_int) -> Arg {
        let mut a = zeroed();
        a.h = v;
        a
    }
    fn arg_o(v: *mut c_void) -> Arg {
        Arg { o: v }
    }
    fn arg_s(v: *const c_char) -> Arg {
        Arg { s: v }
    }
    /// The placeholder slot a `new_id` argument occupies. libwayland fills in
    /// the id it allocates; the value here is never read.
    fn arg_new() -> Arg {
        zeroed()
    }

    /// `struct wl_interface`. Only `name` is read, by `wl_registry.bind`, whose
    /// message signature is `usun` — the interface name goes on the wire as a
    /// string because the compositor has no other way to know what is being
    /// bound.
    #[repr(C)]
    struct WlInterface {
        name: *const c_char,
        version: c_int,
        method_count: c_int,
        methods: *const c_void,
        event_count: c_int,
        events: *const c_void,
    }

    #[repr(C)]
    struct RegistryListener {
        global: unsafe extern "C" fn(*mut c_void, *mut c_void, u32, *const c_char, u32),
        global_remove: unsafe extern "C" fn(*mut c_void, *mut c_void, u32),
    }

    #[repr(C)]
    struct BufferListener {
        release: unsafe extern "C" fn(*mut c_void, *mut c_void),
    }

    struct Lib {
        display_create_queue: unsafe extern "C" fn(*mut c_void) -> *mut c_void,
        event_queue_destroy: unsafe extern "C" fn(*mut c_void),
        proxy_create_wrapper: unsafe extern "C" fn(*mut c_void) -> *mut c_void,
        proxy_wrapper_destroy: unsafe extern "C" fn(*mut c_void),
        proxy_set_queue: unsafe extern "C" fn(*mut c_void, *mut c_void),
        #[allow(clippy::type_complexity)]
        proxy_marshal_array_flags: unsafe extern "C" fn(
            *mut c_void,
            u32,
            *const WlInterface,
            u32,
            u32,
            *mut Arg,
        ) -> *mut c_void,
        proxy_add_listener:
            unsafe extern "C" fn(*mut c_void, *const c_void, *mut c_void) -> c_int,
        proxy_destroy: unsafe extern "C" fn(*mut c_void),
        proxy_get_version: unsafe extern "C" fn(*mut c_void) -> u32,
        proxy_get_display: unsafe extern "C" fn(*mut c_void) -> *mut c_void,
        display_roundtrip_queue: unsafe extern "C" fn(*mut c_void, *mut c_void) -> c_int,
        display_dispatch_queue_pending: unsafe extern "C" fn(*mut c_void, *mut c_void) -> c_int,
        display_flush: unsafe extern "C" fn(*mut c_void) -> c_int,
        registry_interface: *const WlInterface,
        shm_interface: *const WlInterface,
        shm_pool_interface: *const WlInterface,
        buffer_interface: *const WlInterface,
    }

    // SAFETY: every field is a function pointer or a `const` data symbol in a
    // library that is loaded for the life of the process — `dlclose` is never
    // called. libwayland's own thread safety is documented and covers every
    // entry point used here; the four interfaces are immutable.
    unsafe impl Send for Lib {}
    unsafe impl Sync for Lib {}

    // The same exemption `sv-icd-entry` and [`super::x11`] take, for the same
    // reason: `dlsym` returns `*mut c_void` and the only route to a function
    // pointer is a transmute, whose target type is already written once on the
    // field being assigned. A second copy in the annotation is the one that
    // could be wrong.
    #[allow(clippy::missing_transmute_annotations)]
    fn lib() -> Option<&'static Lib> {
        static LIB: OnceLock<Option<Lib>> = OnceLock::new();
        LIB.get_or_init(|| {
            // SAFETY: `dlopen`/`dlsym` with literal NUL-terminated names. The
            // handle is never closed, which is what makes the `'static` true.
            // `RTLD_NOLOAD` is deliberately *not* used: the ICD may be the
            // first thing in the process to need libwayland only in the
            // pathological case, and an ordinary `dlopen` finds the copy the
            // application already has open when there is one.
            unsafe {
                let h = libc::dlopen(
                    c"libwayland-client.so.0".as_ptr(),
                    libc::RTLD_LAZY | libc::RTLD_LOCAL,
                );
                if h.is_null() {
                    return None;
                }
                macro_rules! sym {
                    ($name:literal) => {{
                        let p = libc::dlsym(h, concat!($name, "\0").as_ptr() as *const c_char);
                        if p.is_null() {
                            return None;
                        }
                        core::mem::transmute(p)
                    }};
                }
                macro_rules! iface {
                    ($name:literal) => {{
                        let p = libc::dlsym(h, concat!($name, "\0").as_ptr() as *const c_char);
                        if p.is_null() {
                            return None;
                        }
                        p as *const WlInterface
                    }};
                }
                Some(Lib {
                    display_create_queue: sym!("wl_display_create_queue"),
                    event_queue_destroy: sym!("wl_event_queue_destroy"),
                    proxy_create_wrapper: sym!("wl_proxy_create_wrapper"),
                    proxy_wrapper_destroy: sym!("wl_proxy_wrapper_destroy"),
                    proxy_set_queue: sym!("wl_proxy_set_queue"),
                    proxy_marshal_array_flags: sym!("wl_proxy_marshal_array_flags"),
                    proxy_add_listener: sym!("wl_proxy_add_listener"),
                    proxy_destroy: sym!("wl_proxy_destroy"),
                    proxy_get_version: sym!("wl_proxy_get_version"),
                    proxy_get_display: sym!("wl_proxy_get_display"),
                    display_roundtrip_queue: sym!("wl_display_roundtrip_queue"),
                    display_dispatch_queue_pending: sym!("wl_display_dispatch_queue_pending"),
                    display_flush: sym!("wl_display_flush"),
                    registry_interface: iface!("wl_registry_interface"),
                    shm_interface: iface!("wl_shm_interface"),
                    shm_pool_interface: iface!("wl_shm_pool_interface"),
                    buffer_interface: iface!("wl_buffer_interface"),
                })
            }
        })
        .as_ref()
    }

    /// What the registry sweep is looking for, and where the listener puts it.
    struct Found {
        shm_name: u32,
        shm_version: u32,
    }

    unsafe extern "C" fn on_global(
        data: *mut c_void,
        _registry: *mut c_void,
        name: u32,
        interface: *const c_char,
        version: u32,
    ) {
        if data.is_null() || interface.is_null() {
            return;
        }
        // SAFETY: `interface` is a NUL-terminated string libwayland owns for
        // the duration of the callback, and `data` is the `Found` the caller
        // passed to `wl_proxy_add_listener`, alive until the registry proxy is
        // destroyed at the end of `open`.
        unsafe {
            if core::ffi::CStr::from_ptr(interface).to_bytes() == b"wl_shm" {
                let found = &mut *(data as *mut Found);
                found.shm_name = name;
                found.shm_version = version;
            }
        }
    }

    unsafe extern "C" fn on_global_remove(_data: *mut c_void, _registry: *mut c_void, _name: u32) {}

    /// `wl_buffer.release`: the compositor is finished with this buffer.
    unsafe extern "C" fn on_release(data: *mut c_void, _buffer: *mut c_void) {
        if data.is_null() {
            return;
        }
        // SAFETY: `data` is the `AtomicBool` this buffer's slot owns, boxed so
        // that its address does not move, and dropped only after the buffer
        // proxy is destroyed.
        unsafe { (*(data as *const AtomicBool)).store(false, Ordering::Release) };
    }

    static REGISTRY_LISTENER: RegistryListener =
        RegistryListener { global: on_global, global_remove: on_global_remove };
    static BUFFER_LISTENER: BufferListener = BufferListener { release: on_release };

    /// One `wl_shm` buffer of the ring, and whether the compositor still has it.
    struct Buf {
        proxy: *mut c_void,
        offset: usize,
        /// Boxed for address stability: the pointer is handed to libwayland as
        /// the listener's `data` and must outlive every event.
        busy: Box<AtomicBool>,
    }

    /// Everything one swapchain needs to put frames on one `wl_surface`.
    pub struct Surface {
        display: *mut c_void,
        surface: *mut c_void,
        surface_version: u32,
        queue: *mut c_void,
        shm: *mut c_void,
        pool: *mut c_void,
        map: *mut u8,
        map_len: usize,
        buffers: Vec<Buf>,
        width: u32,
        height: u32,
        next: usize,
        /// This surface's slot in `libsv_wlshim.so`, when the application
        /// was started with it (docs/waycompose-design.md §100). `Some` is
        /// what makes a present after `vkQueuePresentKHR` safe.
        slot: Option<i32>,
    }

    // SAFETY: every pointer is a libwayland object, and libwayland is
    // thread-safe for all of them. The one thing that is *not* safe to do from
    // two threads at once is dispatch the same event queue, and this queue is
    // reachable only through `&mut self` — the swapchain's target is behind a
    // mutex that `crate::wsi` holds across the whole of `present`.
    unsafe impl Send for Surface {}

    impl Surface {
        /// Bind `wl_shm` on a queue of our own and build the buffer ring.
        ///
        /// Every step that can fail returns `None`, and the caller falls back
        /// to the headless target rather than failing swapchain creation — the
        /// same choice, for the same reason, that [`super::Target::open`] makes
        /// for X11.
        pub fn open(display: usize, surface: usize, width: u32, height: u32) -> Option<Surface> {
            let l = lib()?;
            if width == 0 || height == 0 {
                return None;
            }
            let app_display = display as *mut c_void;
            let surface = surface as *mut c_void;

            // SAFETY: from here to the end of the function, every call is a
            // libwayland entry point on a proxy this block owns or on the
            // application's display, which the loader guarantees is live for
            // the lifetime of the `VkSurfaceKHR`. The argument arrays match the
            // signatures in `wayland.xml` for the opcodes named above.
            unsafe {
                // Mesa hands `vkCreateWaylandSurfaceKHR` whichever `wl_display`
                // its EGL display holds, and that may be a proxy *wrapper*
                // rather than the display itself. `wl_display_create_queue` and
                // `wl_display_roundtrip_queue` want the real one, and
                // `wl_proxy_get_display` is the only thing that reliably says
                // which that is — for a real display it returns itself.
                let display = (l.proxy_get_display)(app_display);
                if display.is_null() {
                    return None;
                }

                let queue = (l.display_create_queue)(display);
                if queue.is_null() {
                    return None;
                }

                // The wrapper exists for exactly one request: it makes the
                // registry — and so everything bound from it — belong to our
                // queue rather than to the application's default one.
                let wrapper = (l.proxy_create_wrapper)(display);
                if wrapper.is_null() {
                    (l.event_queue_destroy)(queue);
                    return None;
                }
                (l.proxy_set_queue)(wrapper, queue);
                let mut args = [arg_new()];
                let registry = (l.proxy_marshal_array_flags)(
                    wrapper,
                    WL_DISPLAY_GET_REGISTRY,
                    l.registry_interface,
                    (l.proxy_get_version)(wrapper),
                    0,
                    args.as_mut_ptr(),
                );
                (l.proxy_wrapper_destroy)(wrapper);
                if registry.is_null() {
                    (l.event_queue_destroy)(queue);
                    return None;
                }

                let mut found = Found { shm_name: 0, shm_version: 0 };
                (l.proxy_add_listener)(
                    registry,
                    &REGISTRY_LISTENER as *const RegistryListener as *const c_void,
                    &mut found as *mut Found as *mut c_void,
                );
                // One round trip is enough and is the whole cost of setting
                // this up: the compositor sends every `global` before it
                // answers the sync.
                if (l.display_roundtrip_queue)(display, queue) < 0 || found.shm_version == 0 {
                    (l.proxy_destroy)(registry);
                    (l.event_queue_destroy)(queue);
                    return None;
                }

                // Version 1 is all this needs — `create_pool` and the two
                // mandatory formats have been there since the beginning — so
                // asking for more would only be a way to fail against an older
                // compositor.
                let shm_name = (*l.shm_interface).name;
                let mut args = [
                    arg_u(found.shm_name),
                    arg_s(shm_name),
                    arg_u(1),
                    arg_new(),
                ];
                let shm = (l.proxy_marshal_array_flags)(
                    registry,
                    WL_REGISTRY_BIND,
                    l.shm_interface,
                    1,
                    0,
                    args.as_mut_ptr(),
                );
                // The listener's `data` is a stack local, so the registry must
                // not outlive this function.
                (l.proxy_destroy)(registry);
                if shm.is_null() {
                    (l.event_queue_destroy)(queue);
                    return None;
                }

                let stride = width as usize * 4;
                let frame = stride * height as usize;
                let map_len = frame * BUFFERS;

                // **The descriptor that does not cross.** It is created here,
                // sent to `wc-proxy` on this host, and closed here; what
                // reaches the guest is `(slab, offset, len)`. See this module's
                // header.
                let fd = libc::memfd_create(c"sv-icd-swapchain".as_ptr(), libc::MFD_CLOEXEC);
                if fd < 0 {
                    (l.proxy_destroy)(shm);
                    (l.event_queue_destroy)(queue);
                    return None;
                }
                if libc::ftruncate(fd, map_len as libc::off_t) != 0 {
                    libc::close(fd);
                    (l.proxy_destroy)(shm);
                    (l.event_queue_destroy)(queue);
                    return None;
                }
                let map = libc::mmap(
                    core::ptr::null_mut(),
                    map_len,
                    libc::PROT_READ | libc::PROT_WRITE,
                    libc::MAP_SHARED,
                    fd,
                    0,
                );
                if map == libc::MAP_FAILED {
                    libc::close(fd);
                    (l.proxy_destroy)(shm);
                    (l.event_queue_destroy)(queue);
                    return None;
                }

                let mut args = [arg_new(), arg_h(fd), arg_i(map_len as i32)];
                let pool = (l.proxy_marshal_array_flags)(
                    shm,
                    WL_SHM_CREATE_POOL,
                    l.shm_pool_interface,
                    (l.proxy_get_version)(shm),
                    0,
                    args.as_mut_ptr(),
                );
                // libwayland `dup`s the descriptor into its own outgoing queue,
                // so this one is ours to close and closing it is not optional:
                // a swapchain rebuilt on every window resize would otherwise
                // leak a descriptor per rebuild.
                libc::close(fd);
                if pool.is_null() {
                    libc::munmap(map, map_len);
                    (l.proxy_destroy)(shm);
                    (l.event_queue_destroy)(queue);
                    return None;
                }

                let mut buffers = Vec::with_capacity(BUFFERS);
                for i in 0..BUFFERS {
                    let offset = i * frame;
                    let mut args = [
                        arg_new(),
                        arg_i(offset as i32),
                        arg_i(width as i32),
                        arg_i(height as i32),
                        arg_i(stride as i32),
                        arg_u(WL_SHM_FORMAT_XRGB8888),
                    ];
                    let b = (l.proxy_marshal_array_flags)(
                        pool,
                        WL_SHM_POOL_CREATE_BUFFER,
                        l.buffer_interface,
                        (l.proxy_get_version)(pool),
                        0,
                        args.as_mut_ptr(),
                    );
                    if b.is_null() {
                        break;
                    }
                    let busy = Box::new(AtomicBool::new(false));
                    (l.proxy_add_listener)(
                        b,
                        &BUFFER_LISTENER as *const BufferListener as *const c_void,
                        &*busy as *const AtomicBool as *mut c_void,
                    );
                    buffers.push(Buf { proxy: b, offset, busy });
                }
                if buffers.is_empty() {
                    libc::munmap(map, map_len);
                    let mut none: [Arg; 0] = [];
                    (l.proxy_marshal_array_flags)(
                        pool,
                        WL_SHM_POOL_DESTROY,
                        core::ptr::null(),
                        (l.proxy_get_version)(pool),
                        MARSHAL_FLAG_DESTROY,
                        none.as_mut_ptr(),
                    );
                    (l.proxy_destroy)(shm);
                    (l.event_queue_destroy)(queue);
                    return None;
                }

                sv_icd_entry::report_transport_failure(&format_args!(
                    "presentation: wl_shm ready on the application's display — \
                     {width}x{height}, {} buffer(s), {map_len} bytes",
                    buffers.len()
                ));

                Some(Surface {
                    display,
                    surface,
                    surface_version: (l.proxy_get_version)(surface),
                    queue,
                    shm,
                    pool,
                    map: map as *mut u8,
                    map_len,
                    buffers,
                    width,
                    height,
                    next: 0,
                    // Registered here, inside the application's
                    // vkCreateSwapchainKHR, while the surface is known alive.
                    slot: guard::track(surface),
                })
            }
        }

        /// This surface's slot in `libsv_wlshim.so`; `None` without it.
        pub fn slot(&self) -> Option<i32> {
            self.slot
        }

        /// One frame: copy the arena bytes into a free buffer, attach, damage,
        /// commit. `generation` is [`Surface::generation`] as it was inside the
        /// `vkQueuePresentKHR` that presented it: with the shim, the frame is
        /// dropped if the application has destroyed or unmapped the surface
        /// since, and its requests are made under the shim's lock, so the
        /// application cannot destroy the surface in the middle of them.
        pub fn put(&mut self, src: &[u8], width: u32, height: u32, swap_rb: bool, generation: Option<u32>) {
            // A resize between swapchain creation and this frame is the
            // application's to notice, and Mesa does — it rebuilds. Dropping
            // the frame is the only thing that is safe here, because the
            // buffers are the old size.
            let frame = self.width as usize * self.height as usize * 4;
            if width != self.width || height != self.height || src.len() < frame {
                return;
            }
            let Some(l) = lib() else { return };

            // SAFETY: libwayland entry points on this object's own proxies and
            // its own queue, and a write into this object's own mapping at an
            // offset the constructor established is `frame` bytes long.
            let traced = sv_transport::frametrace::enabled().then(std::time::Instant::now);
            let mut waited = false;
            let (picked, copied, buf_i): (Option<std::time::Instant>, Option<std::time::Instant>, usize);
            unsafe {
                // Free, without blocking: whatever releases the compositor has
                // already sent are sitting in our queue.
                (l.display_dispatch_queue_pending)(self.display, self.queue);
                let mut idx = self.pick();
                if idx.is_none() {
                    waited = true;
                    // Every buffer is still the compositor's. One round trip is
                    // the backpressure — it returns as soon as the compositor
                    // answers a `wl_display.sync`, so it cannot wait on a frame
                    // that will never come, which is what makes this safe where
                    // waiting on `wl_surface.frame` would not be.
                    (l.display_flush)(self.display);
                    (l.display_roundtrip_queue)(self.display, self.queue);
                    idx = self.pick();
                }
                // Still nothing: rather than drop the frame, take the next in
                // the ring. The worst case is one torn frame; a swapchain that
                // silently stops presenting is much harder to diagnose.
                let i = idx.unwrap_or(self.next % self.buffers.len());
                self.next = (i + 1) % self.buffers.len();
                picked = traced.map(|_| std::time::Instant::now());
                buf_i = i;

                let dst = self.map.add(self.buffers[i].offset);
                if swap_rb {
                    for p in (0..frame).step_by(4) {
                        *dst.add(p) = src[p + 2];
                        *dst.add(p + 1) = src[p + 1];
                        *dst.add(p + 2) = src[p];
                        *dst.add(p + 3) = src[p + 3];
                    }
                } else {
                    core::ptr::copy_nonoverlapping(src.as_ptr(), dst, frame);
                }
                copied = traced.map(|_| std::time::Instant::now());
                self.buffers[i].busy.store(true, Ordering::Release);

                // From here to the commit, the requests go to the
                // application's surface. With the shim, under its lock, and
                // not at all if the surface died or was unmapped after the
                // application presented this frame (§100).
                let guarded = match (generation, self.slot) {
                    (Some(g), Some(slot)) => {
                        if !guard::begin(slot, g) {
                            self.buffers[i].busy.store(false, Ordering::Release);
                            return;
                        }
                        true
                    }
                    _ => false,
                };

                let mut args = [arg_o(self.buffers[i].proxy), arg_i(0), arg_i(0)];
                (l.proxy_marshal_array_flags)(
                    self.surface,
                    WL_SURFACE_ATTACH,
                    core::ptr::null(),
                    self.surface_version,
                    0,
                    args.as_mut_ptr(),
                );

                // `damage_buffer` is in buffer coordinates and `damage` in
                // surface coordinates. They differ the moment the surface has a
                // scale, and the whole buffer is what changed either way, so
                // the newer one is used where it exists.
                let (op, w, h) = if self.surface_version >= DAMAGE_BUFFER_SINCE {
                    (WL_SURFACE_DAMAGE_BUFFER, self.width as i32, self.height as i32)
                } else {
                    (WL_SURFACE_DAMAGE, i32::MAX, i32::MAX)
                };
                let mut args = [arg_i(0), arg_i(0), arg_i(w), arg_i(h)];
                (l.proxy_marshal_array_flags)(
                    self.surface,
                    op,
                    core::ptr::null(),
                    self.surface_version,
                    0,
                    args.as_mut_ptr(),
                );

                let mut none: [Arg; 0] = [];
                (l.proxy_marshal_array_flags)(
                    self.surface,
                    WL_SURFACE_COMMIT,
                    core::ptr::null(),
                    self.surface_version,
                    0,
                    none.as_mut_ptr(),
                );
                if guarded {
                    guard::end();
                }
                (l.display_flush)(self.display);
            }
            if let (Some(t0), Some(picked), Some(copied)) = (traced, picked, copied) {
                use sv_transport::frametrace as ft;
                // The `wl_shm` half of the present (docs/waycompose-design.md
                // §88): a buffer the compositor still held, the copy out of
                // the arena, and the attach-damage-commit.
                ft::emit_at(
                    ft::at_us(t0),
                    "icd.put",
                    format_args!(
                        "surf={:#x} buf={buf_i} waited={} pick_us={} copy_us={} commit_us={}",
                        self.surface as usize,
                        waited as u8,
                        ft::us(picked - t0),
                        ft::us(copied - picked),
                        ft::us(copied.elapsed()),
                    ),
                );
            }
        }

        /// The first buffer the compositor is not holding.
        fn pick(&self) -> Option<usize> {
            (0..self.buffers.len())
                .map(|k| (self.next + k) % self.buffers.len())
                .find(|&i| !self.buffers[i].busy.load(Ordering::Acquire))
        }
    }

    impl Drop for Surface {
        fn drop(&mut self) {
            let Some(l) = lib() else { return };
            // SAFETY: proxies this object created and has not yet destroyed,
            // and its own mapping. Nothing here can fail in a way the
            // application sees: `vkDestroySwapchainKHR` returns void.
            unsafe {
                for b in &self.buffers {
                    let mut none: [Arg; 0] = [];
                    (l.proxy_marshal_array_flags)(
                        b.proxy,
                        WL_BUFFER_DESTROY,
                        core::ptr::null(),
                        (l.proxy_get_version)(b.proxy),
                        MARSHAL_FLAG_DESTROY,
                        none.as_mut_ptr(),
                    );
                }
                let mut none: [Arg; 0] = [];
                (l.proxy_marshal_array_flags)(
                    self.pool,
                    WL_SHM_POOL_DESTROY,
                    core::ptr::null(),
                    (l.proxy_get_version)(self.pool),
                    MARSHAL_FLAG_DESTROY,
                    none.as_mut_ptr(),
                );
                (l.proxy_destroy)(self.shm);
                (l.display_flush)(self.display);
                (l.event_queue_destroy)(self.queue);
                libc::munmap(self.map as *mut c_void, self.map_len);
            }
            if let Some(slot) = self.slot {
                guard::untrack(slot);
            }
        }
    }

    /// `libsv_wlshim.so`'s side of a present that finishes after
    /// `vkQueuePresentKHR` (docs/waycompose-design.md §100), found in the
    /// process with `dlsym(RTLD_DEFAULT)`. Absent, every function says so and
    /// a Wayland swapchain presents inside the call, as §98 requires.
    pub mod guard {
        use std::ffi::c_void;
        use std::sync::OnceLock;

        struct Shim {
            track: unsafe extern "C" fn(*mut c_void) -> i32,
            untrack: unsafe extern "C" fn(i32),
            generation: unsafe extern "C" fn(i32) -> u32,
            begin: unsafe extern "C" fn(i32, u32) -> i32,
            end: unsafe extern "C" fn(),
        }

        // The `sym!` transmutes name their target by the field they fill.
        #[allow(clippy::missing_transmute_annotations)]
        fn shim() -> Option<&'static Shim> {
            static SHIM: OnceLock<Option<Shim>> = OnceLock::new();
            SHIM.get_or_init(|| {
                // SAFETY: `dlsym` with literal NUL-terminated names; each
                // symbol, when present, is the shim's function of exactly
                // this signature (crates/sv-icd/shim/sv_wlshim.c, version 1
                // in the names).
                unsafe {
                    macro_rules! sym {
                        ($n:literal) => {{
                            let p = libc::dlsym(libc::RTLD_DEFAULT, concat!($n, "\0").as_ptr().cast());
                            if p.is_null() {
                                return None;
                            }
                            core::mem::transmute(p)
                        }};
                    }
                    Some(Shim {
                        track: sym!("svwl_shim_v1_track"),
                        untrack: sym!("svwl_shim_v1_untrack"),
                        generation: sym!("svwl_shim_v1_generation"),
                        begin: sym!("svwl_shim_v1_begin"),
                        end: sym!("svwl_shim_v1_end"),
                    })
                }
            })
            .as_ref()
        }

        /// Whether the application runs with the shim.
        pub fn available() -> bool {
            shim().is_some()
        }

        /// Register `surface`. `None` without the shim or a free slot.
        ///
        /// # Safety
        ///
        /// `surface` must be a live `wl_surface` proxy: call it inside the
        /// application's Vulkan call. The shim stores the pointer and only
        /// ever compares it.
        pub unsafe fn track(surface: *mut c_void) -> Option<i32> {
            let s = shim()?;
            // SAFETY: as this function's contract.
            let slot = unsafe { (s.track)(surface) };
            (slot >= 0).then_some(slot)
        }

        pub fn untrack(slot: i32) {
            if let Some(s) = shim() {
                // SAFETY: a slot `track` returned.
                unsafe { (s.untrack)(slot) }
            }
        }

        /// The surface's generation now. Read inside `vkQueuePresentKHR`.
        pub fn generation(slot: i32) -> u32 {
            // SAFETY: a slot `track` returned.
            shim().map_or(0, |s| unsafe { (s.generation)(slot) })
        }

        /// `true` with the shim's lock held: the surface is alive and not
        /// unmapped since generation `generation`. Pair with [`end`].
        pub fn begin(slot: i32, generation: u32) -> bool {
            // SAFETY: a slot `track` returned.
            shim().is_some_and(|s| unsafe { (s.begin)(slot, generation) } == 1)
        }

        pub fn end() {
            if let Some(s) = shim() {
                // SAFETY: only after a `begin` that returned `true`.
                unsafe { (s.end)() }
            }
        }
    }
}
