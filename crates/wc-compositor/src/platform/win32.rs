//! One `HWND` per `xdg_toplevel`, with the system's own decorations, showing
//! the client's pixels.
//!
//! This is the stage-3 milestone. Everything above it is platform-neutral;
//! everything below is Win32.
//!
//! # Reentrancy, which is the thing that makes this file hard
//!
//! Almost every interesting Win32 call **sends a message to our own window
//! procedure before it returns**. `SetWindowPos` sends `WM_SIZE` and
//! `WM_WINDOWPOSCHANGED`; `ShowWindow` sends `WM_SHOWWINDOW` and usually
//! `WM_SIZE`; `SetWindowTextW` sends `WM_SETTEXT`; `DestroyWindow` sends
//! `WM_DESTROY`. So the window procedure runs *inside* the command handler,
//! on the same thread, with the same data reachable.
//!
//! In Rust that is an aliasing problem with teeth: a `RefCell<HashMap<..>>`
//! borrowed across a `SetWindowPos` is a panic, and a `&mut` handed to the
//! window procedure through a raw pointer is undefined behaviour. Both are
//! *intermittent* — they need the reentrant path to be taken — which is the
//! worst possible failure mode.
//!
//! The answer here is structural rather than careful: **no `&mut` to per-window
//! state exists anywhere in this file.** Each window's state is an
//! `Rc<WindowState>` whose mutable fields are `Cell`s. The window procedure
//! takes a shared reference and so does the command handler, so they can be
//! active at the same time by construction and no discipline is required of
//! whoever edits this next. The `RefCell` around the id->window map is borrowed
//! only to clone an `Rc` out, never across a Win32 call.
//!
//! # Why `SetDIBitsToDevice` and not a DIB section
//!
//! `CreateDIBSection` plus `BitBlt` is faster — the blit becomes a driver-side
//! `memcpy` and the source rectangle is unambiguous. It is not used, for one
//! reason that is about correctness rather than taste: `SetDIBitsToDevice`'s
//! `ySrc`/`uStartScan` arguments are documented in terms of a **bottom-up**
//! DIB, and their meaning for the top-down DIB we want (negative `biHeight`,
//! matching Wayland's top-left origin) is genuinely ambiguous in the
//! documentation and differs between descriptions of it.
//!
//! So this passes the *whole image* with `xSrc = ySrc = uStartScan = 0` and
//! `cLines = height`, which has no ambiguity at all, and lets the clip region
//! that `BeginPaint` installs from the update region decide which pixels
//! actually reach the screen. Damage therefore limits two of the three things
//! it should: how much is copied out of the client's buffer, and how much is
//! written to the framebuffer. It does not limit how much of the DIB is handed
//! to GDI. That is the known cost, it is a constant factor on an already-CPU
//! path, and the fix is the DIB section — which is worth doing when there is a
//! measurement saying it matters, and not before.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::ffi::c_void;
use std::rc::Rc;
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};

use windows_sys::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows_sys::Win32::Graphics::Gdi::{
    BeginPaint, CreateSolidBrush, DeleteObject, EndPaint, ExcludeClipRect, FillRect,
    InvalidateRect, SetDIBitsToDevice, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS,
    HBRUSH, HDC, PAINTSTRUCT,
    // ADDED FOR STAGE 9. `ClientToScreen` is filed under GDI rather than under
    // WindowsAndMessaging, which is where one would look for it.
    ClientToScreen,
};
use windows_sys::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress, LoadLibraryW};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    GetKeyboardLayout, ReleaseCapture, SetCapture, TrackMouseEvent, TME_LEAVE, TRACKMOUSEEVENT,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    AdjustWindowRectEx, CreateIconIndirect, CreateWindowExW, DefWindowProcW, DestroyIcon,
    DestroyWindow, DispatchMessageW, GetClientRect, GetMessageTime, GetMessageW, GetSystemMetrics,
    GetWindowLongPtrW, LoadCursorW, PostMessageW, PostQuitMessage,
    RegisterClassExW, SendMessageW, SetWindowLongPtrW, SetWindowPos, SetWindowTextW, ShowWindow,
    TranslateMessage, CW_USEDEFAULT, GWLP_USERDATA, HWND_MESSAGE, ICONINFO, ICON_BIG, ICON_SMALL,
    IDC_ARROW, MSG, SM_CXICON, SM_CXSMICON, SM_CYICON, SM_CYSMICON, SIZE_MAXIMIZED,
    SIZE_MINIMIZED, SIZE_RESTORED, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOZORDER, SW_MAXIMIZE,
    SW_MINIMIZE, SW_RESTORE, SW_SHOWNORMAL, WA_INACTIVE, WHEEL_DELTA, WM_ACTIVATE, WM_APP,
    WM_CLOSE, WM_DESTROY, WM_ERASEBKGND, WM_INPUTLANGCHANGE, WM_KEYDOWN, WM_KEYUP,
    WM_KILLFOCUS, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MBUTTONDOWN, WM_MBUTTONUP, WM_MOUSEHWHEEL,
    WM_MOUSEMOVE, WM_MOUSEWHEEL, WM_PAINT, WM_RBUTTONDOWN, WM_RBUTTONUP,
    WM_SETFOCUS, WM_SETICON, WM_SIZE, WM_SYSKEYDOWN, WM_SYSKEYUP, WM_XBUTTONDOWN, WM_XBUTTONUP,
    WNDCLASSEXW, WS_CLIPCHILDREN, WS_CLIPSIBLINGS, WS_OVERLAPPEDWINDOW, XBUTTON1,
    // ADDED FOR STAGE 9, for popups.
    GetWindow, GW_OWNER, HWND_TOPMOST, SW_SHOWNOACTIVATE, WS_EX_NOACTIVATE,
    WS_EX_TOOLWINDOW, WS_POPUP,
};
use windows_sys::Win32::Foundation::POINT;

use crate::input::{evdev, evdev_keycode};

use super::{
    assemble, Commands, FrameStyle, FromUi, IconImage, PlatformConfig, Started, ToUi, WindowId,
    WindowKind,
};
use crate::surface::{Rect, Shadow};

/// "Drain the command queue." The only message this module posts.
const WM_WC_COMMAND: u32 = WM_APP + 1;

/// `WM_MOUSELEAVE`, 0x02A3.
///
/// Written out rather than imported, because `windows-sys` files it under
/// `Win32::UI::Controls` — with the common controls, not with the other mouse
/// messages — and pulling that whole feature in for one integer would add a
/// large module to every build to say `675`. `winuser.h` declares it beside
/// `WM_MOUSEHOVER` and `TrackMouseEvent`; the metadata's placement is a fact
/// about the Windows SDK's headers rather than about the message.
const WM_MOUSELEAVE: u32 = 0x02A3;

/// The colour behind a window whose buffer does not fill it.
///
/// This is what the user sees during a resize, between Windows growing the
/// window and the client committing a buffer at the new size, and it is
/// therefore a deliberate choice rather than a default — see
/// [`WindowState::paint`].
const BACKGROUND: COLORREF = 0x0020_2020;

// ---------------------------------------------------------------------------
// Waking the UI thread
// ---------------------------------------------------------------------------

/// A `PostMessage` target that lives for the life of the process.
///
/// **A message-only window, not a thread id.** `PostThreadMessage` would be the
/// obvious way to poke a message loop and it is the wrong one: a thread message
/// has no `hwnd`, so `DispatchMessage` has nowhere to send it, and the modal
/// loops that `DefWindowProc` runs while the user drags or resizes a window
/// discard it. The symptom is a compositor that stops repainting for exactly as
/// long as the mouse button is held on the title bar, which reads as a
/// rendering bug and is not one. Posting to a window works because those same
/// modal loops dispatch window messages for every window on the thread.
pub(crate) struct Wake {
    hwnd: isize,
}

// SAFETY: `PostMessageW` is documented as callable from any thread, and an
// `HWND` is a process-wide value rather than a thread-local one. Thread
// *affinity* governs who may pump and who may destroy the window, not who may
// post to it. Nothing else in this struct is touched off-thread.
unsafe impl Send for Wake {}
unsafe impl Sync for Wake {}

impl Wake {
    pub(crate) fn wake(&self) {
        // SAFETY: `hwnd` is the controller window created in `start`, which
        // outlives every `Ui` handle because it is destroyed only after the
        // pump has returned.
        unsafe {
            PostMessageW(self.hwnd as HWND, WM_WC_COMMAND, 0, 0);
        }
    }
}

// ---------------------------------------------------------------------------
// Per-window state
// ---------------------------------------------------------------------------

/// Everything one window knows.
///
/// Every mutable field is a `Cell`, for the reason in this module's header:
/// the window procedure and the command handler are both live at once whenever
/// a Win32 call reenters, and shared references plus interior mutability is the
/// only shape that makes that sound without relying on anyone being careful.
struct WindowState {
    id: WindowId,
    /// ADDED FOR STAGE 9. Read at map time, to decide whether the window is
    /// placed by Windows or by an `xdg_positioner`, and whether showing it may
    /// take the foreground.
    kind: WindowKind,
    hwnd: Cell<isize>,
    shadow: Arc<Mutex<Shadow>>,
    events: Sender<FromUi>,
    frame: FrameStyle,

    /// The last surface size we sent as a configure. Windows emits `WM_SIZE`
    /// for every pixel of a drag; sending a configure per pixel is legal but
    /// pointless, and this is what makes the negotiation quiet when nothing
    /// changed.
    last_configured: Cell<(i32, i32)>,
    maximized: Cell<bool>,
    minimized: Cell<bool>,
    mapped: Cell<bool>,
    /// Set while we are the ones calling `ShowWindow`, so the `WM_SIZE` it
    /// provokes is not reported back to the client as a user action. Without
    /// it, a client asking to be maximised receives its own request as an
    /// event and toolkits that resend on change oscillate.
    self_driven: Cell<bool>,
    icon_big: Cell<isize>,
    /// How many frames this window has presented, for the dump filenames.
    frames: Cell<u32>,
    icon_small: Cell<isize>,

    // --- input, stage 6 ---------------------------------------------------
    /// Whether `TrackMouseEvent` is currently armed for this window.
    ///
    /// **`WM_MOUSELEAVE` does not exist unless it is.** Win32 sends
    /// `WM_MOUSEMOVE` for free and sends nothing at all when the pointer
    /// leaves, unless the window has asked — once, per departure, because the
    /// request is consumed when it fires. The omission is silent and produces a
    /// compositor whose clients believe the pointer is still inside forever:
    /// a hover highlight that never turns off, a terminal that keeps its I-beam
    /// cursor, and a `wl_pointer` that never sends `leave`.
    tracking: Cell<bool>,
    /// How many buttons are down, so the capture is released exactly once.
    buttons_down: Cell<u32>,
    /// The last surface position reported, so a `WM_MOUSEWHEEL` — whose
    /// coordinates are *screen* coordinates, unlike every other mouse message
    /// — does not have to convert them.
    last_pos: Cell<(f64, f64)>,
}

impl WindowState {
    fn hwnd(&self) -> HWND {
        self.hwnd.get() as HWND
    }

    /// Paint the window: the client's pixels where they are, the background
    /// colour everywhere else.
    ///
    /// # What is shown while a resize is being negotiated
    ///
    /// This is the question the stage has to answer, because Windows and
    /// Wayland disagree about what a resize *is*. Windows resizes the window
    /// and wants it painted now; Wayland requires the compositor to send a
    /// configure and then wait, possibly for several frames, for the client to
    /// commit a buffer at the new size. Something has to be on screen in
    /// between.
    ///
    /// The options are to stretch the old buffer to the new size, or to draw it
    /// at its true size and fill the rest. **This draws it at its true size,
    /// anchored at the content origin, and fills the remainder with a flat
    /// colour**, for three reasons:
    ///
    /// * Stretching is a lie about the content. A `StretchBlt` of text is
    ///   unreadable, and the CPU cost of `HALFTONE` mode on a window-sized
    ///   bitmap every mouse-move is real.
    /// * Growing a window shows a band of background that fills in a frame
    ///   later, which is what X11 compositors have always done and what users
    ///   already read as "the application is catching up". Shrinking simply
    ///   clips, which is free and correct.
    /// * It makes the negotiation *visible*. If a client stops acking
    ///   configures, the band never fills in, and that is a bug someone can
    ///   see rather than a smooth stretch that hides a stuck client.
    fn paint(&self) {
        let hwnd = self.hwnd();
        let mut ps: PAINTSTRUCT = unsafe { std::mem::zeroed() };
        // SAFETY: a live window handle; `ps` is written by the call.
        let hdc = unsafe { BeginPaint(hwnd, &mut ps) };
        if !hdc.is_null() {
            self.blit(hdc);
        }
        // SAFETY: paired with the `BeginPaint` above, same `ps`.
        unsafe {
            EndPaint(hwnd, &ps);
        }
    }

    fn blit(&self, hdc: HDC) {
        let shadow = match self.shadow.lock() {
            Ok(s) => s,
            // A poisoned lock means the Wayland thread panicked mid-blit. The
            // pixels may be a torn frame, but they are readable and showing a
            // torn frame beats leaving the window unpainted while the process
            // winds down.
            Err(poisoned) => poisoned.into_inner(),
        };
        if shadow.is_empty() {
            return;
        }
        let (w, h) = (shadow.width(), shadow.height());
        let (ox, oy) = self.frame.content_origin();

        let mut bmi: BITMAPINFO = unsafe { std::mem::zeroed() };
        bmi.bmiHeader = BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: w,
            // Negative height means top-down, which is Wayland's origin. A
            // positive value here is the classic upside-down window.
            biHeight: -h,
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB,
            biSizeImage: (w as u32) * (h as u32) * 4,
            biXPelsPerMeter: 0,
            biYPelsPerMeter: 0,
            biClrUsed: 0,
            biClrImportant: 0,
        };

        // SAFETY: `bmi` describes exactly `w * h * 4` bytes and the shadow is
        // packed to that size by `Shadow::blit`; the pointer is valid for the
        // duration of the call because the lock is still held. The whole image
        // is passed with a zero source origin, which is the unambiguous form —
        // see this module's header.
        unsafe {
            SetDIBitsToDevice(
                hdc,
                ox,
                oy,
                w as u32,
                h as u32,
                0,
                0,
                0,
                h as u32,
                shadow.pixels().as_ptr().cast::<c_void>(),
                &bmi,
                DIB_RGB_COLORS,
            );
        }
    }

    /// Fill the part of the client area the client's buffer does not cover.
    ///
    /// Handled rather than left to `DefWindowProc` and a class background
    /// brush, because the default erases the *whole* client area and then
    /// `WM_PAINT` draws over it — a full-window flash of background on every
    /// frame. Excluding the content rectangle first means the only pixels
    /// written twice are the ones that need it.
    fn erase(&self, hdc: HDC) {
        let (w, h) = {
            let shadow = match self.shadow.lock() {
                Ok(s) => s,
                Err(p) => p.into_inner(),
            };
            (shadow.width(), shadow.height())
        };
        let (ox, oy) = self.frame.content_origin();

        let mut rc: RECT = unsafe { std::mem::zeroed() };
        // SAFETY: live window, `rc` written by the call.
        unsafe {
            GetClientRect(self.hwnd(), &mut rc);
        }
        // SAFETY: a fresh solid brush, deleted below; `ExcludeClipRect` and
        // `FillRect` take the caller's DC, which `WM_ERASEBKGND` supplied.
        unsafe {
            let brush = CreateSolidBrush(BACKGROUND);
            if w > 0 && h > 0 {
                ExcludeClipRect(hdc, ox, oy, ox + w, oy + h);
            }
            FillRect(hdc, &rc, brush as HBRUSH);
            DeleteObject(brush as _);
        }
    }

    /// Turn a `WM_SIZE` into a configure, if anything actually changed.
    fn on_size(&self, kind: WPARAM, client_w: i32, client_h: i32) {
        let minimized = kind == SIZE_MINIMIZED as WPARAM;
        if minimized != self.minimized.get() {
            self.minimized.set(minimized);
            let _ = self.events.send(FromUi::Minimized {
                id: self.id,
                on: minimized,
            });
        }
        if minimized {
            // A minimised window reports a 0x0 client area. Forwarding that as
            // a configure would tell the client "pick your own size", and it
            // would pick one and repaint for a window nobody can see.
            return;
        }

        let maximized = kind == SIZE_MAXIMIZED as WPARAM;
        if (kind == SIZE_MAXIMIZED as WPARAM || kind == SIZE_RESTORED as WPARAM)
            && maximized != self.maximized.get()
        {
            self.maximized.set(maximized);
            if !self.self_driven.get() {
                let _ = self.events.send(FromUi::Maximized {
                    id: self.id,
                    on: maximized,
                });
            }
        }

        let (w, h) = self.frame.surface_size_for(client_w, client_h);
        if w <= 0 || h <= 0 || (w, h) == self.last_configured.get() {
            return;
        }
        self.last_configured.set((w, h));
        let _ = self.events.send(FromUi::Resized {
            id: self.id,
            width: w,
            height: h,
        });
    }

    // -----------------------------------------------------------------------
    // Input
    // -----------------------------------------------------------------------

    fn send(&self, e: FromUi) {
        let _ = self.events.send(e);
    }

    /// A client-area point, in the surface's own coordinates.
    ///
    /// The subtraction is [`FrameStyle::content_origin`] and it is not
    /// decoration: once `DwmExtendFrameIntoClientArea` moves the content origin
    /// up into the caption, a click at client `y = 0` is *above* the surface and
    /// the client must be told a negative number or told nothing. Routing every
    /// coordinate through the same conversion the blit uses is what keeps the
    /// pointer and the pixels agreeing about where the surface is.
    fn to_surface(&self, x: i32, y: i32) -> (f64, f64) {
        let (ox, oy) = self.frame.content_origin();
        (f64::from(x - ox), f64::from(y - oy))
    }

    /// `WM_MOUSEMOVE`. Also where the pointer *enters*, because Win32 has no
    /// enter message.
    fn on_mouse_move(&self, lparam: LPARAM, time: u32) {
        let (x, y) = (lo_i16(lparam), hi_i16(lparam));
        let (sx, sy) = self.to_surface(x, y);
        self.last_pos.set((sx, sy));

        if !self.tracking.replace(true) {
            // Arm the leave notification, and only now: `TrackMouseEvent` is a
            // one-shot and re-arming it on every mouse-move would be a syscall
            // per pixel of movement. The first move after an entry is the
            // moment there is something to leave.
            let mut tme = TRACKMOUSEEVENT {
                cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32,
                dwFlags: TME_LEAVE,
                hwndTrack: self.hwnd(),
                dwHoverTime: 0,
            };
            // SAFETY: a fully initialised struct whose `cbSize` is truthful,
            // and a live window handle.
            unsafe {
                TrackMouseEvent(&mut tme);
            }
            self.send(FromUi::PointerEnter { id: self.id, x: sx, y: sy, time });
            return;
        }
        self.send(FromUi::PointerMotion { id: self.id, x: sx, y: sy, time });
    }

    fn on_mouse_leave(&self, time: u32) {
        self.tracking.set(false);
        self.send(FromUi::PointerLeave { id: self.id, time });
    }

    /// A button went down or came up.
    ///
    /// **`SetCapture` on the first press and `ReleaseCapture` on the last
    /// release.** Without it a drag that leaves the window loses its release:
    /// Win32 delivers mouse messages to the window under the pointer, so the
    /// `WM_LBUTTONUP` goes to whatever the user dragged onto — or to nothing —
    /// and the client is left with a button it believes is still held. Every
    /// selection in a terminal and every scrollbar drag depends on this.
    ///
    /// Counting rather than a boolean because two buttons can be down at once
    /// and releasing the capture on the first release would reintroduce the bug
    /// for the second.
    fn on_button(&self, button: u32, pressed: bool, lparam: LPARAM, time: u32) {
        let (x, y) = (lo_i16(lparam), hi_i16(lparam));
        let (sx, sy) = self.to_surface(x, y);
        self.last_pos.set((sx, sy));
        // The position first: `wl_pointer.button` carries no coordinates, so
        // the client places the click wherever the last motion was, and a click
        // on a control the pointer reached in the same event would otherwise
        // land at the previous position.
        self.send(FromUi::PointerMotion { id: self.id, x: sx, y: sy, time });

        if pressed {
            if self.buttons_down.replace(self.buttons_down.get() + 1) == 0 {
                // SAFETY: a live window handle owned by this thread.
                unsafe {
                    SetCapture(self.hwnd());
                }
            }
        } else {
            let n = self.buttons_down.get().saturating_sub(1);
            self.buttons_down.set(n);
            if n == 0 {
                // SAFETY: paired with the `SetCapture` above.
                unsafe {
                    ReleaseCapture();
                }
            }
        }
        self.send(FromUi::PointerButton { id: self.id, button, pressed, time });
    }

    /// `WM_MOUSEWHEEL` / `WM_MOUSEHWHEEL`.
    ///
    /// # Two conversions, both of which have a sign to get right
    ///
    /// * **Vertical.** Windows counts a wheel notch as `+WHEEL_DELTA` when it
    ///   is rotated *away* from the user, and Wayland's vertical axis is
    ///   positive *downwards* — which is the direction the content moves, not
    ///   the wheel. So the sign is inverted. Getting this wrong is not subtle
    ///   and is not caught by any test that only checks that scrolling happens.
    /// * **Horizontal.** Both are positive to the right, so it is not inverted.
    ///   The asymmetry is real and is the reason these are not one line.
    ///
    /// `WHEEL_DELTA` is 120 and `wl_pointer.axis_value120` counts in
    /// hundred-and-twentieths of a notch, so the raw delta *is* the v120 value
    /// with the sign fixed. That is not a coincidence: the protocol added v120
    /// because Windows and every high-resolution wheel already spoke it.
    ///
    /// The `axis` value in surface units is ten per notch, which is Weston's
    /// `DEFAULT_AXIS_STEP_DISTANCE` and what a client that ignores v120 will
    /// scroll by.
    fn on_wheel(&self, wparam: WPARAM, horizontal: bool, time: u32) {
        let delta = ((wparam >> 16) & 0xffff) as u16 as i16;
        let v120 = if horizontal { i32::from(delta) } else { -i32::from(delta) };
        let value = f64::from(v120) * 10.0 / f64::from(WHEEL_DELTA);
        let (h, v) = if horizontal { (value, 0.0) } else { (0.0, value) };
        let (v120_h, v120_v) = if horizontal { (v120, 0) } else { (0, v120) };
        self.send(FromUi::PointerAxis {
            id: self.id,
            horizontal: h,
            vertical: v,
            v120_h,
            v120_v,
            time,
        });
    }

    /// A key, from its scan code.
    ///
    /// Returns whether it was consumed, which matters only for `WM_SYSKEYDOWN`:
    /// letting `DefWindowProc` see an Alt press opens the (nonexistent) window
    /// menu and swallows the following key, so the Alt combinations a client
    /// expects would never arrive.
    fn on_key(&self, wparam: WPARAM, lparam: LPARAM, pressed: bool, time: u32) {
        // bit 30 of lParam is "the key was already down". Windows repeats a
        // held key by resending `WM_KEYDOWN` with it set; Wayland's model is
        // that the *client* repeats, from the rate in `repeat_info`. Forwarding
        // both would give a client two repeat streams at different rates, which
        // presents as a stuttering, doubled repeat — so the platform's is
        // dropped here, at the only place it can be told apart.
        if pressed && (lparam & (1 << 30)) != 0 {
            return;
        }
        let scan = ((lparam >> 16) & 0xff) as u8;
        let extended = (lparam & (1 << 24)) != 0;
        let vk = (wparam & 0xffff) as u16;
        let Some(keycode) = evdev_keycode(scan, extended, vk) else {
            // Said out loud rather than dropped quietly: a key that produces
            // nothing is the one thing a user cannot debug from the outside,
            // and the scan code is what somebody would need to add it to the
            // table in `crate::input`.
            println!(
                "wc-compositor: no evdev keycode for scan {scan:#04x}{} (vk {vk:#04x}); key dropped",
                if extended { " e0" } else { "" }
            );
            return;
        };
        self.send(FromUi::Key { id: self.id, keycode, pressed, time });
    }
}

/// The low and high halves of an `LPARAM`, **as signed 16-bit**.
///
/// Not a detail: a mouse position on a multi-monitor desktop or during a drag
/// off the left edge is genuinely negative, and `lparam & 0xffff` reads -1 as
/// 65535. The window then receives a pointer at x = 65535, which no clamp
/// catches because it is a plausible number.
fn lo_i16(lparam: LPARAM) -> i32 {
    i32::from((lparam & 0xffff) as u16 as i16)
}

fn hi_i16(lparam: LPARAM) -> i32 {
    i32::from(((lparam >> 16) & 0xffff) as u16 as i16)
}

/// Which of `crate::input::LAYOUTS` this thread's keyboard layout asks for.
///
/// `GetKeyboardLayout(0)` is the *calling thread's* layout, which is what a
/// compositor wants: Windows keeps one per thread and switching it is per
/// window in practice, so the thread that owns the windows is the thread whose
/// answer matters.
pub fn current_layout() -> &'static str {
    // SAFETY: a documented query with no pointer arguments. 0 means "this
    // thread".
    let hkl = unsafe { GetKeyboardLayout(0) } as usize;
    let langid = (hkl & 0xffff) as u16;
    let name = crate::input::layout_for_langid(langid);
    println!("wc-compositor: keyboard layout {langid:#06x} -> {name}");
    name
}

// ---------------------------------------------------------------------------
// The thread-local registry
// ---------------------------------------------------------------------------

struct Ui {
    windows: RefCell<HashMap<WindowId, Rc<WindowState>>>,
    commands: Commands,
    events: Sender<FromUi>,
    frame: FrameStyle,
    /// Guards against `WM_WC_COMMAND` arriving while an earlier drain is still
    /// running. It should not be possible — the calls a drain makes send
    /// messages rather than pumping them — but "should not be possible" is not
    /// a thing to find out about from a corrupted window list.
    draining: Cell<bool>,
    controller: Cell<isize>,
    /// Where to write a PPM of every presented frame, if asked.
    ///
    /// ADDED FOR STAGE 4. Until then this was headless-only, and that asymmetry
    /// is exactly what docs/design.md §8.5 had to apologise for: the half of
    /// the evidence that produced an `HWND` was also the half whose pixels
    /// nobody outside the process ever measured. A frame dumped here is read
    /// back by `identify` and `compare`, which are not ours.
    dump_frames: Option<std::path::PathBuf>,
}

thread_local! {
    static UI: RefCell<Option<Rc<Ui>>> = const { RefCell::new(None) };
}

fn ui() -> Option<Rc<Ui>> {
    UI.with(|u| u.borrow().clone())
}

// ---------------------------------------------------------------------------
// Window procedures
// ---------------------------------------------------------------------------

/// When the message being dispatched was posted, in milliseconds.
///
/// **`GetMessageTime`, not a clock read.** Wayland's input events carry a
/// timestamp and clients use the differences between them — for double-click
/// detection, for key repeat, for gesture timing. Reading a clock inside the
/// window procedure would timestamp events with *when the compositor got round
/// to them*, which on a busy poll loop is not when they happened; the queue
/// already carries the real time and this is how to read it.
///
/// The epoch is the system tick count, which is a monotonic millisecond counter
/// that wraps every 49.7 days. Wayland says only that the base is undefined and
/// that the values are comparable, which this satisfies — the same statement
/// `now_ms` makes in `state.rs`, from a different clock, and the two are not
/// required to agree.
fn message_time() -> u32 {
    // SAFETY: a pure query with no arguments.
    unsafe { GetMessageTime() as u32 }
}

/// UTF-16, NUL-terminated. Every `*W` entry point wants one.
fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

const CLASS_TOPLEVEL: &str = "WaycomposeToplevel";
const CLASS_CONTROLLER: &str = "WaycomposeController";

unsafe extern "system" fn toplevel_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    // SAFETY: `GWLP_USERDATA` holds either 0 or a pointer produced by
    // `Rc::into_raw` in `create_window`, and it is cleared in `WM_DESTROY`
    // before the `Rc` is dropped. Messages arriving before `CreateWindowExW`
    // returns — `WM_CREATE`, and the `WM_SIZE` that follows it — see 0 and fall
    // through to `DefWindowProc`, which is correct: there is nothing to report
    // about a window the compositor has not been told about yet.
    let ptr = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *const WindowState;
    if ptr.is_null() {
        return unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) };
    }
    // SAFETY: shared, never `&mut`. See this module's header — the same state
    // is reachable from the command handler further up this very stack whenever
    // a Win32 call reenters, and a shared reference is the only sound shape.
    let state: &WindowState = unsafe { &*ptr };

    match msg {
        WM_PAINT => {
            state.paint();
            0
        }
        WM_ERASEBKGND => {
            state.erase(wparam as HDC);
            // Non-zero: "the background is erased, do not do it again".
            1
        }
        WM_SIZE => {
            let w = (lparam & 0xffff) as i32;
            let h = ((lparam >> 16) & 0xffff) as i32;
            state.on_size(wparam, w, h);
            0
        }
        WM_ACTIVATE => {
            let on = (wparam & 0xffff) as u32 != WA_INACTIVE;
            let _ = state.events.send(FromUi::Activated { id: state.id, on });
            0
        }

        // --- input, stage 6 ------------------------------------------------
        //
        // **Focus is `WM_SETFOCUS`, not `WM_ACTIVATE`.** They arrive together
        // for a top-level window and they are different facts: activation says
        // which window is frontmost and becomes `xdg_toplevel`'s `activated`
        // state, focus says where a keystroke goes and becomes
        // `wl_keyboard.enter`. The brief's item 4 is this message pair, because
        // the user's window manager is Windows and a compositor that decided
        // its own keyboard focus would be fighting it.
        WM_SETFOCUS => {
            state.send(FromUi::KeyboardFocus { id: state.id, on: true });
            0
        }
        WM_KILLFOCUS => {
            state.send(FromUi::KeyboardFocus { id: state.id, on: false });
            0
        }

        // `WM_SYSKEYDOWN` as well as `WM_KEYDOWN`, and this is the classic
        // omission on the keyboard side. Windows routes Alt, F10 and every
        // Alt-combination through the `SYS` pair; a compositor that handles
        // only `WM_KEYDOWN` delivers no Alt at all, and therefore no
        // Alt-shortcut in any client, while every other key works perfectly.
        //
        // The `SYS` cases return 0 rather than falling through to
        // `DefWindowProc`, which would open the window menu on Alt and swallow
        // the next key.
        WM_KEYDOWN | WM_SYSKEYDOWN => {
            state.on_key(wparam, lparam, true, message_time());
            0
        }
        WM_KEYUP | WM_SYSKEYUP => {
            state.on_key(wparam, lparam, false, message_time());
            0
        }
        WM_INPUTLANGCHANGE => {
            // `lParam` is the new `HKL`. Handled rather than ignored because a
            // user who switches layout under a running client and keeps getting
            // the old one has no way to tell what went wrong.
            let langid = (lparam as usize & 0xffff) as u16;
            let name = crate::input::layout_for_langid(langid);
            println!("wc-compositor: WM_INPUTLANGCHANGE {langid:#06x} -> {name}");
            state.send(FromUi::Layout { name });
            0
        }

        WM_MOUSEMOVE => {
            state.on_mouse_move(lparam, message_time());
            0
        }
        WM_MOUSELEAVE => {
            state.on_mouse_leave(message_time());
            0
        }
        WM_LBUTTONDOWN => {
            state.on_button(evdev::BTN_LEFT, true, lparam, message_time());
            0
        }
        WM_LBUTTONUP => {
            state.on_button(evdev::BTN_LEFT, false, lparam, message_time());
            0
        }
        WM_RBUTTONDOWN => {
            state.on_button(evdev::BTN_RIGHT, true, lparam, message_time());
            0
        }
        WM_RBUTTONUP => {
            state.on_button(evdev::BTN_RIGHT, false, lparam, message_time());
            0
        }
        WM_MBUTTONDOWN => {
            state.on_button(evdev::BTN_MIDDLE, true, lparam, message_time());
            0
        }
        WM_MBUTTONUP => {
            state.on_button(evdev::BTN_MIDDLE, false, lparam, message_time());
            0
        }
        WM_XBUTTONDOWN | WM_XBUTTONUP => {
            // Which side button is in the *high* word of `wParam`, where every
            // other button message keeps the modifier keys.
            let which = ((wparam >> 16) & 0xffff) as u16;
            let button = if which == XBUTTON1 { evdev::BTN_SIDE } else { evdev::BTN_EXTRA };
            state.on_button(button, msg == WM_XBUTTONDOWN, lparam, message_time());
            // Documented as requiring TRUE rather than 0, unlike every other
            // button message.
            1
        }
        WM_MOUSEWHEEL => {
            state.on_wheel(wparam, false, message_time());
            0
        }
        WM_MOUSEHWHEEL => {
            state.on_wheel(wparam, true, message_time());
            0
        }
        WM_CLOSE => {
            // Deliberately **not** `DefWindowProc`, which would destroy the
            // window. In Wayland the close button is a request: the compositor
            // sends `xdg_toplevel.close` and the client decides. A client that
            // ignores it keeps its window, which is the same behaviour every
            // Wayland compositor has and is worth knowing before it is filed as
            // a bug.
            let _ = state
                .events
                .send(FromUi::CloseRequested { id: state.id });
            0
        }
        WM_DESTROY => {
            // SAFETY: clearing the slot before reclaiming the `Rc` means any
            // message still to be delivered to this window sees a null pointer
            // and takes the `DefWindowProc` path above.
            unsafe {
                SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
                drop(Rc::from_raw(ptr));
            }
            0
        }
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}

unsafe extern "system" fn controller_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if msg == WM_WC_COMMAND {
        if let Some(ui) = ui() {
            drain(&ui);
        }
        return 0;
    }
    unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
}

// ---------------------------------------------------------------------------
// Command handling
// ---------------------------------------------------------------------------

fn drain(ui: &Rc<Ui>) {
    if ui.draining.replace(true) {
        // Reentered. Put the wakeup back so nothing is lost and unwind.
        // SAFETY: the controller window outlives every drain.
        unsafe {
            PostMessageW(ui.controller.get() as HWND, WM_WC_COMMAND, 0, 0);
        }
        return;
    }
    while let Ok(cmd) = ui.commands.try_recv() {
        if matches!(cmd, ToUi::Quit) {
            // SAFETY: ends this thread's `GetMessage` loop.
            unsafe { PostQuitMessage(0) };
            break;
        }
        handle(ui, cmd);
    }
    ui.draining.set(false);
}

/// Look a window up without holding the map borrow across anything that can
/// reenter. Every Win32 call below happens after the `Rc` is out and the
/// `RefCell` guard is dropped.
fn window(ui: &Rc<Ui>, id: WindowId) -> Option<Rc<WindowState>> {
    ui.windows.borrow().get(&id).cloned()
}

fn handle(ui: &Rc<Ui>, cmd: ToUi) {
    match cmd {
        ToUi::Create {
            id,
            title,
            app_id,
            shadow,
            kind,
        } => {
            if let Err(e) = create_window(ui, id, &title, &app_id, shadow, kind) {
                eprintln!("wc-compositor: could not create a window for surface {id}: {e}");
            }
        }
        ToUi::Title { id, title } => {
            if let Some(w) = window(ui, id) {
                let t = wide(&title);
                // SAFETY: live window, live NUL-terminated UTF-16 buffer. This
                // sends `WM_SETTEXT` to `toplevel_proc` before returning, which
                // is exactly the reentrancy the `Cell`s are for.
                unsafe {
                    SetWindowTextW(w.hwnd(), t.as_ptr());
                }
                println!("wc-compositor: window {id} title -> {title:?}");
            }
        }
        ToUi::AppId { id, app_id } => {
            // Nothing to do on Win32 yet. `set_app_id` names a desktop entry,
            // and turning that into a taskbar icon means resolving it on the
            // machine the client is on and shipping pixels back
            // (docs/goal-firefox.md). Logged so the harness can see it arrive.
            println!("wc-compositor: window {id} app_id -> {app_id:?}");
        }
        ToUi::Icon { id, icon } => {
            if let Some(w) = window(ui, id) {
                set_icon(&w, icon.as_ref());
            }
        }
        ToUi::Map { id, width, height } => {
            if let Some(w) = window(ui, id) {
                map_window(&w, width, height);
            }
        }
        ToUi::Present { id, damage } => {
            if let Some(w) = window(ui, id) {
                present(&w, &damage);
                dump_frame(ui, &w);
            }
        }
        ToUi::SetWindowState {
            id,
            maximized,
            minimized,
        } => {
            if let Some(w) = window(ui, id) {
                set_window_state(&w, maximized, minimized);
            }
        }
        ToUi::Destroy { id } => {
            let w = ui.windows.borrow_mut().remove(&id);
            if let Some(w) = w {
                set_icon(&w, None);
                let hwnd = w.hwnd();
                if !hwnd.is_null() {
                    // SAFETY: the window was created on this thread, which is
                    // the only thread allowed to destroy it. This sends
                    // `WM_DESTROY`, which reclaims the `Rc` held in
                    // `GWLP_USERDATA`.
                    unsafe {
                        DestroyWindow(hwnd);
                    }
                }
                println!("wc-compositor: window {id} destroyed");
            }
        }
        ToUi::Quit => unreachable!("handled in drain"),
    }
}

fn create_window(
    ui: &Rc<Ui>,
    id: WindowId,
    title: &str,
    app_id: &str,
    shadow: Arc<Mutex<Shadow>>,
    kind: WindowKind,
) -> std::io::Result<()> {
    let state = Rc::new(WindowState {
        id,
        kind,
        hwnd: Cell::new(0),
        shadow,
        events: ui.events.clone(),
        frame: ui.frame,
        last_configured: Cell::new((0, 0)),
        maximized: Cell::new(false),
        minimized: Cell::new(false),
        mapped: Cell::new(false),
        self_driven: Cell::new(false),
        icon_big: Cell::new(0),
        frames: Cell::new(0),
        icon_small: Cell::new(0),
        tracking: Cell::new(false),
        buttons_down: Cell::new(0),
        last_pos: Cell::new((0.0, 0.0)),
    });

    let class = wide(CLASS_TOPLEVEL);
    let title_w = wide(title);
    // The owner of a popup, so Windows keeps it above the parent, destroys it
    // with the parent and keeps it off the taskbar. ADDED FOR STAGE 9.
    let owner = match kind {
        WindowKind::Toplevel => std::ptr::null_mut(),
        WindowKind::Popup { parent, .. } => window(ui, parent)
            .map(|w| w.hwnd())
            .unwrap_or(std::ptr::null_mut()),
    };
    // SAFETY: both string buffers outlive the call. The window is created
    // *hidden* — no `WS_VISIBLE` — because a Wayland surface is not mapped
    // until it has committed a buffer, and showing it earlier is a white flash
    // of an empty window before the first frame.
    let hwnd = unsafe {
        CreateWindowExW(
            // `WS_EX_NOACTIVATE` on a popup, and it is load-bearing rather
            // than tidy: a menu that took the foreground would take keyboard
            // focus away from the toplevel, the client would be told its
            // window was deactivated, and a GTK menu responds to that by
            // closing itself. `WS_EX_TOOLWINDOW` keeps it out of the taskbar
            // and out of alt-tab. ADDED FOR STAGE 9.
            match kind {
                WindowKind::Toplevel => 0,
                WindowKind::Popup { .. } => WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW,
            },
            class.as_ptr(),
            title_w.as_ptr(),
            // `WS_OVERLAPPEDWINDOW` is the whole point of stage 3: the
            // caption, the border, the sizing grip and the minimise / maximise
            // / close buttons are Windows', not ours. Everything the brief
            // asks for in item 1 follows from this constant and from *not*
            // handling `WM_NCPAINT`.
            //
            // `WS_POPUP` is the opposite and is right for the opposite reason:
            // a menu has no frame of its own to be native, and giving one a
            // caption would put a title bar on every dropdown in Firefox.
            //
            // `WS_CLIPCHILDREN | WS_CLIPSIBLINGS` cost nothing now and are what
            // subsurfaces will need when they become child windows or when a
            // scene graph starts compositing into this one.
            match kind {
                WindowKind::Toplevel => WS_OVERLAPPEDWINDOW,
                WindowKind::Popup { .. } => WS_POPUP,
            } | WS_CLIPCHILDREN
                | WS_CLIPSIBLINGS,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            owner,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    if hwnd.is_null() {
        return Err(std::io::Error::last_os_error());
    }
    state.hwnd.set(hwnd as isize);

    // Hand one strong reference to the window, reclaimed in `WM_DESTROY`.
    // SAFETY: the pointer is not dereferenced until a message arrives, by
    // which time the slot is set, and it is turned back into an `Rc` exactly
    // once.
    unsafe {
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, Rc::into_raw(state.clone()) as isize);
    }
    ui.windows.borrow_mut().insert(id, state);

    println!("wc-compositor: window {id} created (HWND {hwnd:p}) title={title:?} app_id={app_id:?}");
    Ok(())
}

/// Show the window for the first time, at the size the client chose.
fn map_window(w: &Rc<WindowState>, width: i32, height: i32) {
    // ADDED FOR STAGE 9. A popup is a different act of window management from a
    // toplevel and shares only the blit: it is placed where the positioner said
    // relative to its owner's client area, it has no frame to adjust for, and
    // it is shown without activation so the toplevel keeps the keyboard.
    if let WindowKind::Popup { parent: _, x, y } = w.kind {
        let mut origin = POINT { x, y };
        // SAFETY: live handles. `GetWindow(GW_OWNER)` is the owner set at
        // creation; `ClientToScreen` converts in place.
        unsafe {
            let owner = GetWindow(w.hwnd(), GW_OWNER);
            if !owner.is_null() {
                ClientToScreen(owner, &mut origin);
            }
            SetWindowPos(
                w.hwnd(),
                HWND_TOPMOST,
                origin.x,
                origin.y,
                width,
                height,
                SWP_NOACTIVATE,
            );
            if !w.mapped.replace(true) {
                ShowWindow(w.hwnd(), SW_SHOWNOACTIVATE);
            }
        }
        w.last_configured.set((width, height));
        println!(
            "wc-compositor: window {} mapped {width}x{height} as a popup at {},{}",
            w.id, origin.x, origin.y
        );
        return;
    }

    let (cw, ch) = w.frame.client_size_for(width, height);
    let mut rc = RECT {
        left: 0,
        top: 0,
        right: cw,
        bottom: ch,
    };
    // The Wayland size is the *content* size, which is Win32's *client* size —
    // the frame is Windows' and is not the client's business. So the outer
    // window has to be grown by whatever the current theme's border and caption
    // come to, which is what `AdjustWindowRectEx` is for and is why hard-coding
    // `GetSystemMetrics(SM_CYCAPTION)` would be wrong on a themed desktop.
    // SAFETY: `rc` is written in place; the style must match the window's.
    unsafe {
        AdjustWindowRectEx(&mut rc, WS_OVERLAPPEDWINDOW | WS_CLIPCHILDREN | WS_CLIPSIBLINGS, 0, 0);
    }
    w.self_driven.set(true);
    // SAFETY: live window handle; both calls send messages to `toplevel_proc`
    // on this thread before returning.
    unsafe {
        SetWindowPos(
            w.hwnd(),
            std::ptr::null_mut(),
            0,
            0,
            rc.right - rc.left,
            rc.bottom - rc.top,
            SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE,
        );
        if !w.mapped.replace(true) {
            ShowWindow(w.hwnd(), SW_SHOWNORMAL);
        }
    }
    w.self_driven.set(false);
    // The size we just imposed is the size the client already has, so recording
    // it here stops the `WM_SIZE` above from being echoed back as a configure.
    w.last_configured.set((width, height));

    // Say once, per window, whether the client's alpha is being thrown away.
    // GDI's `SetDIBitsToDevice` treats a 32-bit `BI_RGB` DIB as opaque, so an
    // `Argb8888` surface is composited against nothing and its translucent
    // pixels come out solid. That is a real limitation rather than a rounding
    // error — a client with a shaped or translucent window will look wrong —
    // and the fix is a layered window or `AlphaBlend`, which is not this stage.
    let alpha = {
        let shadow = w.shadow.lock().unwrap_or_else(|e| e.into_inner());
        shadow.format().has_alpha()
    };
    println!(
        "wc-compositor: window {} mapped {width}x{height}{}",
        w.id,
        if alpha {
            " (argb: alpha ignored by GDI, drawn opaque)"
        } else {
            " (xrgb)"
        }
    );
}

/// Write this frame out as a PPM, if `--dump-frames` asked for it.
///
/// # Why it is worth doing here as well as headless
///
/// `present` above invalidates a rectangle and lets `WM_PAINT` blit it, so
/// everything after this point is GDI's and the operating system's. A dump
/// taken here is therefore the compositor's **last** view of the pixels — what
/// it is about to hand to `SetDIBitsToDevice` — which is exactly the right
/// place to read from if the question is "did the client's bytes survive the
/// journey", and exactly the wrong place if the question is "did GDI draw
/// them". The screenshot answers the second; a harness that had only one of the
/// two could not tell a stride bug from a blit bug.
///
/// Done from the shadow rather than by reading the window back with `BitBlt`
/// for the same reason: reading the window back would measure the X server
/// under Wine, which `xwd` already does better and from outside.
fn dump_frame(ui: &Rc<Ui>, w: &Rc<WindowState>) {
    let Some(dir) = &ui.dump_frames else {
        return;
    };
    let n = w.frames.get();
    w.frames.set(n + 1);
    let path = dir.join(format!("window{}-{n:04}.ppm", w.id));
    let shadow = w.shadow.lock().unwrap_or_else(|e| e.into_inner());
    match crate::surface::write_ppm(&path, &shadow) {
        Ok(()) => {
            // Only the first, or an animating client fills the log with one
            // line per frame and buries everything the harness greps for.
            if n == 0 {
                println!("wc-compositor: writing frames to {}", dir.display());
            }
        }
        Err(e) => eprintln!("wc-compositor: could not write {}: {e}", path.display()),
    }
}

fn present(w: &Rc<WindowState>, damage: &[Rect]) {
    if !w.mapped.get() {
        return;
    }
    let (ox, oy) = w.frame.content_origin();
    for r in damage {
        let rc = RECT {
            left: r.x + ox,
            top: r.y + oy,
            right: r.x + ox + r.w,
            bottom: r.y + oy + r.h,
        };
        // `erase = FALSE`: the content rectangle is fully overwritten by the
        // blit, so erasing it first would be a visible flash of background
        // under every frame of an animation. Areas *outside* the buffer are
        // invalidated by Windows itself when the window grows, with erase set,
        // which is the case `WM_ERASEBKGND` handles.
        //
        // No `UpdateWindow`: `WM_PAINT` is synthesised by `GetMessage` only
        // when the queue is otherwise empty, so a burst of commits coalesces
        // into one paint for free. Forcing each one through would make a
        // 60 Hz client cost 60 paints whether or not the screen could show
        // them.
        //
        // SAFETY: live window; `rc` is read, not written.
        unsafe {
            InvalidateRect(w.hwnd(), &rc, 0);
        }
    }
}

fn set_window_state(w: &Rc<WindowState>, maximized: Option<bool>, minimized: Option<bool>) {
    w.self_driven.set(true);
    // SAFETY: live window handle. `ShowWindow` sends `WM_SIZE` synchronously;
    // `self_driven` is what stops that from being reported back to the client
    // as though the user had done it.
    unsafe {
        if let Some(true) = minimized {
            ShowWindow(w.hwnd(), SW_MINIMIZE);
        } else {
            match maximized {
                Some(true) => {
                    ShowWindow(w.hwnd(), SW_MAXIMIZE);
                }
                Some(false) => {
                    ShowWindow(w.hwnd(), SW_RESTORE);
                }
                None => {
                    if minimized == Some(false) {
                        ShowWindow(w.hwnd(), SW_RESTORE);
                    }
                }
            }
        }
    }
    w.self_driven.set(false);
}

/// Set the window and taskbar icon from pixels.
///
/// # Pixels, and never a path
///
/// The signature is the point. In the topology this project is aimed at, the
/// client is on a different machine from the window: `set_app_id` names a
/// desktop entry in a Linux icon theme, and turning that into something
/// `WM_SETICON` accepts means resolving it *there* and shipping the result
/// across (docs/goal-firefox.md). A function that took a file path or a
/// resource id would work on this machine and be unimplementable on the one
/// that matters.
///
/// **Status: implemented, unexercised.** No client path produces an icon yet —
/// `xdg-toplevel-icon-v1` is not bound and the app-id resolver does not exist —
/// so this runs only when `--debug-icon` synthesises one. That flag exists
/// precisely so this is not a code path that has never executed.
fn set_icon(w: &Rc<WindowState>, icon: Option<&IconImage>) {
    // Release whatever is there first: `WM_SETICON` does not take ownership,
    // so the previous `HICON` leaks unless it is destroyed here.
    for slot in [&w.icon_big, &w.icon_small] {
        let old = slot.replace(0);
        if old != 0 {
            // SAFETY: `old` came from `CreateIconIndirect` below and is
            // destroyed exactly once.
            unsafe {
                DestroyIcon(old as _);
            }
        }
    }
    let Some(icon) = icon else {
        return;
    };
    for (which, slot) in [(ICON_BIG, &w.icon_big), (ICON_SMALL, &w.icon_small)] {
        let (tw, th) = if which == ICON_BIG {
            // SAFETY: pure queries.
            unsafe { (GetSystemMetrics(SM_CXICON), GetSystemMetrics(SM_CYICON)) }
        } else {
            unsafe { (GetSystemMetrics(SM_CXSMICON), GetSystemMetrics(SM_CYSMICON)) }
        };
        let Some(hicon) = make_icon(icon, tw.max(1), th.max(1)) else {
            continue;
        };
        slot.set(hicon);
        // SAFETY: live window; `WM_SETICON` takes the handle as `lParam` and
        // does not take ownership of it.
        unsafe {
            SendMessageW(w.hwnd(), WM_SETICON, which as WPARAM, hicon);
        }
    }
    println!(
        "wc-compositor: window {} icon set from {}x{} pixels",
        w.id, icon.width, icon.height
    );
}

/// Build an `HICON` from BGRA pixels, nearest-neighbour scaled to `tw` x `th`.
///
/// Nearest-neighbour because this is a fallback path for an icon that arrived
/// at the wrong size; the right answer is for whatever resolves the app-id to
/// ship the sizes Windows asks for, which is what an icon theme has.
fn make_icon(icon: &IconImage, tw: i32, th: i32) -> Option<isize> {
    use windows_sys::Win32::Graphics::Gdi::{CreateBitmap, CreateDIBSection};

    if icon.width <= 0 || icon.height <= 0 {
        return None;
    }
    let need = (icon.width as usize).checked_mul(icon.height as usize)?.checked_mul(4)?;
    if icon.pixels.len() < need {
        return None;
    }

    let mut bmi: BITMAPINFO = unsafe { std::mem::zeroed() };
    bmi.bmiHeader = BITMAPINFOHEADER {
        biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
        biWidth: tw,
        biHeight: -th,
        biPlanes: 1,
        biBitCount: 32,
        biCompression: BI_RGB,
        biSizeImage: (tw as u32) * (th as u32) * 4,
        biXPelsPerMeter: 0,
        biYPelsPerMeter: 0,
        biClrUsed: 0,
        biClrImportant: 0,
    };
    let mut bits: *mut c_void = std::ptr::null_mut();
    // SAFETY: `bmi` is fully initialised and `bits` receives the allocation.
    let colour = unsafe {
        CreateDIBSection(
            std::ptr::null_mut(),
            &bmi,
            DIB_RGB_COLORS,
            &mut bits,
            std::ptr::null_mut(),
            0,
        )
    };
    if colour.is_null() || bits.is_null() {
        return None;
    }
    // SAFETY: the section is `tw * th * 4` bytes, which is what is written.
    unsafe {
        let dst = std::slice::from_raw_parts_mut(bits.cast::<u8>(), (tw * th * 4) as usize);
        for y in 0..th {
            let sy = (y as i64 * icon.height as i64 / th as i64) as usize;
            for x in 0..tw {
                let sx = (x as i64 * icon.width as i64 / tw as i64) as usize;
                let s = (sy * icon.width as usize + sx) * 4;
                let d = ((y * tw + x) * 4) as usize;
                dst[d..d + 4].copy_from_slice(&icon.pixels[s..s + 4]);
            }
        }
    }

    // A 32-bit colour bitmap with an alpha channel still needs a mask bitmap
    // present; an all-zero mask means "take the alpha from the colour bitmap",
    // which is what every modern icon does.
    // SAFETY: a 1bpp monochrome bitmap of the right size, zero-initialised by
    // passing a null bit pointer.
    let mask = unsafe { CreateBitmap(tw, th, 1, 1, std::ptr::null()) };

    let info = ICONINFO {
        fIcon: 1,
        xHotspot: 0,
        yHotspot: 0,
        hbmMask: mask,
        hbmColor: colour,
    };
    // SAFETY: both bitmaps are live; `CreateIconIndirect` copies them, so both
    // are deleted immediately afterwards.
    let hicon = unsafe { CreateIconIndirect(&info) };
    unsafe {
        DeleteObject(colour as _);
        DeleteObject(mask as _);
    }
    if hicon.is_null() {
        None
    } else {
        Some(hicon as isize)
    }
}

// ---------------------------------------------------------------------------
// Startup and the pump
// ---------------------------------------------------------------------------

pub(crate) struct PumpInner {
    ui: Rc<Ui>,
}

pub fn start(config: PlatformConfig) -> std::io::Result<Started> {
    if let Some(dir) = &config.dump_frames {
        std::fs::create_dir_all(dir)?;
    }
    if config.simulate_wm {
        // Honoured only by the headless platform, and refusing it here rather
        // than ignoring it silently is the point: on Windows these events come
        // from `WM_SIZE`, `WM_ACTIVATE` and `WM_CLOSE`, and a synthetic source
        // would be testing the harness instead of the platform.
        eprintln!("wc-compositor: --simulate-wm is headless-only and is ignored on Windows");
    }
    announce_dpi_awareness();
    register_classes()?;

    let (tx, rx) = std::sync::mpsc::channel();

    let class = wide(CLASS_CONTROLLER);
    let name = wide("waycompose");
    // A message-only window: `HWND_MESSAGE` as the parent means it is never
    // shown, never enumerated and never a taskbar entry, but it receives posted
    // messages like any other. See `Wake` for why this exists.
    // SAFETY: both string buffers outlive the call.
    let controller = unsafe {
        CreateWindowExW(
            0,
            class.as_ptr(),
            name.as_ptr(),
            0,
            0,
            0,
            0,
            0,
            HWND_MESSAGE,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    if controller.is_null() {
        return Err(std::io::Error::last_os_error());
    }

    let ui = Rc::new(Ui {
        windows: RefCell::new(HashMap::new()),
        commands: rx,
        events: config.events,
        frame: config.frame,
        draining: Cell::new(false),
        controller: Cell::new(controller as isize),
        dump_frames: config.dump_frames.clone(),
    });
    UI.with(|slot| *slot.borrow_mut() = Some(ui.clone()));

    Ok(assemble(
        tx,
        Wake {
            hwnd: controller as isize,
        },
        PumpInner { ui },
    ))
}

impl PumpInner {
    pub(crate) fn run(self) {
        println!("wc-compositor: win32 message pump running on the main thread");
        let mut msg: MSG = unsafe { std::mem::zeroed() };
        loop {
            // SAFETY: `msg` is written by the call. A return of 0 is `WM_QUIT`
            // and -1 is an error; both end the loop, which is why this is not
            // the `while GetMessage(..) != 0` that the Win32 documentation
            // warns about.
            let r = unsafe { GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) };
            if r <= 0 {
                break;
            }
            unsafe {
                TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }

        // Tear down every remaining window from the thread that owns them,
        // which is the only thread allowed to.
        let ids: Vec<WindowId> = self.ui.windows.borrow().keys().copied().collect();
        for id in ids {
            handle(&self.ui, ToUi::Destroy { id });
        }
        let controller = self.ui.controller.replace(0);
        if controller != 0 {
            // SAFETY: created on this thread, destroyed once, and no `Ui`
            // handle can post to it after `Stopped` is observed.
            unsafe {
                DestroyWindow(controller as HWND);
            }
        }
        let _ = self.ui.events.send(FromUi::Stopped);
        println!("wc-compositor: win32 message pump stopped");
    }
}

fn register_classes() -> std::io::Result<()> {
    // SAFETY: a null module name asks for the handle of the running .exe.
    let hinstance = unsafe { GetModuleHandleW(std::ptr::null()) };
    // SAFETY: `IDC_ARROW` is a predefined cursor id, not a pointer.
    let cursor = unsafe { LoadCursorW(std::ptr::null_mut(), IDC_ARROW) };

    for (name, proc) in [
        (
            CLASS_TOPLEVEL,
            toplevel_proc as unsafe extern "system" fn(HWND, u32, WPARAM, LPARAM) -> LRESULT,
        ),
        (CLASS_CONTROLLER, controller_proc),
    ] {
        let class_w = wide(name);
        let class = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            // Deliberately **not** `CS_HREDRAW | CS_VREDRAW`. Those invalidate
            // the entire client area on every resize message, which during a
            // drag is a full-window repaint per mouse-move and a visible flash
            // of background under content that has not changed. Without them
            // Windows invalidates only the newly exposed region, which is
            // exactly the "content anchored, background fills the rest"
            // behaviour `WindowState::paint` is built around.
            style: 0,
            lpfnWndProc: Some(proc),
            cbClsExtra: 0,
            cbWndExtra: 0,
            hInstance: hinstance as _,
            hIcon: std::ptr::null_mut(),
            hCursor: cursor,
            // Null, not a brush: `WM_ERASEBKGND` is handled so that only the
            // area outside the client's buffer is filled.
            hbrBackground: std::ptr::null_mut(),
            lpszMenuName: std::ptr::null(),
            lpszClassName: class_w.as_ptr(),
            hIconSm: std::ptr::null_mut(),
        };
        // SAFETY: `class_w` outlives the call, and Windows copies the struct.
        if unsafe { RegisterClassExW(&class) } == 0 {
            return Err(std::io::Error::last_os_error());
        }
    }
    Ok(())
}

/// Ask for per-monitor DPI awareness, and say what happened.
///
/// # Why this is here and why it is a `GetProcAddress`
///
/// A process that does not declare DPI awareness gets its windows *virtualised*
/// on a scaled desktop: Windows lies to it about sizes and then bitmap-stretches
/// the result, so `WM_SIZE` reports pre-scaling pixels and the client's crisp
/// buffer arrives on screen blurred. For a compositor whose entire job is to
/// put a client's exact pixels on screen at the size it configured, that is a
/// correctness bug rather than a polish item.
///
/// `SetProcessDpiAwarenessContext` is resolved at runtime rather than linked,
/// because it is a Windows 10 1703 entry point and a link-time reference would
/// mean the binary does not load at all on anything older — trading a
/// degradation for a failure.
///
/// **This is one of the items that Wine cannot settle** (docs/design.md §8.8):
/// Wine's DPI handling is not `ntdll`'s, and a success here says only that the
/// call was made.
fn announce_dpi_awareness() {
    // Per-monitor v2, the value of DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2.
    const PER_MONITOR_AWARE_V2: isize = -4;
    let lib = wide("user32.dll");
    // SAFETY: a live NUL-terminated name; user32 is already loaded, so this is
    // a reference-count bump on a module that cannot go away.
    let user32 = unsafe { LoadLibraryW(lib.as_ptr()) };
    if user32.is_null() {
        return;
    }
    // SAFETY: an ASCII, NUL-terminated symbol name.
    let sym = unsafe { GetProcAddress(user32, c"SetProcessDpiAwarenessContext".as_ptr().cast()) };
    match sym {
        Some(f) => {
            // SAFETY: the documented signature of
            // `SetProcessDpiAwarenessContext`: one `DPI_AWARENESS_CONTEXT`
            // (an opaque `isize`), returning `BOOL`.
            let f: unsafe extern "system" fn(isize) -> i32 =
                unsafe { std::mem::transmute(f) };
            let ok = unsafe { f(PER_MONITOR_AWARE_V2) };
            println!(
                "wc-compositor: SetProcessDpiAwarenessContext(PER_MONITOR_AWARE_V2) -> {}",
                if ok != 0 { "ok" } else { "refused" }
            );
        }
        None => {
            println!(
                "wc-compositor: no SetProcessDpiAwarenessContext; windows will be DPI-virtualised"
            );
        }
    }
}
