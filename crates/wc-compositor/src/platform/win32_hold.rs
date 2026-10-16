//! §96. Holding the pointer on Win32: `ClipCursor` and raw input.
//!
//! A child module of `win32.rs` so it can read the window state, and a file of
//! its own so that the whole of the Win32 half of
//! `zwp_pointer_constraints_v1` can be read in one place.
//!
//! # The two calls
//!
//! * **`ClipCursor`** keeps the system pointer inside a rectangle of the
//!   screen. A confinement is the region's bounding box; a lock is one pixel,
//!   at the centre of the window, which is where Windows games keep it and
//!   where the client finds the pointer when the lock ends.
//! * **Raw input** (`RegisterRawInputDevices`, `WM_INPUT`) reports the device's
//!   own motion. A clipped pointer stops producing `WM_MOUSEMOVE` the moment it
//!   reaches the clip, so a lock without raw input would turn the view by one
//!   pixel and stop. Registered only while a lock is held, and only for the
//!   mouse: keys and buttons keep arriving as the ordinary messages.
//!
//! # Letting go is not left to the Wayland thread
//!
//! A pointer clipped to a window the user has left is the failure that would
//! make this feature worse than not having it, and `ClipCursor` is
//! system-wide. So this side lets go **by itself** the moment the window loses
//! the keyboard (`WM_KILLFOCUS`) or the foreground (`WM_ACTIVATE`), before the
//! Wayland thread has heard of it, and on the window's destruction. What the
//! Wayland side asked for is remembered separately from what is in force, so
//! that coming back to the window — focus returning within §93's grace, where
//! the Wayland side never ended the lock — puts the clip back.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use windows_sys::Win32::Foundation::{LPARAM, POINT, RECT};
use windows_sys::Win32::Graphics::Gdi::ClientToScreen;
use windows_sys::Win32::UI::Input::{
    GetRawInputData, RegisterRawInputDevices, HRAWINPUT, MOUSE_MOVE_ABSOLUTE,
    MOUSE_VIRTUAL_DESKTOP, RAWINPUT, RAWINPUTDEVICE, RAWINPUTHEADER, RIDEV_REMOVE, RID_INPUT,
    RIM_TYPEMOUSE,
};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::GetFocus;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    ClipCursor, GetClientRect, GetClipCursor, GetSystemMetrics, SM_CXSCREEN,
    SM_CXVIRTUALSCREEN, SM_CYSCREEN, SM_CYVIRTUALSCREEN,
};

use super::{message_time, window, Ui, WindowState};
use crate::hold::{AbsoluteMotion, PointerHold};
use crate::platform::{FromUi, WindowId};

/// The UI thread's half of a hold. One per process, on [`Ui`]: there is one
/// system pointer.
#[derive(Default)]
pub(super) struct HoldState {
    /// What the Wayland side last asked for: `ToUi::PointerHold`.
    wanted: Cell<Option<(WindowId, PointerHold)>>,
    /// The clip rectangle this process put in force, in screen pixels, and
    /// whether raw input is registered with it.
    applied: Cell<Option<(RECT, bool)>>,
    /// See [`AbsoluteMotion`]: a tablet, a VM's absolute pointer or a remote
    /// desktop reports positions, and the lock wants distances.
    absolute: RefCell<AbsoluteMotion>,
    /// `WM_INPUT` messages turned into motion, and how many arrived at all,
    /// said when the hold ends: the one number that says whether raw input
    /// reached this process.
    moved: Cell<u64>,
    seen: Cell<u64>,
}

/// `ToUi::PointerHold`.
pub(super) fn set(ui: &Rc<Ui>, id: WindowId, hold: Option<PointerHold>) {
    match hold {
        Some(h) => {
            ui.hold.wanted.set(Some((id, h)));
            match window(ui, id) {
                // Only while the window has the keyboard. If it does not, the
                // Wayland side is a message behind the platform, and the
                // focus coming back will apply it.
                Some(w) if has_focus(&w) => apply(ui, &w, h),
                _ => println!(
                    "wc-compositor: window {id} pointer hold {h:?} wanted; not applied while \
                     the window does not have the keyboard"
                ),
            }
        }
        None => {
            if ui.hold.wanted.get().is_some_and(|(w, _)| w == id) {
                ui.hold.wanted.set(None);
            }
            release(ui, id, "the compositor let go");
        }
    }
}

/// `WM_SETFOCUS` (`gained`) or `WM_KILLFOCUS`/`WM_ACTIVATE(WA_INACTIVE)`.
pub(super) fn on_focus(ui: &Rc<Ui>, w: &WindowState, gained: bool) {
    let Some((id, h)) = ui.hold.wanted.get() else { return };
    if id != w.id {
        return;
    }
    if gained {
        apply(ui, w, h);
    } else {
        release(ui, id, "the window lost the keyboard");
    }
}

/// The window moved or changed size: a clip is in screen pixels, so it has to
/// follow.
pub(super) fn on_moved(ui: &Rc<Ui>, w: &WindowState) {
    if ui.hold.applied.get().is_none() {
        return;
    }
    if let Some((id, h)) = ui.hold.wanted.get()
        && id == w.id
    {
        apply(ui, w, h);
    }
}

/// The window is being destroyed.
pub(super) fn on_destroy(ui: &Rc<Ui>, id: WindowId) {
    if ui.hold.wanted.get().is_some_and(|(w, _)| w == id) {
        ui.hold.wanted.set(None);
        release(ui, id, "the window was destroyed");
    }
}

/// Whether `WM_MOUSEMOVE` for this window says nothing a client may hear: the
/// pointer is locked, so it does not move, and the motion arrives as
/// `WM_INPUT` instead.
pub(super) fn locked(ui: &Ui, id: WindowId) -> bool {
    ui.hold.applied.get().is_some_and(|(_, raw)| raw)
        && ui.hold.wanted.get().is_some_and(|(w, h)| w == id && h == PointerHold::Locked)
}

/// `WM_INPUT`. The caller passes the message on to `DefWindowProc`, which is
/// what frees the input's buffer for the system.
pub(super) fn on_input(ui: &Rc<Ui>, w: &WindowState, lparam: LPARAM) {
    // SAFETY: `RAWINPUT` is plain data; `size` says how much of it there is
    // room for, and `GetRawInputData` writes at most that and says how much it
    // wrote. A mouse report always fits: the struct is sized for the largest
    // of the three device kinds.
    let raw = unsafe {
        let mut raw: RAWINPUT = std::mem::zeroed();
        let mut size = std::mem::size_of::<RAWINPUT>() as u32;
        let got = GetRawInputData(
            lparam as HRAWINPUT,
            RID_INPUT,
            (&mut raw as *mut RAWINPUT).cast(),
            &mut size,
            std::mem::size_of::<RAWINPUTHEADER>() as u32,
        );
        if got == u32::MAX || got == 0 || raw.header.dwType != RIM_TYPEMOUSE {
            return;
        }
        raw
    };
    ui.hold.seen.set(ui.hold.seen.get() + 1);
    if !locked(ui, w.id) {
        return;
    }
    // SAFETY: `dwType` is `RIM_TYPEMOUSE`, so `mouse` is the member written.
    let m = unsafe { raw.data.mouse };
    let (dx, dy) = if m.usFlags & MOUSE_MOVE_ABSOLUTE != 0 {
        let span = if m.usFlags & MOUSE_VIRTUAL_DESKTOP != 0 {
            (metric(SM_CXVIRTUALSCREEN), metric(SM_CYVIRTUALSCREEN))
        } else {
            (metric(SM_CXSCREEN), metric(SM_CYSCREEN))
        };
        ui.hold.absolute.borrow_mut().delta(m.lLastX, m.lLastY, span)
    } else {
        (f64::from(m.lLastX), f64::from(m.lLastY))
    };
    // Something other than this process can take the clip away — the secure
    // desktop, a UAC prompt, another application calling `ClipCursor` — and
    // nothing tells us. Raw input keeps coming, so this is where it is noticed.
    reassert(ui);
    if dx == 0.0 && dy == 0.0 {
        return;
    }
    ui.hold.moved.set(ui.hold.moved.get() + 1);
    w.send(FromUi::PointerRelative { id: w.id, dx, dy, time: message_time() });
}

fn metric(i: i32) -> i32 {
    // SAFETY: a pure query.
    unsafe { GetSystemMetrics(i) }
}

fn has_focus(w: &WindowState) -> bool {
    // SAFETY: a pure query on this thread.
    unsafe { GetFocus() == w.hwnd() }
}

/// Put the hold in force: the clip, and for a lock the raw-input registration.
fn apply(ui: &Rc<Ui>, w: &WindowState, hold: PointerHold) {
    let hwnd = w.hwnd();
    // The client rectangle, in client pixels — which are surface pixels
    // (`WindowState::to_surface` is the identity).
    let mut client = RECT { left: 0, top: 0, right: 0, bottom: 0 };
    // SAFETY: a live window on its own thread; `client` is written in place.
    unsafe {
        GetClientRect(hwnd, &mut client);
    }
    let local = match hold {
        PointerHold::Locked => {
            let (cx, cy) = ((client.left + client.right) / 2, (client.top + client.bottom) / 2);
            RECT { left: cx, top: cy, right: cx + 1, bottom: cy + 1 }
        }
        PointerHold::Confined(r) => RECT {
            left: r.x.max(client.left),
            top: r.y.max(client.top),
            right: (r.x + r.w).min(client.right),
            bottom: (r.y + r.h).min(client.bottom),
        },
    };
    if local.right <= local.left || local.bottom <= local.top {
        println!("wc-compositor: window {} pointer hold {hold:?} is empty here; not applied", w.id);
        return;
    }
    let mut tl = POINT { x: local.left, y: local.top };
    let mut br = POINT { x: local.right, y: local.bottom };
    // SAFETY: a live window; both points are converted in place.
    unsafe {
        ClientToScreen(hwnd, &mut tl);
        ClientToScreen(hwnd, &mut br);
    }
    let screen = RECT { left: tl.x, top: tl.y, right: br.x, bottom: br.y };
    let raw = hold == PointerHold::Locked;
    let had_raw = ui.hold.applied.get().is_some_and(|(_, r)| r);
    // SAFETY: a fully initialised rectangle.
    let clipped = unsafe { ClipCursor(&screen) } != 0;
    if raw && !had_raw {
        let dev = RAWINPUTDEVICE {
            usUsagePage: 0x01, // generic desktop
            usUsage: 0x02,     // mouse
            dwFlags: 0,
            hwndTarget: hwnd,
        };
        // SAFETY: one initialised device, and its size.
        let ok = unsafe {
            RegisterRawInputDevices(&dev, 1, std::mem::size_of::<RAWINPUTDEVICE>() as u32)
        } != 0;
        if !ok {
            println!(
                "wc-compositor: window {} RegisterRawInputDevices failed ({}); a locked pointer \
                 will not report motion",
                w.id,
                std::io::Error::last_os_error()
            );
        }
        ui.hold.absolute.borrow_mut().reset();
    } else if !raw && had_raw {
        unregister_raw();
    }
    let was = ui.hold.applied.replace(Some((screen, raw)));
    if was.map(|(r, k)| (rect_tuple(r), k)) != Some((rect_tuple(screen), raw)) {
        println!(
            "wc-compositor: window {} pointer hold {hold:?} in force: ClipCursor({},{} {}x{}) {}{}",
            w.id,
            screen.left,
            screen.top,
            screen.right - screen.left,
            screen.bottom - screen.top,
            if clipped { "ok" } else { "FAILED" },
            if raw { ", raw input registered" } else { "" },
        );
    }
}

/// Take the hold out of force, if this process put one in.
fn release(ui: &Rc<Ui>, id: WindowId, why: &str) {
    let Some((_, raw)) = ui.hold.applied.take() else { return };
    // SAFETY: a null rectangle is the documented "no clip".
    unsafe {
        ClipCursor(std::ptr::null());
    }
    if raw {
        unregister_raw();
    }
    println!(
        "wc-compositor: window {id} pointer hold released ({why}); {} raw-input reports, {} moved \
         the view",
        ui.hold.seen.replace(0),
        ui.hold.moved.replace(0)
    );
}

fn unregister_raw() {
    let dev = RAWINPUTDEVICE {
        usUsagePage: 0x01,
        usUsage: 0x02,
        dwFlags: RIDEV_REMOVE,
        // `RIDEV_REMOVE` requires a null target.
        hwndTarget: std::ptr::null_mut(),
    };
    // SAFETY: one initialised device, and its size.
    unsafe {
        RegisterRawInputDevices(&dev, 1, std::mem::size_of::<RAWINPUTDEVICE>() as u32);
    }
}

/// Put the clip back if somebody else changed it.
fn reassert(ui: &Ui) {
    let Some((want, _)) = ui.hold.applied.get() else { return };
    let mut now = RECT { left: 0, top: 0, right: 0, bottom: 0 };
    // SAFETY: `now` is written in place.
    let read = unsafe { GetClipCursor(&mut now) } != 0;
    if read && rect_tuple(now) != rect_tuple(want) {
        // SAFETY: a fully initialised rectangle.
        unsafe {
            ClipCursor(&want);
        }
    }
}

fn rect_tuple(r: RECT) -> (i32, i32, i32, i32) {
    (r.left, r.top, r.right, r.bottom)
}
