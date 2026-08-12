//! Where a presented frame goes once the app side holds its pixels.
//!
//! design.md §10 stage 2 ends with "the app-side WSI then blits from the arena
//! to the window with `wl_shm` or `XShm` — a straight memcpy from shared memory
//! into a window buffer, with no GPU involvement and no second copy of our own".
//! This is that step, and it is deliberately the *only* part of presentation
//! that knows what a window is. [`crate::wsi`] produces a rectangle of BGRA
//! bytes in the arena and hands it here.
//!
//! # Two targets, and why there is no third
//!
//! - [`Target::Headless`] displays nothing. That is not a stub: it is what
//!   `VK_EXT_headless_surface` means, it is what Wine maps `VK_KHR_win32_surface`
//!   onto when there is no display (design.md §0.5), and it is what DXVK needs
//!   in order to create a device at all. `SV_PRESENT_DUMP` writes the frames out
//!   so that a harness can judge presentation on pixels rather than on
//!   `vkQueuePresentKHR` returning `VK_SUCCESS`.
//! - [`Target::X11`] puts the pixels in a window with `XPutImage`.
//!
//! Wayland is not here and is not an oversight: it needs a `wl_display` this
//! side would have to speak a protocol to, and a real compositor integration is
//! phase 2 (`docs/phase2`). The rule this module follows instead is the smallest
//! one that is honest — **make something an application can present to and a
//! human can see**, and do not build a compositor.
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
        }
    }

    /// One frame. `pixels` is `width * height * 4` bytes read straight out of
    /// the arena; `swap_rb` says the application chose an `R8G8B8A8` format and
    /// the bytes are therefore RGBA rather than the BGRA both targets want.
    pub fn present(&mut self, pixels: &[u8], width: u32, height: u32, swap_rb: bool) {
        match self {
            Target::Headless(d) => d.write(pixels, width, height, swap_rb),
            #[cfg(unix)]
            Target::X11(w) => w.put(pixels, width, height, swap_rb),
        }
    }

    /// The size the surface currently is, or `None` for "no size of its own".
    pub fn extent(target: PresentTarget) -> Option<(u32, u32)> {
        match target {
            PresentTarget::Headless => None,
            #[cfg(unix)]
            PresentTarget::X11 { window } => x11::geometry(window),
            #[cfg(not(unix))]
            PresentTarget::X11 { .. } => None,
        }
    }
}

/// `SV_PRESENT_DUMP`: write presented frames out as binary PPM.
///
/// This is the headless target's whole reason to exist beyond "accept and
/// discard". A swapchain that returns `VK_SUCCESS` and drops every frame passes
/// every check an application can make; the only way to know the image was
/// rendered, copied and arrived is to look at it, which is the standard
/// `gl-probe` sets (design.md §0.4).
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
        sync: unsafe extern "C" fn(*mut Display, c_int) -> c_int,
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
    unsafe extern "C" fn ignore_error(_dpy: *mut Display, _event: *mut c_void) -> c_int {
        0
    }

    /// Run `f` with X errors swallowed, and make sure ours have arrived before
    /// the application's handler goes back.
    ///
    /// `XSetErrorHandler` is process-global rather than per-`Display`, which is
    /// the one genuinely awkward thing about talking X from inside a driver. The
    /// `XSync` is what makes the window narrow enough to accept: an X error is
    /// delivered while its *own* connection is being read, so syncing our
    /// connection before restoring the previous handler means every error we
    /// caused has already been swallowed by ours. An error on the application's
    /// connection can only be delivered while the application is inside Xlib,
    /// which for a single-threaded GL program is never during this call.
    fn quietly<T>(l: &Lib, dpy: *mut Display, f: impl FnOnce() -> T) -> T {
        // SAFETY: `XSetErrorHandler` takes and returns a plain function pointer,
        // and `XSync` on a live display.
        unsafe {
            let prev = (l.set_error_handler)(Some(ignore_error));
            let out = f();
            (l.sync)(dpy, 0);
            (l.set_error_handler)(prev);
            out
        }
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
                    sync: sym!("XSync"),
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

    fn connection() -> Option<&'static Mutex<Connection>> {
        static CONN: OnceLock<Option<Mutex<Connection>>> = OnceLock::new();
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
            quietly(l, dpy, || unsafe {
                let pixels = libc::malloc(len.max(1)) as *mut u8;
                if pixels.is_null() {
                    return None;
                }
                let visual = (l.default_visual)(dpy, g.screen);
                let depth = (l.default_depth)(dpy, g.screen) as c_uint;
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
            // `quietly` ends in an `XSync`, which is a round trip to the X
            // server per frame. That is the price of not being able to let an
            // error escape into the application's handler, and it is paid where
            // the frame has already cost a GPU copy and 8 MB of memory traffic.
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

}
