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
//!
//! §104's follow-up narrowed the third without giving up the unambiguous form:
//! the image handed over is the band of rows the update region spans, as an
//! image of its own (`WindowState::blit`). And a window whose picture is one
//! client buffer in a section is drawn with `BitBlt` from a DIB section made
//! over that buffer (`DirectDib`), which is the fix this paragraph named.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::ffi::c_void;
use std::rc::Rc;
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};

use super::caption;
// §96: `ClipCursor` and raw input, the Win32 half of `zwp_pointer_constraints_v1`.
#[path = "win32_hold.rs"]
mod hold;

use windows_sys::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows_sys::Win32::Graphics::Gdi::{
    BeginPaint, BitBlt, CreateSolidBrush, DeleteObject, EndPaint, ExcludeClipRect, FillRect,
    InvalidateRect, SetDIBitsToDevice, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS,
    HBRUSH, HDC, PAINTSTRUCT, SRCCOPY,
    // ADDED FOR STAGE 9. `ClientToScreen` is filed under GDI rather than under
    // WindowsAndMessaging, which is where one would look for it.
    ClientToScreen,
    // §82: the caption buttons Windows draws when DWM will not say where its
    // own are — Wine, chiefly. Filed under GDI with the other drawing calls.
    DrawFrameControl, DFCS_CAPTIONCLOSE, DFCS_CAPTIONMAX, DFCS_CAPTIONMIN, DFCS_CAPTIONRESTORE,
    DFC_CAPTION,
    // ADDED FOR STAGE 13, for per-pixel alpha on popups. `BLENDFUNCTION` and
    // its two constants are filed under GDI even though the call that takes
    // them, `UpdateLayeredWindow`, is filed under WindowsAndMessaging.
    AC_SRC_ALPHA, AC_SRC_OVER, BLENDFUNCTION,
    // ADDED FOR STAGE 20, filed in the same surprising place, and needed
    // because `POINTER_INFO` reports in **screen** coordinates while every
    // other input message this file handles reports in client ones.
    ScreenToClient,
    // ADDED FOR STAGE 38, and the first time this project asks Windows where
    // anything is. `MonitorFromWindow` picks the monitor a window is on and
    // `GetMonitorInfoW` gives its work area; both are filed under GDI, which
    // is not where a reader looks for them either.
    GetMonitorInfoW, MonitorFromWindow, MONITORINFO, MONITOR_DEFAULTTONEAREST,
};
// ADDED FOR STAGE 20. See `on_pointer`: the `WM_POINTER*` family rather than
// `RegisterTouchWindow`/`WM_TOUCH`, and this is the whole of the extra API.
use windows_sys::Win32::UI::Input::Pointer::{
    GetPointerTouchInfo, GetPointerType, POINTER_FLAG_CANCELED, POINTER_FLAG_INCONTACT,
    POINTER_TOUCH_INFO,
};
use windows_sys::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress, LoadLibraryW};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    // ADDED FOR STAGE 13, for the locked modifiers. See `lock_on`.
    GetKeyState, VK_CAPITAL, VK_NUMLOCK,
    // ADDED FOR STAGE 20: a touch that consumes `WM_POINTERDOWN` also consumes
    // the `DefWindowProc` that would have moved the focus, so `on_pointer`
    // moves it by hand.
    GetFocus, SetFocus,
    GetKeyboardLayout, ReleaseCapture, SetCapture, TrackMouseEvent, TME_LEAVE, TRACKMOUSEEVENT,
};
// ADDED FOR STAGE 12, for the cursor (docs/waycompose-design.md §23). `SetCursor` and
// `DestroyCursor` are the whole of the Win32 half; `HTCLIENT` is the hit-test
// code that decides whether the client's cursor applies or the system's frame
// keeps its own.
use windows_sys::Win32::UI::WindowsAndMessaging::{
    // ADDED FOR STAGE 38: the primary monitor's work area, before any window
    // exists. See `primary_work_area`.
    SPI_GETWORKAREA,
    DestroyCursor, SetCursor, SystemParametersInfoW, HTCLIENT, SPI_GETKEYBOARDDELAY,
    SPI_GETKEYBOARDSPEED, WM_SETCURSOR,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    // ADDED FOR STAGE 54: asking Windows what still exists, rather than asking
    // this process what it believes. See `live_toplevels`.
    EnumWindows, GetClassNameW, GetWindowThreadProcessId,
    AdjustWindowRectEx, CreateIconIndirect, CreateWindowExW, DefWindowProcW, DestroyIcon,
    DestroyWindow, DispatchMessageW, GetClientRect, GetMessageTime, GetMessageW, GetSystemMetrics,
    GetWindowLongPtrW, GetWindowRect, LoadCursorW, PostMessageW, PostQuitMessage,
    RegisterClassExW, SendMessageW, SetWindowLongPtrW, SetWindowPos, SetWindowTextW, ShowWindow,
    TranslateMessage, CW_USEDEFAULT, GWLP_USERDATA, HWND_MESSAGE, ICONINFO, ICON_BIG, ICON_SMALL,
    IDC_ARROW, MSG, SM_CXICON, SM_CXSCREEN, SM_CXSMICON, SM_CYICON, SM_CYSCREEN, SM_CYSMICON,
    // ADDED FOR STAGE 38: the three messages that change where a window's
    // popups may go. See `toplevel_proc`. (`GWL_STYLE`, which stage 38 also
    // needs, is imported with the title-bar group below.)
    WM_DISPLAYCHANGE, WM_SETTINGCHANGE, WM_WINDOWPOSCHANGED,
    // ADDED FOR STAGE 13, for per-pixel alpha on popups. See `update_layered`.
    UpdateLayeredWindow, ULW_ALPHA, WS_EX_LAYERED,
    SIZE_MAXIMIZED,
    SIZE_MINIMIZED, SIZE_RESTORED, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOZORDER, SW_MAXIMIZE,
    // ADDED FOR STAGE 17: the three toplevel requests a client-decorated
    // window makes, handed to Windows' own modal loops.
    GetSystemMenu, TrackPopupMenu, SC_MOVE, SC_SIZE, TPM_RETURNCMD, TPM_RIGHTBUTTON,
    // §82: where the pointer is when a move is handed over. See
    // `begin_sysmenu_gesture`.
    GetCursorPos,
    SW_MINIMIZE, SW_RESTORE, SW_SHOWNORMAL, WA_INACTIVE, WHEEL_DELTA, WM_ACTIVATE, WM_APP,
    WM_CLOSE, WM_DESTROY, WM_ERASEBKGND, WM_INPUT, WM_INPUTLANGCHANGE, WM_KEYDOWN, WM_KEYUP,
    WM_KILLFOCUS, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MBUTTONDOWN, WM_MBUTTONUP, WM_MOUSEHWHEEL,
    WM_MOUSEMOVE, WM_MOUSEWHEEL, WM_PAINT, WM_RBUTTONDOWN, WM_RBUTTONUP,
    WM_SETFOCUS, WM_SETICON, WM_SIZE, WM_SYSKEYDOWN, WM_SYSKEYUP, WM_XBUTTONDOWN, WM_XBUTTONUP,
    WNDCLASSEXW, WS_CLIPCHILDREN, WS_CLIPSIBLINGS, WS_OVERLAPPEDWINDOW, XBUTTON1,
    // ADDED FOR STAGE 20: touch. `SM_MAXIMUMTOUCHES` is how a machine says
    // whether it has a digitiser at all, and the `PT_*` and `TOUCH_MASK_*`
    // constants are filed here rather than beside the functions that produce
    // them.
    PT_TOUCH, POINTER_INPUT_TYPE, SM_MAXIMUMTOUCHES, TOUCH_MASK_CONTACTAREA,
    TOUCH_MASK_ORIENTATION, WM_POINTERCAPTURECHANGED, WM_POINTERDOWN, WM_POINTERENTER,
    WM_POINTERLEAVE, WM_POINTERUP, WM_POINTERUPDATE,
    // ADDED FOR STAGE 9, for popups.
    GetWindow, GW_OWNER, HWND_TOPMOST, SW_SHOWNOACTIVATE, WS_EX_NOACTIVATE,
    WS_EX_TOOLWINDOW, WS_POPUP,
    // §93: a click on a popup activates nothing. See `WM_MOUSEACTIVATE`.
    MA_NOACTIVATE, WM_MOUSEACTIVATE,
    // ADDED FOR STAGE 21: `xdg_activation_v1` (raise a window) and the
    // screensaver half of `zwp_idle_inhibit_manager_v1`.
    FlashWindowEx, GetForegroundWindow, IsIconic, SetForegroundWindow, FLASHWINFO, FLASHW_TIMERNOFG,
    FLASHW_TRAY, SC_MONITORPOWER, WM_SYSCOMMAND,
    // ADDED FOR TABS IN THE TITLE BAR: the caption folded into the client area. These four
    // messages and six metrics are the whole of it — see `nccalcsize` and
    // `nchittest`. `SWP_FRAMECHANGED` is what makes a window that already
    // exists ask `WM_NCCALCSIZE` again, and without it the fold happens on the
    // *next* resize instead of at map, which looks like a compositor that
    // works only after the user grabs a corner.
    GWL_STYLE, NCCALCSIZE_PARAMS, SM_CXPADDEDBORDER,
    SM_CXSIZE, SM_CXSIZEFRAME, SM_CYSIZEFRAME, SWP_FRAMECHANGED, SWP_NOSIZE, WM_DPICHANGED, WM_NCCALCSIZE,
    WM_NCHITTEST, WS_MAXIMIZE,
    // The five DWM cares about. `WM_NCHITTEST` is handled on its own path
    // because it has an answer to fall back to; these five are pure forwarding.
    WM_NCLBUTTONDOWN, WM_NCLBUTTONUP, WM_NCMOUSELEAVE, WM_NCMOUSEMOVE, WM_NCRBUTTONUP,
    // §91: the three messages `DefWindowProc` answers by painting a caption
    // over the folded one (see `defproc_without_caption_paint`), and a new
    // toplevel's size: the client's own, its min and max, and a user's drag.
    WM_SETTEXT, WM_NCACTIVATE, WS_VISIBLE, WM_GETMINMAXINFO, MINMAXINFO, WM_ENTERSIZEMOVE,
    WM_EXITSIZEMOVE, IsZoomed, SW_SHOWMAXIMIZED,
    // §95: the fourth, and the one every frame repaint goes through.
    WM_NCPAINT,
};
// ADDED FOR STAGE 21. `SetThreadExecutionState` is filed under System::Power
// rather than anywhere near the window APIs, because it is not about a window:
// it is a property of the calling thread. See `set_idle_inhibit`.
use windows_sys::Win32::System::Power::{
    SetThreadExecutionState, ES_CONTINUOUS, ES_DISPLAY_REQUIRED,
};
// ADDED FOR STAGE 32: the taskbar identity. `SHGetPropertyStoreForWindow` is
// the only entry point in this file that hands back a **COM interface**, and
// the workspace manifest's "no COM" note stops being true here — see
// `set_taskbar_identity` for what that costs and why nothing else will do.
use windows_sys::Win32::System::Com::{CoInitializeEx, CoTaskMemFree, COINIT_APARTMENTTHREADED};
use windows_sys::Win32::UI::Shell::PropertiesSystem::SHGetPropertyStoreForWindow;
use windows_sys::Win32::UI::WindowsAndMessaging::{SW_HIDE, SW_SHOWNA};
// §96: a fullscreen window's placement, so a restore from the taskbar comes back
// over the monitor (`set_fullscreen`).
use windows_sys::Win32::UI::WindowsAndMessaging::{
    GetWindowPlacement, SetWindowPlacement, GWL_EXSTYLE, SW_SHOWMINIMIZED, WINDOWPLACEMENT,
    WPF_RESTORETOMAXIMIZED,
};
use windows_sys::Win32::Foundation::{GlobalFree, POINT, PROPERTYKEY, SIZE};
use windows_sys::core::{GUID, HRESULT};
// ADDED FOR THE CLIPBOARD. The clipboard lives in `DataExchange` rather than with
// the window messages, and its payloads are moveable global handles rather than
// pointers, which is why `Memory` comes with it. See `set_clipboard`.
use windows_sys::Win32::System::DataExchange::{
    AddClipboardFormatListener, CloseClipboard, EmptyClipboard, EnumClipboardFormats,
    GetClipboardData, IsClipboardFormatAvailable, OpenClipboard, RegisterClipboardFormatW,
    RemoveClipboardFormatListener, SetClipboardData,
};
use windows_sys::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock, GMEM_MOVEABLE};

use crate::clipboard::SystemFormat;

use crate::input::{evdev, evdev_keycode};

use super::{
    assemble, Commands, CursorSpec, Edge, FrameStyle, FromUi, IconImage, PlatformConfig,
    PopupBounds, Started, ToUi, WindowId, WindowKind,
};
use crate::surface::{Rect, Shadow};

/// "Drain the command queue." The only message this module posts.
const WM_WC_COMMAND: u32 = WM_APP + 1;
/// "Put up the system menu", with the screen position packed into the `LPARAM`
/// the way `WM_CONTEXTMENU` packs one.
///
/// ADDED FOR STAGE 17. A message of its own rather than a call from `drain`,
/// for the reason the `ToUi::WindowMenu` arm gives: `TrackPopupMenu` does not
/// return until the menu closes.
const WM_WC_SYSMENU: u32 = WM_APP + 2;

/// `WM_MOUSELEAVE`, 0x02A3.
///
/// Written out rather than imported, because `windows-sys` files it under
/// `Win32::UI::Controls` — with the common controls, not with the other mouse
/// messages — and pulling that whole feature in for one integer would add a
/// large module to every build to say `675`. `winuser.h` declares it beside
/// `WM_MOUSEHOVER` and `TrackMouseEvent`; the metadata's placement is a fact
/// about the Windows SDK's headers rather than about the message.
const WM_MOUSELEAVE: u32 = 0x02A3;

/// `SC_SCREENSAVE`, 0xF140 — the `WM_SYSCOMMAND` the system sends to the
/// foreground window when the screen saver is about to start.
///
/// Written out for the same reason as `WM_MOUSELEAVE`: `windows-sys` has
/// `SC_MONITORPOWER` and does not have this one, which is a fact about which
/// constants the Windows metadata chose to export rather than about the message.
/// `winuser.h` declares the two on consecutive lines.
const SC_SCREENSAVE: u32 = 0xF140;

/// The mask `WM_SYSCOMMAND` requires before comparing `wParam`.
///
/// The low four bits are used internally by the system, so the documented way
/// to test a system command is `(wParam & 0xFFF0) == SC_...`. Comparing the raw
/// `wParam` works most of the time, which is worse than never working.
const SC_MASK: usize = 0xFFF0;
/// `WM_CLIPBOARDUPDATE`, 0x031D: the system clipboard's contents changed.
///
/// Delivered to every window registered with `AddClipboardFormatListener`,
/// which is how a clipboard is watched now — the old route was a
/// `SetClipboardViewer` chain, where one badly behaved application that forgot
/// to forward broke the notification for everything after it. There is nothing
/// to unhook here except our own registration.
const WM_CLIPBOARDUPDATE: u32 = 0x031D;

/// `CF_UNICODETEXT` and `CF_HDROP`.
///
/// Written out for the reason [`WM_MOUSELEAVE`] is: `windows-sys` files the
/// clipboard format constants under `Win32::System::Ole`, which is a large
/// module to compile into every build in order to say `13`. The numbers are in
/// `winuser.h` and have not moved since Windows 3.1.
const CF_UNICODETEXT: u32 = 13;
const CF_HDROP: u32 = 15;
/// §93. `CF_DIB`: a bitmap with its header, which is how a picture is read.
const CF_DIB: u32 = 8;

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
    /// This window's frame arrangement.
    ///
    /// **A `Cell`, and per window, since §46.** It was one value for the whole
    /// run, copied in from `ui.frame` at creation, because `--decorations` was
    /// the only thing that decided it. It is now settled per window from what
    /// the client turned out to be (`ToUi::Map`'s `fold_caption`), because
    /// §31's arrangement is right for a client that decorates itself and wrong
    /// for one that does not, and a run contains both.
    ///
    /// Written exactly once, in [`map_window`], **before** the window is shown.
    /// Every reader below is on the UI thread and every read is a `get`, so
    /// this is the same shared-`&self` discipline as every other `Cell` in this
    /// struct and for the same reason: a Win32 call can reenter the window
    /// procedure from inside `drain`.
    frame: Cell<FrameStyle>,

    /// The last surface size we sent as a configure. Windows emits `WM_SIZE`
    /// for every pixel of a drag; sending a configure per pixel is legal but
    /// pointless, and this is what makes the negotiation quiet when nothing
    /// changed.
    last_configured: Cell<(i32, i32)>,
    maximized: Cell<bool>,
    minimized: Cell<bool>,
    mapped: Cell<bool>,
    /// The first `Present` not yet painted, for `WC_FRAME_TRACE`
    /// (docs/waycompose-design.md §88): how long a frame waits for `WM_PAINT`.
    unpainted_since: Cell<Option<std::time::Instant>>,
    /// §104's follow-up: DIB sections over client buffers this window has been
    /// told to draw from directly (`ToUi::Direct`), newest last, and the key
    /// of the one it draws from now; `None` is the shadow.
    dibs: RefCell<Vec<DirectDib>>,
    direct_on: Cell<Option<u64>>,
    /// Set while we are the ones calling `ShowWindow`, so the `WM_SIZE` it
    /// provokes is not reported back to the client as a user action. Without
    /// it, a client asking to be maximised receives its own request as an
    /// event and toolkits that resend on change oscillate.
    self_driven: Cell<bool>,
    /// §91: set while this process places the window itself (`map_window`,
    /// `follow_client_size`). Every `WM_SIZE` in that span is the size the
    /// client asked for, or one Windows passed through on the way to it
    /// (`ShowWindow` on a `CW_USEDEFAULT` window sizes it to the cascade
    /// first), and none of them is news to the client. What the client is
    /// told is decided once, after the placement: nothing if it got what it
    /// committed, a configure if the work area made it smaller.
    placing: Cell<bool>,
    /// §91: the client asked to be maximised before the window was shown;
    /// `map_window` shows it maximised.
    maximize_at_map: Cell<bool>,
    /// §96: while fullscreen, the style and the placement to go back to.
    fullscreen: Cell<Option<(isize, WINDOWPLACEMENT)>>,
    /// §91: inside `WM_ENTERSIZEMOVE` … `WM_EXITSIZEMOVE`, when the user owns
    /// the window's size and a client commit must not resize it.
    user_sizing: Cell<bool>,
    /// §91: `xdg_toplevel.set_min_size` and `set_max_size`, client pixels,
    /// 0 for unset. Answered in `WM_GETMINMAXINFO`.
    size_hints: Cell<((i32, i32), (i32, i32))>,
    icon_big: Cell<isize>,
    /// How many frames this window has presented, for the dump filenames.
    frames: Cell<u32>,
    icon_small: Cell<isize>,
    /// ADDED FOR TABS IN THE TITLE BAR. How many pixels of top frame `DefWindowProc` wanted
    /// the last time it was asked, which is how many `WM_NCCALCSIZE` handed to
    /// the client. Recorded rather than computed because it is the *window's*
    /// number at the *window's* DPI as `DefWindowProc` computed it.
    nc_top: Cell<i32>,
    /// The last colour handed to `DWMWA_CAPTION_COLOR`, so the three DWM calls
    /// happen when the client's title bar changes colour and not once per
    /// frame of every animation it runs.
    caption_colour: Cell<Option<u32>>,
    /// §82: the last verdict of `check_corner`, so it is printed on change.
    corner_clear: Cell<Option<bool>>,
    /// §82: whether `draw_fallback_buttons` has said so yet.
    fallback_said: Cell<bool>,
    /// §85. For a popup: where it sits in its owner's client coordinates now,
    /// which is `kind`'s `x, y` until `ToUi::MovePopup` says otherwise. Kept so
    /// the popup can be put back there when the *owner* moves: an owned
    /// `WS_POPUP` does not follow its owner on Windows, and `xdg_popup` is
    /// positioned relative to its parent, so without this a menu stayed where
    /// the window had been.
    popup_offset: Cell<(i32, i32)>,
    /// The taskbar identity currently on this window's property store.
    ///
    /// ADDED FOR STAGE 32. Held so that a repeated `set_app_id` with the same
    /// string — which is a thing clients do — does not rebuild a taskbar button
    /// that is already right. A `RefCell` rather than a `Cell` because it has
    /// to be *read* to answer that; it is borrowed only for the comparison and
    /// never across a Win32 call, which is this module's rule.
    aumid: RefCell<String>,
    /// §102: the identity this window is meant to have, whether or not the
    /// shell's property store took it (Wine's does not implement it). What
    /// `super::Succession` matches on: which application a window belongs to
    /// is a fact about the client, not about the taskbar.
    identity: RefCell<String>,

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

    // --- popups, stage 38 --------------------------------------------------
    /// The rectangle this window's popups must stay inside, shared with the
    /// Wayland thread. See [`crate::platform::PopupBounds`] and
    /// [`WindowState::publish_popup_bounds`].
    bounds: PopupBounds,
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
            let blit_started = std::time::Instant::now();
            // §104's follow-up: from the client's buffer when the window has
            // been told to, else from the shadow.
            let direct = self.blit_direct(hdc, &ps.rcPaint);
            if !direct {
                self.blit(hdc, &ps.rcPaint);
            }
            if let Some(since) = self.unpainted_since.take()
                && sv_transport::frametrace::enabled()
            {
                use sv_transport::frametrace as ft;
                let r = ps.rcPaint;
                ft::emit_at(
                    ft::at_us(blit_started),
                    "wc.paint",
                    format_args!(
                        "win={} wait_us={} blit_us={} rect={}x{} direct={}",
                        self.id,
                        ft::us(blit_started - since),
                        ft::us(blit_started.elapsed()),
                        r.right - r.left,
                        r.bottom - r.top,
                        u8::from(direct),
                    ),
                );
            }
            // After the blit and inside the paint, because it reads the same
            // buffer the blit just used and must not disagree with what is on
            // the screen. It is a no-op on every frame where the client's title
            // bar has not changed colour, which is almost all of them. Not when
            // drawing directly: the shadow is not the picture then (and a
            // window with a folded caption is never drawn directly).
            if !direct {
                self.match_caption_colour();
                self.check_corner();
            }
        }
        // SAFETY: paired with the `BeginPaint` above, same `ps`.
        unsafe {
            EndPaint(hwnd, &ps);
        }
    }

    /// Draw the update region from the client buffer this window is drawn from
    /// directly, if it is; `false` when it is drawn from its shadow.
    ///
    /// §104's follow-up. A `BitBlt` from a memory DC holding a DIB section made
    /// over the client's own section (`DirectDib`): nothing of the client's
    /// frame has been read by this process, GDI reads the rows it draws, and
    /// only those of the update region. Outside the buffer nothing is drawn,
    /// exactly as `blit` does for a shadow smaller than the window.
    fn blit_direct(&self, hdc: HDC, paint: &RECT) -> bool {
        let Some(key) = self.direct_on.get() else { return false };
        let dibs = self.dibs.borrow();
        let Some(dib) = dibs.iter().find(|d| d.key == key) else { return false };
        let left = paint.left.clamp(0, dib.width);
        let top = paint.top.clamp(0, dib.height);
        let right = paint.right.clamp(left, dib.width);
        let bottom = paint.bottom.clamp(top, dib.height);
        if right > left && bottom > top {
            // SAFETY: `hdc` is the paint DC and `dib.dc` a live memory DC with
            // the DIB selected; the rectangle is inside both.
            unsafe {
                BitBlt(hdc, left, top, right - left, bottom - top, dib.dc, left, top, SRCCOPY);
            }
        }
        drop(dibs);
        self.uncover_caption_buttons(hdc);
        true
    }

    fn blit(&self, hdc: HDC, paint: &RECT) {
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
        // §104's follow-up: only the rows the update region spans are handed
        // to GDI, as an image of their own — still the whole-image form this
        // module's header argues for (zero source origin, every line), so the
        // band is unambiguous; the clip `BeginPaint` installed still decides
        // the columns. A 64x64 change in a 1280x720 window hands GDI 64 rows
        // instead of 720.
        let top = paint.top.clamp(0, h);
        let rows = paint.bottom.clamp(top, h) - top;
        if rows == 0 {
            self.uncover_caption_buttons(hdc);
            return;
        }

        let mut bmi: BITMAPINFO = unsafe { std::mem::zeroed() };
        bmi.bmiHeader = BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: w,
            // Negative height means top-down, which is Wayland's origin. A
            // positive value here is the classic upside-down window.
            biHeight: -rows,
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB,
            biSizeImage: (w as u32) * (rows as u32) * 4,
            biXPelsPerMeter: 0,
            biYPelsPerMeter: 0,
            biClrUsed: 0,
            biClrImportant: 0,
        };
        let band = &shadow.pixels()[top as usize * w as usize * 4..];

        // SAFETY: `bmi` describes exactly `w * rows * 4` bytes, and `band`
        // starts at row `top` of a shadow packed to `w * h * 4` bytes with
        // `top + rows <= h`; the pointer is valid for the duration of the call
        // because the lock is still held. The whole band is passed with a zero
        // source origin, which is the unambiguous form — see this module's
        // header.
        unsafe {
            SetDIBitsToDevice(
                hdc,
                0,
                top,
                w as u32,
                rows as u32,
                0,
                0,
                0,
                rows as u32,
                band.as_ptr().cast::<c_void>(),
                &bmi,
                DIB_RGB_COLORS,
            );
        }
        self.uncover_caption_buttons(hdc);
    }

    /// Give the caption buttons back to DWM, by making the pixels over them
    /// **transparent**.
    ///
    /// ADDED FOR TABS IN THE TITLE BAR, and it is the whole mechanism by which a window with
    /// a custom frame keeps the system's real buttons.
    ///
    /// # Why a `FillRect` with black is a hole rather than a black rectangle
    ///
    /// Inside a region handed to `DwmExtendFrameIntoClientArea`, DWM composites
    /// the window's pixels over the frame it drew — buttons and all — using the
    /// **per-pixel alpha** in the redirection surface. GDI has no notion of
    /// alpha: every drawing call writes zero into the fourth byte, which is the
    /// long-standing wart that made "glass" work at all in the first place. So
    /// `FillRect` here writes `alpha = 0` over the button strip, DWM's own
    /// caption buttons show through it, and the client's pixels keep the rest
    /// of the caption because `SetDIBitsToDevice` copied the client's own
    /// `alpha = 255` there.
    ///
    /// That asymmetry is not folklore in this codebase: it is measured, in
    /// §31.6, by a run of the same window with `--dwm-glass off`, where the
    /// same `FillRect` produces an actual black rectangle because there is no
    /// extended frame for anything to show through.
    ///
    /// Nothing happens for a client-decorated window that did not ask for the
    /// fold, and nothing happens when DWM declines to say where its buttons
    /// are — see `caption_button_rect`.
    /// Make **Windows' caption match the client's title bar**, by sampling the
    /// client's own pixels and telling DWM what colour to paint.
    ///
    /// # Why this direction, and not the one that was asked for
    ///
    /// The obvious statement of the problem is the other way round: tell
    /// *Firefox* what colour Windows' title bar is, so that it draws a tab strip
    /// that matches. There is no way to do that. It is §31.8 for the third time
    /// — Wayland carries sizes and states, not a palette, and GTK 3 takes its
    /// colours from a theme on the client's own machine, which is the far side
    /// of a boundary this compositor cannot reach across. Anything built that
    /// way would be a launcher writing a `gtk.css`, out of band, per client, per
    /// theme, and wrong the moment the user switched to dark mode.
    ///
    /// Reversing it makes the problem disappear, and the reversal is the correct
    /// ownership rather than a trick: **the frame is the compositor's**, the
    /// client's pixels are already in this process's address space, and Windows
    /// 11 has an attribute for precisely this. So the strip DWM paints behind
    /// the caption buttons is set to the colour the client is painting beside
    /// it — sampled, every frame, from the buffer that is about to be blitted —
    /// and it follows the client into dark mode, a different theme or a
    /// different page with no protocol, no configuration and no launcher.
    ///
    /// `DWMWA_TEXT_COLOR` moves with it, chosen by luminance, or the caption
    /// glyphs on a dark title bar are black on black.
    ///
    /// # What is sampled
    ///
    /// Eight pixels immediately to the **left** of the button strip, halfway
    /// down it, averaged. Left of the strip because that is the pixel the
    /// caption is continuing from, and the one a seam would show against;
    /// halfway down because the top row of a GTK header bar is a one-pixel
    /// highlight and the bottom row is a border. Averaged because a single
    /// sample that lands on an anti-aliased glyph edge produces a caption that
    /// is subtly the wrong colour on some frames and not others.
    fn match_caption_colour(&self) {
        if !self.frame.get().extend_top || !self.frame.get().caption_buttons {
            return;
        }
        let Some(rc) = caption_button_rect(self.hwnd()) else {
            return;
        };
        let sampled = {
            let shadow = match self.shadow.lock() {
                Ok(s) => s,
                Err(p) => p.into_inner(),
            };
            let (w, h) = (shadow.width(), shadow.height());
            // **Before the clamps, not after.** A window that has been created
            // and has not yet committed a buffer has a 0x0 shadow, `blit`
            // returns early for it, and this runs anyway — so `clamp(0, h - 1)`
            // is `clamp(0, -1)`, whose min exceeds its max, which is a *panic*
            // rather than a wrong answer. On the UI thread, inside `WM_PAINT`,
            // across an `extern "system"` boundary, that is a non-unwinding
            // panic and the process aborts: the guest leg came back with a
            // truncated log, a compositor that had gone, and a picture of a
            // window that no longer existed. It cost one guest run.
            if w < 8 || h <= 0 {
                return;
            }
            let y = (rc.bottom / 2).clamp(0, h - 1);
            let x0 = (rc.left - 8).clamp(0, w - 8);
            let px = shadow.pixels();
            let (mut b, mut g, mut r) = (0u32, 0u32, 0u32);
            for i in 0..8 {
                let o = (((y * w) + x0 + i) * 4) as usize;
                let Some(p) = px.get(o..o + 4) else { return };
                // BGRA, which is the layout `Shadow` guarantees.
                b += u32::from(p[0]);
                g += u32::from(p[1]);
                r += u32::from(p[2]);
            }
            (r / 8, g / 8, b / 8)
        };
        let (r, g, b) = sampled;
        // COLORREF is 0x00BBGGRR. See the constants' doc comment.
        let colour = (b << 16) | (g << 8) | r;
        if self.caption_colour.replace(Some(colour)) == Some(colour) {
            return;
        }
        // Rec. 601 luma, which is what every "is this background light or dark"
        // decision in a UI toolkit uses and is close enough for a two-way
        // choice between black glyphs and white ones.
        let luma = (299 * r + 587 * g + 114 * b) / 1000;
        let text = if luma >= 140 { 0x0000_0000 } else { 0x00FF_FFFF };
        dwm_set_colour(self.hwnd(), DWMWA_CAPTION_COLOR, colour);
        dwm_set_colour(self.hwnd(), DWMWA_TEXT_COLOR, text);
        // The border too, or a window whose caption is now the client's colour
        // keeps a hairline of the system's around it, which is more visible
        // than the caption was.
        dwm_set_colour(self.hwnd(), DWMWA_BORDER_COLOR, colour);
        println!(
            "wc-compositor: window {} caption colour follows the client: \
             #{r:02x}{g:02x}{b:02x} (luma {luma}, {} glyphs)",
            self.id,
            if luma >= 140 { "dark" } else { "light" }
        );
    }

    /// The caption-button strip in **client** coordinates, and whether it is
    /// DWM's (`true`) or the fallback this window draws itself (`false`).
    ///
    /// ADDED FOR §82. `None` when this window has no buttons: not folded,
    /// `--caption-buttons off`, or a popup.
    ///
    /// The fallback is [`caption::fallback_buttons`]: three `SM_CXSIZE`-wide
    /// buttons at this window's DPI, at the right edge, the folded caption's
    /// height. It exists because Wine's `DwmGetWindowAttribute` is `E_NOTIMPL`
    /// (§67.4), and before it a folded window under Wine had no way to
    /// minimise, maximise or close except the keyboard — and once Firefox is
    /// told to draw no buttons of its own (§82), not even its own.
    fn button_strip(&self) -> Option<(RECT, bool)> {
        let frame = self.frame.get();
        if !frame.extend_top || !frame.caption_buttons || matches!(self.kind, WindowKind::Popup { .. }) {
            return None;
        }
        if let Some(rc) = caption_button_rect(self.hwnd()) {
            return Some((rc, true));
        }
        let mut cr = RECT { left: 0, top: 0, right: 0, bottom: 0 };
        // SAFETY: a live window handle; the rectangle is written in place.
        unsafe { GetClientRect(self.hwnd(), &mut cr) };
        let dpi = window_dpi(self.hwnd());
        let height = match self.nc_top.get() {
            n if n > 0 => n,
            _ => top_inset(self.hwnd()),
        };
        let r = caption::fallback_buttons(
            cr.right - cr.left,
            metric_for_dpi(SM_CXSIZE, dpi),
            height,
        );
        (!r.is_empty()).then_some((
            RECT { left: r.left, top: r.top, right: r.right, bottom: r.bottom },
            false,
        ))
    }

    /// Everything [`caption::hit`] needs, in screen coordinates. §82.
    fn caption_geometry(&self) -> caption::Geometry {
        let hwnd = self.hwnd();
        let mut wr = RECT { left: 0, top: 0, right: 0, bottom: 0 };
        let mut origin = POINT { x: 0, y: 0 };
        // SAFETY: live window handle; both are written in place.
        unsafe {
            GetWindowRect(hwnd, &mut wr);
            ClientToScreen(hwnd, &mut origin);
        }
        caption::Geometry {
            window: caption::Rect { left: wr.left, top: wr.top, right: wr.right, bottom: wr.bottom },
            band: frame_border(hwnd),
            buttons: self.button_strip().map(|(r, _)| caption::Rect {
                left: r.left + origin.x,
                top: r.top + origin.y,
                right: r.right + origin.x,
                bottom: r.bottom + origin.y,
            }),
            maximized: is_maximized(hwnd),
        }
    }

    /// Say, when it changes, whether the client left the button strip empty.
    ///
    /// ADDED FOR §82, and it is §31.14 item 4 — *warn when the levers
    /// disagree* — for the one lever whose failure is visible only in a
    /// photograph. A Firefox started without the launcher's profile draws its
    /// tab-list chevron and close button in the corner Windows is drawing its
    /// own buttons over; nothing in either log said so. The compositor has the
    /// pixels, so it looks. [`caption::corner_is_clear`] is the judgement and
    /// is unit-tested; this is the plumbing and the log line, printed only when
    /// the verdict changes, so a steady client costs one line.
    fn check_corner(&self) {
        let Some((rc, from_dwm)) = self.button_strip() else { return };
        let verdict = {
            let shadow = match self.shadow.lock() {
                Ok(s) => s,
                Err(p) => p.into_inner(),
            };
            caption::corner_is_clear(
                shadow.pixels(),
                shadow.width(),
                shadow.height(),
                shadow.format().has_alpha(),
                caption::Rect { left: rc.left, top: rc.top, right: rc.right, bottom: rc.bottom },
            )
        };
        let Some(clear) = verdict else { return };
        if self.corner_clear.replace(Some(clear)) == Some(clear) {
            return;
        }
        let whose = if from_dwm { "DWM's" } else { "the fallback" };
        if clear {
            println!(
                "wc-compositor: window {} leaves the caption-button corner clear: client rect \
                 {},{} {}x{} ({whose}) has nothing of the client's in it",
                self.id,
                rc.left,
                rc.top,
                rc.right - rc.left,
                rc.bottom - rc.top
            );
        } else {
            println!(
                "wc-compositor: window {} DRAWS UNDER THE CAPTION BUTTONS: client rect {},{} {}x{} \
                 ({whose}) has the client's own widgets in it, which Windows' buttons hide -- \
                 for Firefox, the launcher's profile (titlebar = tabs) is what moves them \
                 (docs/waycompose-design.md §82)",
                self.id,
                rc.left,
                rc.top,
                rc.right - rc.left,
                rc.bottom - rc.top
            );
        }
    }

    fn uncover_caption_buttons(&self, hdc: HDC) {
        let Some((mut rc, from_dwm)) = self.button_strip() else {
            return;
        };
        if !from_dwm {
            self.draw_fallback_buttons(hdc, rc);
            return;
        }
        // Down to the bottom of the strip that was folded in, not to the
        // bottom DWM reported. The two differ by a pixel or two of rounding at
        // 100% and by more at 150%, and the pixels between them are a sliver of
        // the client's own title bar showing under the close button — which
        // reads as a caption button that has been cut off rather than as an
        // off-by-one. The client's tab strip is taller than the system caption
        // either way (§31.8) and that is a different problem.
        rc.bottom = rc.bottom.max(self.nc_top.get());
        // SAFETY: a fresh stock-black brush over a rectangle inside this
        // window's client area, on the DC `BeginPaint` supplied.
        unsafe {
            let brush = CreateSolidBrush(0);
            FillRect(hdc, &rc, brush as HBRUSH);
            DeleteObject(brush as _);
        }
    }

    /// Windows' caption buttons, drawn by Windows, where DWM is not drawing
    /// them. §82.
    ///
    /// `DrawFrameControl(DFC_CAPTION)` is the system's own rendering of a
    /// minimise, maximise (or restore) and close button — the pre-DWM one, so
    /// it looks classic, and it is still the system's rather than ours. It
    /// keeps the rule §31.5 set ("do not draw the buttons — uncover them")
    /// where there is nothing to uncover: the buttons stay Windows', their hit
    /// test is [`caption::hit`]'s, and a click on one goes to `DefWindowProc`
    /// as `HTCLOSE` and comes back as `SC_CLOSE`, exactly as DWM's would.
    ///
    /// Reached on real Windows only if DWM declines to report its bounds,
    /// which no guest run has seen; under Wine it is every frame.
    fn draw_fallback_buttons(&self, hdc: HDC, strip: RECT) {
        let third = (strip.right - strip.left) / 3;
        if third <= 0 {
            return;
        }
        let max = if is_maximized(self.hwnd()) { DFCS_CAPTIONRESTORE } else { DFCS_CAPTIONMAX };
        for (i, kind) in [DFCS_CAPTIONMIN, max, DFCS_CAPTIONCLOSE].into_iter().enumerate() {
            let left = strip.left + third * i as i32;
            let right = if i == 2 { strip.right } else { left + third };
            let mut r = RECT { left, top: strip.top, right, bottom: strip.bottom };
            // SAFETY: the DC `BeginPaint` supplied and a rectangle inside this
            // window's client area, written in place by nothing.
            unsafe {
                DrawFrameControl(hdc, &mut r, DFC_CAPTION, kind);
            }
        }
        if !self.fallback_said.replace(true) {
            println!(
                "wc-compositor: window {} draws Windows' caption buttons itself \
                 (DrawFrameControl): DWM would not say where its own are -- client rect \
                 {},{} {}x{}",
                self.id,
                strip.left,
                strip.top,
                strip.right - strip.left,
                strip.bottom - strip.top
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
                ExcludeClipRect(hdc, 0, 0, w, h);
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
            // §82: where a folded window's client area landed when it was
            // maximised, against the work area it should start at. One line
            // per transition; it is the number the guest run reads to say the
            // tab strip's first row is the monitor's first row.
            if maximized && self.frame.get().extend_top {
                let mut origin = POINT { x: 0, y: 0 };
                // SAFETY: a live window handle; written in place.
                unsafe { ClientToScreen(self.hwnd(), &mut origin) };
                println!(
                    "wc-compositor: window {} maximised: client area starts at screen y={}, \
                     work area at y={}{}",
                    self.id,
                    origin.y,
                    work_area_of(self.hwnd()).map_or(i32::MIN, |r| r.top),
                    if Some(origin.y) == work_area_of(self.hwnd()).map(|r| r.top) {
                        " -- the tab strip's first row is the monitor's"
                    } else {
                        " -- MISMATCH: part of the tab strip is off the monitor or below its top"
                    }
                );
            }
            if !self.self_driven.get() {
                let _ = self.events.send(FromUi::Maximized {
                    id: self.id,
                    on: maximized,
                });
            }
        }

        // The client area **is** the surface, in both frame arrangements: with
        // the caption folded in, `WM_NCCALCSIZE` grew the client rectangle
        // rather than shifting the content inside it. See `FrameStyle`.
        let (w, h) = (client_w.max(0), client_h.max(0));
        if self.placing.get() {
            // §91. See `placing`: the caller decides what the client hears.
            return;
        }
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
        // §89: the moment Windows handed the compositor the input, before the
        // Wayland thread's turn adds its share.
        if sv_transport::frametrace::enabled() {
            super::trace_input("wc.in", &e);
        }
        let _ = self.events.send(e);
        // §89: and the loop handles it now, not at the end of its wait.
        super::wake_wayland();
    }

    /// A client-area point, in the surface's own coordinates.
    ///
    /// The identity, and TABS IN THE TITLE BAR is where that stopped being an accident of
    /// `extend_top` being zero and became a fact about both arrangements: the
    /// caption is folded in by moving the *client rectangle's* top edge, so the
    /// client origin is the surface origin whether or not the fold happened. A
    /// click on the tab strip inside the caption arrives at client `y = 4` and
    /// is surface `y = 4`, which is why nothing on the input path needed a
    /// branch. See [`FrameStyle`] for what stage 3 assumed instead.
    fn to_surface(&self, x: i32, y: i32) -> (f64, f64) {
        (f64::from(x), f64::from(y))
    }

    /// Ask Windows where this window's monitor is, and write the answer where
    /// the Wayland thread can constrain a popup against it.
    ///
    /// ADDED FOR STAGE 38 (docs/waycompose-design.md §38), and it is the first
    /// call in this project that asks Windows where anything is on the desktop.
    ///
    /// # Three choices, each of which can be got wrong silently
    ///
    /// **`MonitorFromWindow`, not the primary monitor.** `GetSystemMetrics`
    /// with `SM_CXSCREEN` is shorter and describes the *primary* display, so on
    /// a two-monitor desktop it would push every menu on the second screen back
    /// towards the first — a bug that is invisible on the machine most people
    /// test on. The monitor that matters is the one the parent window is on,
    /// and `MONITOR_DEFAULTTONEAREST` also answers for a window that has been
    /// dragged half off the desktop, where `MONITOR_DEFAULTTONULL` would give
    /// nothing and this would fall back to not constraining at all.
    ///
    /// **`rcWork`, not `rcMonitor`.** The work area is the monitor minus the
    /// taskbar and any other appbar. A menu constrained to `rcMonitor` at the
    /// bottom of the screen is slid until its last row is behind the taskbar,
    /// which is exactly as unusable as being off the screen and looks
    /// deliberate. This is also the number Windows' own menus respect.
    ///
    /// **The coordinates are converted, not assumed.** `rcWork` is in screen
    /// coordinates and `xdg_positioner` works in the parent's window-geometry
    /// coordinates, so both corners go through `ScreenToClient` and then
    /// through [`WindowState::to_surface`] — the same conversion the pointer
    /// and touch paths use, so a menu cannot be constrained against a rectangle
    /// that disagrees with where the clicks are. Getting this wrong would place
    /// popups plausibly and incorrectly, which is worse than not constraining
    /// them, because it looks right in the common case where the window is near
    /// the top left.
    fn publish_popup_bounds(&self) {
        let hwnd = self.hwnd();
        if hwnd.is_null() {
            return;
        }
        let Some(work) = work_area_of(hwnd) else { return };
        // SAFETY: a live window handle on the thread that owns it; both points
        // are written in place by `ScreenToClient`.
        let (topleft, bottomright) = unsafe {
            let mut tl = POINT { x: work.left, y: work.top };
            let mut br = POINT { x: work.right, y: work.bottom };
            ScreenToClient(hwnd, &mut tl);
            ScreenToClient(hwnd, &mut br);
            (tl, br)
        };
        let (sx, sy) = self.to_surface(topleft.x, topleft.y);
        let (ex, ey) = self.to_surface(bottomright.x, bottomright.y);
        let rect = Rect::new(sx as i32, sy as i32, (ex - sx) as i32, (ey - sy) as i32);
        let mut cell = self.bounds.lock().unwrap_or_else(|e| e.into_inner());
        if *cell != Some(rect) {
            *cell = Some(rect);
            println!(
                "wc-compositor: window {} may put popups in {},{} {}x{} (its own coordinates)",
                self.id, rect.x, rect.y, rect.w, rect.h
            );
        }
    }

    /// `WM_MOUSEMOVE`. Also where the pointer *enters*, because Win32 has no
    /// enter message.
    fn on_mouse_move(&self, lparam: LPARAM, time: u32) {
        let (x, y) = (lo_i16(lparam), hi_i16(lparam));
        let (sx, sy) = self.to_surface(x, y);
        self.last_pos.set((sx, sy));

        // The pointer is over a client area of ours, so a cursor change may be
        // applied immediately rather than waiting for the next mouse message.
        // Set on every move, not only on entry: cheap, and it means a window
        // that never saw its own `WM_MOUSELEAVE` — because it was destroyed
        // under the pointer — cannot leave the flag stuck on for long.
        if let Some(ui) = ui() {
            ui.pointer_inside.set(true);
        }

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
        if let Some(ui) = ui() {
            ui.pointer_inside.set(false);
        }
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
        //
        // §96: except under a lock, where the pointer has not moved and the
        // click happens where the client already believes it is.
        if !ui().is_some_and(|ui| hold::locked(&ui, self.id)) {
            self.send(FromUi::PointerMotion { id: self.id, x: sx, y: sy, time });
        }

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

pub(crate) struct Ui {
    windows: RefCell<HashMap<WindowId, Rc<WindowState>>>,
    commands: Commands,
    pub(crate) events: Sender<FromUi>,
    frame: FrameStyle,
    /// Guards against `WM_WC_COMMAND` arriving while an earlier drain is still
    /// running. It should not be possible — the calls a drain makes send
    /// messages rather than pumping them — but "should not be possible" is not
    /// a thing to find out about from a corrupted window list.
    draining: Cell<bool>,
    /// How many windows this process has ever made, and how many it has
    /// destroyed. §54.
    ///
    /// Every gate in this tree asserts that something **appeared**. Nothing
    /// asserted that anything stopped existing, so a leaked `HWND` per client
    /// survived a whole session with every check green — the same shape of hole
    /// the WLCS side found on protocol errors, in the platform layer.
    ///
    /// Counted here rather than derived from `windows.len()` because the map is
    /// the thing under suspicion: a `Destroy` that removed the entry and failed
    /// to destroy the window would leave the map empty and the `HWND` alive,
    /// which is exactly the leak worth catching and exactly the one a
    /// map-length check cannot see. These two numbers are the *claim*;
    /// `live_toplevels` below is the platform's own answer, and the pair
    /// disagreeing is the finding.
    made: Cell<u64>,
    gone: Cell<u64>,
    pub(crate) controller: Cell<isize>,
    /// Whether a client currently holds an idle inhibitor.
    ///
    /// ADDED FOR STAGE 21. Kept here as well as inside Windows' own execution
    /// state because `SetThreadExecutionState` is write-only — it returns the
    /// *previous* state, and there is no "what is it now" query — and because
    /// `toplevel_proc` has to read it to answer `WM_SYSCOMMAND`.
    idle_inhibited: Cell<bool>,
    /// `RegisterClipboardFormatW("HTML Format")`, resolved once.
    ///
    /// A **registered** format has no compile-time number: the id is minted per
    /// session and two processes get the same one only because they registered
    /// the same string. 0 means "not resolved yet"; the registration cannot
    /// fail in any way worth a second code path — if it does, `html_format`
    /// returns 0 and no HTML crosses, which is the same outcome as a clipboard
    /// with no HTML on it.
    html_format: Cell<u32>,
    /// §93. `RegisterClipboardFormatW("PNG")`, resolved once, like `html_format`.
    png_format: Cell<u32>,
    /// Where to write a PPM of every presented frame, if asked.
    ///
    /// ADDED FOR STAGE 4. Until then this was headless-only, and that asymmetry
    /// is exactly what docs/waycompose-design.md §8.5 had to apologise for: the half of
    /// the evidence that produced an `HWND` was also the half whose pixels
    /// nobody outside the process ever measured. A frame dumped here is read
    /// back by `identify` and `compare`, which are not ours.
    dump_frames: Option<std::path::PathBuf>,

    // --- the cursor, stage 12 ---------------------------------------------
    //
    // Three cells rather than one, because the `HCURSOR` a window shows, the
    // `HCURSOR` this process must destroy, and whether the pointer is
    // anywhere it matters are three independent facts (docs/waycompose-design.md §23).
    /// What `WM_SETCURSOR` should install over the client area. `0` means
    /// "hidden", which is a value `SetCursor` accepts and is what
    /// `wl_pointer.set_cursor` with a null surface asks for; it is therefore
    /// **not** a "nothing set yet" sentinel — that state is the arrow, put
    /// there at startup.
    cursor: Cell<isize>,
    /// The `HCURSOR` we built with `CreateIconIndirect`, if the current cursor
    /// is one. Stock cursors from `LoadCursorW` are shared, must not be
    /// destroyed, and are therefore never recorded here.
    ///
    /// A client that animates its cursor sends a new buffer several times a
    /// second, so leaking these is not a slow leak: `weston-terminal`'s
    /// watch cursor would burn a GDI object every 100 ms and hit the 10,000
    /// handle quota in a quarter of an hour.
    owned_cursor: Cell<isize>,
    /// Whether the pointer is inside the client area of one of our windows.
    ///
    /// **Why this is needed at all.** `WM_SETCURSOR` arrives only when a mouse
    /// message does, so a client that changes its cursor while the pointer is
    /// still — a spinner starting, a `wl_pointer.leave` putting the arrow back
    /// — would not be seen until the user moved. So the command handler calls
    /// `SetCursor` itself. But `SetCursor` is thread-global: doing it while
    /// the pointer is over somebody else's window would repaint *their*
    /// cursor until their next mouse message. This is the guard.
    ///
    /// **Touch deliberately never sets it**, and that is the one place stages
    /// 12 and 20 meet. This flag means "the mouse cursor is over our client
    /// area", not "somebody is pointing at our window": a finger does not move
    /// the cursor, and `on_pointer` suppresses the mouse messages Windows would
    /// otherwise synthesise from a contact, so a tap leaves the real cursor
    /// wherever it was. Setting this on a touch would let a client's cursor
    /// change repaint the cursor while it is sitting over somebody else's
    /// window — exactly the failure this guard exists to prevent, reached from
    /// the one direction that has no cursor in it at all.
    pointer_inside: Cell<bool>,

    /// ADDED FOR STAGE 20. Windows pointer ids to `wl_touch` contact ids.
    ///
    /// **On the pump and not on the window**, which is the one structural
    /// decision in the touch path. `wl_touch`'s ids namespace the *seat*, and
    /// there is one seat; a per-window table would let two windows each hand
    /// out contact 0 and the client would see one finger in two places. Win32
    /// has no seat, so the nearest thing that exists — the thread that pumps
    /// every window — owns it. See `crate::input::touch::Slots`.
    slots: RefCell<crate::input::touch::Slots>,
    /// §96. What the pointer is held to, if anything. One per process: there is
    /// one system pointer. See `win32_hold.rs`.
    hold: hold::HoldState,
}

thread_local! {
    static UI: RefCell<Option<Rc<Ui>>> = const { RefCell::new(None) };
    /// §102: the place of the last named toplevel that closed, for a window of
    /// the same identity that maps soon after (`super::Succession`).
    static SUCCESSION: RefCell<Option<super::Succession>> = const { RefCell::new(None) };
}

pub(crate) fn ui() -> Option<Rc<Ui>> {
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

/// The same clock, for the drop target.
///
/// ADDED FOR §42. `IDropTarget`'s methods are COM calls rather than window
/// messages, so there is no message in the queue for `GetMessageTime` to
/// describe — but OLE's modal loop is pumping messages the whole time, so the
/// value is the time of whatever it last dispatched, which is within a mouse
/// move of now and is on the same timeline as every other input event. That
/// last part is what matters: a drag whose timestamps came from a different
/// clock than the pointer's would give a client a gesture that appears to run
/// backwards.
pub(crate) fn now_ms() -> u32 {
    message_time()
}

/// A screen point, in the surface coordinates of one of our windows.
///
/// ADDED FOR §42, and it goes through [`WindowState::to_surface`] — the
/// conversion the pointer, the touch and §38's popup bounds all share. During
/// an OLE drag there is no `WM_MOUSEMOVE`, so this is the *only* thing placing
/// the drag inside the window, and a second conversion here would be a drop
/// that lands somewhere the clicks do not.
pub(crate) fn screen_to_surface(ui: &Rc<Ui>, id: WindowId, x: i32, y: i32) -> (f64, f64) {
    let Some(w) = window(ui, id) else {
        return (f64::from(x), f64::from(y));
    };
    let mut pt = POINT { x, y };
    // SAFETY: a live window handle on its own thread; `ScreenToClient` converts
    // in place.
    unsafe {
        ScreenToClient(w.hwnd(), &mut pt);
    }
    w.to_surface(pt.x, pt.y)
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

    // ADDED FOR TABS IN THE TITLE BAR, and ahead of the `match` because both arms fall back
    // to `DefWindowProc`'s own answer rather than replacing it, and an arm that
    // has to say "otherwise do what you would have done" is clearer as an
    // `Option` than as a duplicated tail. Neither does anything at all unless
    // `--decorations tabs` asked for it.
    if msg == WM_NCCALCSIZE && let Some(r) = nccalcsize(state, hwnd, wparam, lparam) {
        return r;
    }
    if msg == WM_NCHITTEST && let Some(r) = nchittest(state, hwnd, wparam, lparam) {
        return r;
    }
    // The caption buttons are DWM's, so their *behaviour* is DWM's too. These
    // five messages carry the hover highlight, the pressed state, the click and
    // the caption's right-click menu, and `DwmDefWindowProc` is the only thing
    // that can act on them — the buttons are pixels this process never drew and
    // whose rectangles it learns by asking (`caption_button_rect`). A window
    // that hit-tests them and does not forward these gets buttons that respond
    // to nothing, which is worse than not having them.
    if state.frame.get().extend_top
        && state.frame.get().caption_buttons
        && matches!(
            msg,
            WM_NCMOUSEMOVE | WM_NCMOUSELEAVE | WM_NCLBUTTONDOWN | WM_NCLBUTTONUP | WM_NCRBUTTONUP
        )
        && let Some(r) = dwm_defwindowproc(hwnd, msg, wparam, lparam)
    {
        return r;
    }
    // §91, the caption buttons that vanished (§90.6). `DefWindowProc` answers
    // `WM_SETTEXT`, `WM_SETICON` and `WM_NCACTIVATE` by painting the caption it
    // thinks this window has — synchronously, outside `WM_PAINT`, into the band
    // `WM_NCCALCSIZE` gave to the client and `uncover_caption_buttons` made
    // transparent for DWM's buttons. The strip turns opaque, the buttons behind
    // it disappear and still work, and they come back only when a later
    // `WM_PAINT` happens to cover the strip. Every page §90 lost them on retitles
    // itself several times a second (an fps counter, a video clock), and the
    // compositor forwards each title with `SetWindowTextW`.
    if state.frame.get().extend_top && !matches!(state.kind, WindowKind::Popup { .. }) {
        match msg {
            WM_SETTEXT | WM_SETICON => {
                let r = defproc_without_caption_paint(hwnd, msg, wparam, lparam);
                state.invalidate_button_strip();
                return r;
            }
            WM_NCACTIVATE => {
                // -1: "do not repaint the non-client area" (WPF's WindowChrome
                // does exactly this, for exactly this frame).
                // SAFETY: a live window handle on its own thread.
                let r = unsafe { DefWindowProcW(hwnd, msg, wparam, -1) };
                state.invalidate_button_strip();
                return r;
            }
            // §95. The buttons still went, with neither a title nor an
            // activation change: after a page load, the strip held one flat
            // colour, Firefox's own tab-strip pixel and not DWM's caption
            // colour, so something outside `WM_PAINT` had painted the caption
            // background over it. `WM_NCPAINT` is the general form of what
            // §91 handled for three messages: the default handler draws the
            // caption it believes in, and it does not stay out of the band
            // that `WM_NCCALCSIZE` gave to the client. The frame still needs
            // the default handler, so the strip is re-cleared after it.
            WM_NCPAINT => {
                // SAFETY: a live window handle on its own thread.
                let r = unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) };
                state.invalidate_button_strip();
                return r;
            }
            _ => {}
        }
    }

    match msg {
        // §91: the client's `set_min_size`/`set_max_size`, as the frame's
        // track sizes. `DefWindowProc` applies these to every user resize and
        // to every `SetWindowPos` through `WM_WINDOWPOSCHANGING`.
        WM_GETMINMAXINFO if !matches!(state.kind, WindowKind::Popup { .. }) => {
            let r = unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) };
            // SAFETY: `lparam` is the MINMAXINFO Windows passes with this message.
            let mmi = unsafe { &mut *(lparam as *mut MINMAXINFO) };
            let ((minw, minh), (maxw, maxh)) = state.size_hints.get();
            if minw > 0 || minh > 0 {
                let (ow, oh) = outer_size(state, minw.max(1), minh.max(1));
                if minw > 0 { mmi.ptMinTrackSize.x = mmi.ptMinTrackSize.x.max(ow); }
                if minh > 0 { mmi.ptMinTrackSize.y = mmi.ptMinTrackSize.y.max(oh); }
            }
            if maxw > 0 || maxh > 0 {
                let (ow, oh) = outer_size(state, maxw.max(1), maxh.max(1));
                if maxw > 0 { mmi.ptMaxTrackSize.x = ow; }
                if maxh > 0 { mmi.ptMaxTrackSize.y = oh; }
            }
            r
        }
        WM_ENTERSIZEMOVE => {
            state.user_sizing.set(true);
            unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
        }
        WM_EXITSIZEMOVE => {
            state.user_sizing.set(false);
            unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
        }
        WM_PAINT => {
            state.paint();
            0
        }
        // The frame's thickness is a per-monitor quantity, so a window dragged
        // between a 100% and a 150% display has a different caption to fold in
        // than the one it was created with. Re-asking is one call and the
        // alternative is a title bar that is the other monitor's height until
        // something else happens to invalidate the frame.
        WM_DPICHANGED => {
            apply_frame(state);
            unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
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
        // ADDED FOR STAGE 38. The three ways the answer to "where may this
        // window's popups go" changes, and none of them is a message this
        // compositor already handled:
        //
        //   `WM_WINDOWPOSCHANGED`  the window moved, possibly onto another
        //                          monitor. Sent for every step of a drag, so
        //                          the cache is at most one message stale.
        //   `WM_DISPLAYCHANGE`     the resolution or the monitor layout
        //                          changed under it.
        //   `WM_SETTINGCHANGE`     the taskbar moved, was resized, or was set
        //                          to auto-hide, which changes `rcWork` while
        //                          nothing moves at all.
        //
        // All three fall through to `DefWindowProc` rather than returning 0:
        // `WM_WINDOWPOSCHANGED`'s default handler is what generates `WM_SIZE`
        // and `WM_MOVE`, and swallowing it would silently take the resize
        // negotiation with it.
        WM_WINDOWPOSCHANGED | WM_DISPLAYCHANGE | WM_SETTINGCHANGE => {
            state.publish_popup_bounds();
            // §96: a clip is in screen pixels and follows the window.
            if let Some(ui) = ui() {
                hold::on_moved(&ui, state);
            }
            if msg == WM_WINDOWPOSCHANGED
                && let Some(ui) = ui()
            {
                follow_owner(&ui, state.id, hwnd);
            }
            unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
        }
        // §93. A click on a menu must not take the focus. `WS_EX_NOACTIVATE`
        // keeps a popup from becoming the *foreground* window, and that is
        // all its documentation promises: Wine gave the clicked popup
        // `WM_SETFOCUS` (and took it from the toplevel), the client heard
        // `wl_keyboard.leave` on its window, and Firefox rolls its menus up
        // on focus-out, so clicking "Help" in its menu closed the menu
        // instead of opening the Help panel. `MA_NOACTIVATE` is the answer
        // menus and tooltips give: the click is delivered, and neither
        // activation nor focus moves.
        WM_MOUSEACTIVATE if matches!(state.kind, WindowKind::Popup { .. }) => {
            MA_NOACTIVATE as LRESULT
        }
        WM_ACTIVATE => {
            let on = (wparam & 0xffff) as u32 != WA_INACTIVE;
            // §96: losing the foreground lets go of a held pointer here and now.
            if !on
                && let Some(ui) = ui()
            {
                hold::on_focus(&ui, state, false);
            }
            let _ = state.events.send(FromUi::Activated { id: state.id, on });
            0
        }
        // ADDED FOR STAGE 17. `xdg_toplevel.show_window_menu`, arriving as a
        // posted message so that the modal `TrackPopupMenu` is entered from the
        // pump rather than from inside `drain`.
        //
        // **The system menu, not an imitation of it.** `GetSystemMenu` returns
        // the very menu the caption's right-click and Alt+Space put up, with
        // Windows' own strings, its own accelerators and its own greying — a
        // maximised window's *Maximize* is disabled and a fixed-size window has
        // no *Size*, and none of that has to be reimplemented. `TPM_RETURNCMD`
        // asks for the chosen item back instead of a `WM_COMMAND`, and it is
        // then posted as the `WM_SYSCOMMAND` the caption would have produced,
        // so *Close* on this menu is the same code path as the close button.
        WM_WC_SYSMENU => {
            let x = (lparam & 0xffff) as i16 as i32;
            let y = ((lparam >> 16) & 0xffff) as i16 as i32;
            // SAFETY: live window handle on its own thread. `GetSystemMenu`
            // with `revert = 0` returns a handle owned by the window, which
            // must not be destroyed here. `TrackPopupMenu` runs a modal loop
            // and returns the command id because of `TPM_RETURNCMD`.
            let chosen = unsafe {
                let menu = GetSystemMenu(hwnd, 0);
                if menu.is_null() {
                    0
                } else {
                    TrackPopupMenu(
                        menu,
                        TPM_RETURNCMD | TPM_RIGHTBUTTON,
                        x,
                        y,
                        0,
                        hwnd,
                        std::ptr::null(),
                    )
                }
            };
            if chosen != 0 {
                // SAFETY: live handle; posted rather than sent for the same
                // reason the gesture is — `SC_MOVE` and `SC_SIZE` are among the
                // things this menu can choose.
                unsafe {
                    PostMessageW(hwnd, WM_SYSCOMMAND, chosen as usize, 0);
                }
            }
            println!(
                "wc-compositor: window {} system menu at {x},{y} chose 0x{chosen:04x}",
                state.id
            );
            0
        }

        // ADDED FOR STAGE 21: the second half of `zwp_idle_inhibit`.
        //
        // `SetThreadExecutionState(ES_DISPLAY_REQUIRED)` resets the display
        // idle timer and does not touch the screen saver, which runs on the
        // *input* idle timer — and a client holding an idle inhibitor is
        // usually playing a video, which is exactly the situation with no
        // input. So the execution state alone leaves the screen saver free to
        // start. Refusing the `WM_SYSCOMMAND` is the documented way for the
        // foreground application to veto it, and `DefWindowProc` is what would
        // otherwise let it through.
        //
        // Only while something actually holds an inhibitor: a compositor that
        // permanently blocked the screen saver would be making a policy
        // decision on the user's behalf that no client asked for.
        WM_SYSCOMMAND
            if matches!(wparam & SC_MASK, x if x == SC_SCREENSAVE as usize
                || x == SC_MONITORPOWER as usize)
                && ui().is_some_and(|ui| ui.idle_inhibited.get()) =>
        {
            println!(
                "wc-compositor: refused a screen-blank request; a client holds an idle inhibitor"
            );
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
            state.send(FromUi::KeyboardFocus { id: state.id, on: true, caps: lock_on(VK_CAPITAL), num: lock_on(VK_NUMLOCK) });
            // §96: a hold the window had before it lost the keyboard comes back.
            if let Some(ui) = ui() {
                hold::on_focus(&ui, state, true);
            }
            0
        }
        WM_KILLFOCUS => {
            // §96: first, so the pointer is free before anything else runs.
            if let Some(ui) = ui() {
                hold::on_focus(&ui, state, false);
            }
            state.send(FromUi::KeyboardFocus { id: state.id, on: false, caps: false, num: false });
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
            // §96: a locked pointer does not move; its motion is `WM_INPUT`.
            if ui().is_some_and(|ui| hold::locked(&ui, state.id)) {
                return 0;
            }
            state.on_mouse_move(lparam, message_time());
            0
        }
        // §96. Raw mouse motion, registered only while a lock is held. Passed
        // on to `DefWindowProc` afterwards, which the documentation requires
        // for the system to release the input.
        WM_INPUT => {
            if let Some(ui) = ui() {
                hold::on_input(&ui, state, lparam);
            }
            unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
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
        // ADDED FOR STAGE 12 (docs/waycompose-design.md §23). **The only place a client's
        // cursor is installed on a mouse message**, and the reason it has to be
        // handled rather than left to `DefWindowProc` is that `DefWindowProc`
        // sets the *class* cursor — the `IDC_ARROW` in `register_classes` —
        // over the client area, on every mouse move. Anything the compositor
        // set would be overwritten a few milliseconds later, which presents as
        // a cursor that flickers to the right shape and back.
        //
        // **The hit-test guard is not a nicety.** `wParam` is the window and
        // `LOWORD(lParam)` is the hit-test code, and only `HTCLIENT` is the
        // client's business: `HTLEFT`, `HTBOTTOMRIGHT`, `HTCAPTION` and the
        // rest are the system frame, where Windows draws the resize arrows and
        // the arrow over the title bar. Claiming those would replace the resize
        // cursors on the window border with whatever the client last asked for
        // — the opposite of the goal, which wants a resize arrow on the edge.
        //
        // Returning `TRUE` means "the cursor is set, stop here"; falling
        // through to `DefWindowProc` lets the frame keep its own.
        WM_SETCURSOR => {
            if (lparam as u32) & 0xffff == HTCLIENT
                && let Some(ui) = ui()
            {
                // SAFETY: either 0 — which `SetCursor` documents as "remove
                // the cursor from the screen" — or a live `HCURSOR` this
                // thread owns or borrowed from the system.
                unsafe {
                    SetCursor(ui.cursor.get() as _);
                }
                return 1;
            }
            unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
        }
        WM_MOUSEWHEEL => {
            state.on_wheel(wparam, false, message_time());
            0
        }
        WM_MOUSEHWHEEL => {
            state.on_wheel(wparam, true, message_time());
            0
        }

        // --- touch, stage 20 -----------------------------------------------
        //
        // Deliberately falling through to `DefWindowProc` when `on_pointer`
        // declines: that is what turns a pen — or a touch whose details have
        // already been discarded — back into the mouse messages above, rather
        // than into nothing. See `on_pointer` for why consuming the message is
        // the whole of the opt-in, and why not consuming it would deliver every
        // tap twice.
        WM_POINTERDOWN | WM_POINTERUPDATE | WM_POINTERUP | WM_POINTERENTER | WM_POINTERLEAVE
        | WM_POINTERCAPTURECHANGED => {
            if on_pointer(state, msg, wparam) {
                0
            } else {
                unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
            }
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
            // ADDED FOR STAGE 20. A window that goes away holding contacts
            // would leak their ids for the life of the process — Windows sends
            // no `WM_POINTERUP` to a window that no longer exists — and would
            // leave the client believing fingers are still down on a surface it
            // has destroyed. Both are fixed by the event the protocol already
            // has for "these did not finish".
            //
            // Seat-wide, so it also ends contacts on *other* windows, which is
            // the same asymmetry `on_pointer` accepts for a cancelled contact
            // and for the same reason: one finger is on one window in practice,
            // and a lost gesture beats a stuck one.
            if let Some(ui) = ui() {
                cancel_all(state, &ui, "the window was destroyed");
            }
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

// ---------------------------------------------------------------------------
// Touch
// ---------------------------------------------------------------------------

/// Whether this machine has a touch digitiser.
///
/// **Read at startup so that `wl_seat` does not claim a capability the hardware
/// cannot produce.** `SM_MAXIMUMTOUCHES` is the number of simultaneous contacts
/// the installed digitisers support and is 0 when there is none — the same
/// question `GetSystemMetrics(SM_DIGITIZER)` answers with flags, asked in the
/// form that has a useful number in it. A laptop without a touchscreen
/// therefore advertises pointer and keyboard and not touch, which is what the
/// protocol means by a capability and is why this is not simply hard-coded
/// true.
///
/// `WC_FORCE_TOUCH` overrides it, and exists for exactly one purpose: driving
/// the path with `InjectSyntheticPointerInput` on a machine with no digitiser.
/// It is an environment variable rather than a flag so that a run which used it
/// is visible in the process environment of the thing under test, and the
/// startup line says so — a synthetic run must not be mistakable for a hardware
/// one (docs/waycompose-design.md §24.6).
pub fn has_touch() -> bool {
    if std::env::var_os("WC_FORCE_TOUCH").is_some() {
        println!(
            "wc-compositor: WC_FORCE_TOUCH is set, so wl_seat will advertise touch whatever the \
             hardware says. Any touch event in this run is synthetic."
        );
        return true;
    }
    // SAFETY: a pure query taking a documented index.
    let n = unsafe { GetSystemMetrics(SM_MAXIMUMTOUCHES) };
    if n <= 0 {
        println!(
            "wc-compositor: SM_MAXIMUMTOUCHES is {n}, so this machine has no touch digitiser and \
             wl_seat will not advertise touch."
        );
        return false;
    }
    println!("wc-compositor: touch digitiser reports {n} simultaneous contacts");
    true
}

/// One `WM_POINTER*` message. Returns whether it was consumed.
///
/// # Why `WM_POINTER*` and not `RegisterTouchWindow`/`WM_TOUCH`
///
/// Both are real, both work, and a window must opt into one of them or Windows
/// synthesises **mouse messages** from the primary contact instead — which is
/// what this compositor was doing before this stage, and is worth stating
/// because it means touch was never *absent* here, it was arriving as a mouse.
/// A tap became `WM_LBUTTONDOWN`, Firefox saw `wl_pointer.button`, and it
/// mostly worked; what it could not do was two fingers, or tell a client that
/// the input was a finger at all.
///
/// The pointer family is chosen for one reason that decides it and two that
/// support it:
///
/// * **`WM_TOUCH` cannot express `wl_touch.orientation` at all.** `TOUCHINPUT`
///   carries `cxContact`/`cyContact` — a contact *size* — and no angle
///   whatsoever. `wl_seat` is advertised at 9, so `shape` and `orientation`
///   are both in scope, and choosing `WM_TOUCH` would mean one of the two
///   events could never be sent by construction rather than for want of
///   hardware. `POINTER_TOUCH_INFO` has `rcContact` *and* `orientation`, each
///   with a bit in `touchMask` saying whether the digitiser filled it in,
///   which is exactly the conditionality the protocol wants.
/// * It is the supported family. `WM_TOUCH` is documented as legacy and
///   `RegisterTouchWindow` switches the window *away* from pointer messages,
///   so taking it would also forfeit pen and every later input type.
/// * `InjectSyntheticPointerInput` drives it, which is the only way this path
///   can be exercised at all on a machine with no digitiser.
///
/// The cost is Windows 8, and this compositor already requires far newer.
///
/// # Consuming the message is what suppresses the mouse
///
/// Returning 0 rather than falling through to `DefWindowProc` is not a
/// formality: the default handling of `WM_POINTERDOWN`/`UPDATE`/`UP` is what
/// *generates* the legacy `WM_LBUTTONDOWN`/`WM_MOUSEMOVE`/`WM_LBUTTONUP`. Left
/// in place, every single-finger tap would reach the client twice — once as
/// `wl_touch` and once as `wl_pointer` — and a client that handles both, which
/// Firefox does, would act on it twice.
///
/// So the suppression is deliberate and it is also *narrow*: anything that is
/// not `PT_TOUCH` is handed straight to `DefWindowProc`. A pen still becomes
/// mouse messages and still works exactly as it did, because a `wl_tablet_tool`
/// this compositor does not implement is worse than a mouse it does.
///
/// # Focus
///
/// `SetFocus` is called explicitly on the first contact, because the
/// `DefWindowProc` handling that would have done it is the same handling being
/// suppressed. Without it, tapping a window would move no keyboard focus and
/// the client could not be typed into after being touched — a failure that
/// looks like a keyboard bug and is a touch one.
fn on_pointer(state: &WindowState, msg: u32, wparam: WPARAM) -> bool {
    // `GET_POINTERID_WPARAM` is the low word, on every message in the family.
    let pointer_id = (wparam & 0xffff) as u32;

    let mut kind: POINTER_INPUT_TYPE = 0;
    // SAFETY: `kind` is written by the call; a failure leaves it alone, which
    // is why it is initialised to a value that is not `PT_TOUCH`.
    if unsafe { GetPointerType(pointer_id, &mut kind) } == 0 || kind != PT_TOUCH {
        // A pen, or a pointer that has already gone away. Let Windows make
        // mouse messages out of it; the existing path handles those.
        return false;
    }

    let Some(ui) = ui() else { return false };

    // Capture taken away by the system — an edge swipe, the touch keyboard,
    // a shell gesture. Every live contact is finished at once, which is what
    // `wl_touch.cancel` means, and no per-contact information is needed.
    if msg == WM_POINTERCAPTURECHANGED {
        cancel_all(state, &ui, "the system took the pointer capture");
        return true;
    }

    // A touch contact has no hover, so these carry nothing this compositor
    // wants. They are still consumed, and there are two reasons rather than
    // one. Handed to `DefWindowProc` they would produce the synthesised mouse
    // enter and leave that the rest of this function exists to prevent, so
    // `wl_pointer` would report a pointer wandering in and out of the window
    // every time somebody tapped it — and, since stage 12, that synthesised
    // `WM_MOUSEMOVE` would also set `Ui::pointer_inside`, which is the guard on
    // a **thread-global** `SetCursor`. A finger would then license the
    // compositor to repaint the cursor while the real one is over somebody
    // else's window. See the field's own note.
    if msg == WM_POINTERENTER || msg == WM_POINTERLEAVE {
        return true;
    }

    let mut ti: POINTER_TOUCH_INFO = unsafe { std::mem::zeroed() };
    // SAFETY: `ti` is fully owned here and written by the call.
    if unsafe { GetPointerTouchInfo(pointer_id, &mut ti) } == 0 {
        // The type said touch and the details are gone, which means the contact
        // ended between the two calls. Not consumed: a mouse message is a
        // better outcome than a dropped one.
        return false;
    }

    // **Screen coordinates.** `ptPixelLocation` is on the desktop, unlike every
    // other input message in this file, whose `lParam` is already client-local.
    // Converting here and then through `to_surface` is what keeps a touch and a
    // click landing on the same pixel of the same surface.
    let mut pt = POINT {
        x: ti.pointerInfo.ptPixelLocation.x,
        y: ti.pointerInfo.ptPixelLocation.y,
    };
    // SAFETY: a live window handle and a stack point written by the call.
    unsafe {
        ScreenToClient(state.hwnd(), &mut pt);
    }
    let (sx, sy) = state.to_surface(pt.x, pt.y);

    // `message_time()` and **not** `ti.pointerInfo.dwTime`, which is the
    // digitiser's own stamp. Wayland says only that timestamps within a seat
    // are comparable, and mixing two epochs on one seat would make a
    // touch-then-click sequence appear to go backwards in time — which is
    // precisely what a client's double-tap and gesture timing reads.
    let time = message_time();

    // The contact patch, only if the digitiser said it has one. Both halves are
    // conditional and neither is invented; see `shape_from_contact`.
    let shape = if ti.touchMask & TOUCH_MASK_CONTACTAREA != 0 {
        let r = ti.rcContact;
        let orientation = if ti.touchMask & TOUCH_MASK_ORIENTATION != 0 {
            Some(f64::from(ti.orientation))
        } else {
            None
        };
        crate::input::touch::shape_from_contact(
            f64::from(r.right - r.left),
            f64::from(r.bottom - r.top),
            orientation,
        )
    } else {
        None
    };

    // A contact Windows has marked as not meant — palm rejection is the usual
    // one. There is no per-contact cancel in the protocol: `wl_touch.cancel`
    // ends the whole stream. Cancelling everything is the deliberate choice,
    // because the two ways to be wrong are not symmetric — over-cancelling
    // loses a gesture the user must repeat, while delivering it as a normal
    // `up` completes a tap the user never made, on whatever control the palm
    // was resting on.
    if ti.pointerInfo.pointerFlags & POINTER_FLAG_CANCELED != 0 {
        cancel_all(state, &ui, "Windows marked the contact cancelled");
        return true;
    }

    match msg {
        WM_POINTERDOWN => {
            let slot = ui.slots.borrow_mut().down(pointer_id);
            let Some(slot) = slot else {
                eprintln!(
                    "wc-compositor: window {} dropped touch contact {pointer_id}: more than {} \
                     down at once",
                    state.id,
                    crate::input::touch::MAX_CONTACTS
                );
                return true;
            };
            // The two numberings, side by side, once per contact. Cheap — a
            // contact is a few messages, not one per pixel — and it is the only
            // place the mapping is observable from outside: Windows' id is the
            // input stack's own and the client is told a small dense one, and
            // on the guest they were 4 and 5 against 0 and 1 for the same two
            // fingers. A reader who suspects the ids are wrong can
            // check them here rather than infer it from a client behaving oddly.
            println!(
                "wc-compositor: window {} touch contact: Windows pointer {pointer_id} \
                 -> wl_touch id {slot}",
                state.id
            );
            // See this function's header: the `DefWindowProc` that would have
            // moved the focus is the one being suppressed.
            // SAFETY: both are pure queries on a live handle owned by this
            // thread; `SetFocus` on the already-focused window is a no-op.
            unsafe {
                if GetFocus() != state.hwnd() {
                    SetFocus(state.hwnd());
                }
            }
            state.send(FromUi::TouchDown { id: state.id, slot, x: sx, y: sy, shape, time });
        }
        WM_POINTERUPDATE => {
            // A contact that is in range but not touching. Touch digitisers
            // rarely do this and pens always do; the flag is checked rather
            // than assumed, because a hovering contact reported as motion is a
            // finger that a client believes is dragging.
            if ti.pointerInfo.pointerFlags & POINTER_FLAG_INCONTACT == 0 {
                return true;
            }
            let Some(slot) = ui.slots.borrow().get(pointer_id) else {
                // An update for a contact whose down went somewhere else. Not
                // an error and not deliverable: `wl_touch.motion` may only name
                // an id that a `down` has already introduced.
                return true;
            };
            state.send(FromUi::TouchMotion { id: state.id, slot, x: sx, y: sy, shape, time });
        }
        WM_POINTERUP => {
            let Some(slot) = ui.slots.borrow_mut().up(pointer_id) else {
                return true;
            };
            state.send(FromUi::TouchUp { id: state.id, slot, time });
        }
        _ => return false,
    }

    // **One frame per message, and every message closes its own.**
    //
    // Win32 delivers one `WM_POINTER*` per contact; there is no message that
    // carries two. Windows does offer `GetPointerFrameTouchInfo` plus
    // `SkipPointerFrameMessages`, which would gather every contact sharing a
    // `frameId` into one frame — and it is not used, because `Skip…` discards
    // the queued messages for the whole frame including `WM_POINTERDOWN`s whose
    // implicit capture and whose focus handling this function depends on, and
    // there is no digitiser here to establish that doing so is safe.
    //
    // Nothing is lost by the simpler shape. The protocol says a frame
    // "terminates at least one event but otherwise no guarantee is provided
    // about the set of events within a frame", and a client "must assume that
    // any state not updated in a frame is unchanged" — so a frame per changed
    // contact is exactly what libinput and Weston produce for the same
    // gesture, one evdev `SYN_REPORT` at a time. A pinch arrives as alternating
    // single-contact frames with equal timestamps, which is a correct
    // description of it.
    state.send(FromUi::TouchFrame);
    true
}

/// End every live contact at once, if there is one.
///
/// Guarded on there being something down, because `wl_touch.cancel` with no
/// contact is a client being told to finalise touch points it does not have.
fn cancel_all(state: &WindowState, ui: &Rc<Ui>, why: &str) {
    if !ui.slots.borrow_mut().clear() {
        return;
    }
    println!("wc-compositor: window {} touch cancelled: {why}", state.id);
    state.send(FromUi::TouchCancel);
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
    if msg == WM_CLIPBOARDUPDATE {
        if let Some(ui) = ui() {
            on_clipboard_update(&ui, "WM_CLIPBOARDUPDATE");
        }
        return 0;
    }
    // The OLE drag loop, entered **here** rather than from `drain`. See
    // `super::drag`'s header and stage 17's identical treatment of `SC_MOVE`:
    // `drain` holds `ui.draining`, and a modal loop under it spins the pump
    // re-posting a wake it cannot service. By the time this arrives the drain
    // has returned.
    if msg == super::drag::WM_WC_DRAG {
        if let Some(ui) = ui()
            && let Some(action) = super::drag::enter_modal_loop()
        {
            let _ = ui.events.send(FromUi::DragFinished { action });
        }
        return 0;
    }
    unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
}

// ---------------------------------------------------------------------------
// The clipboard
// ---------------------------------------------------------------------------

/// The clipboard changed. Tell the Wayland side what is on it now.
///
/// # This fires for our own writes too, and that is kept deliberately
///
/// `SetClipboard` below posts a `WM_CLIPBOARDUPDATE` straight back at us, so a
/// Wayland client that copies immediately gets a selection offer describing its
/// own bytes. The obvious tidy-up is to remember `GetClipboardSequenceNumber`
/// after our own write and swallow the echo.
///
/// **That would break copying and pasting inside one client.** wc-proxy does
/// not forward `wl_data_device.set_selection` — it cannot, because the
/// compositor would then have to answer `wl_data_source.send` with a
/// descriptor — so a client that copies has told the compositor nothing about
/// having a selection. The echo is the *only* thing that gives that client a
/// `wl_data_offer` for what it just copied. Swallow it and Ctrl-C followed by
/// Ctrl-V in the same window pastes nothing, which is the first thing anybody
/// tries.
///
/// The cost of keeping it is one extra round trip per copy. That is the right
/// side of the trade, and it is also what makes client-to-client copying work
/// transitively through Windows' own clipboard.
fn on_clipboard_update(ui: &Rc<Ui>, why: &str) {
    let formats = available_formats(ui);
    let mimes = crate::clipboard::mimes_for(&formats);
    println!(
        "wc-compositor: {why}: [{}] -> {} mime type(s)",
        formats.iter().map(|f| f.name()).collect::<Vec<_>>().join(", "),
        mimes.len()
    );
    let _ = ui.events.send(FromUi::ClipboardChanged { mimes });
}

/// `RegisterClipboardFormatW("HTML Format")`, once per process.
fn html_format(ui: &Rc<Ui>) -> u32 {
    if ui.html_format.get() != 0 {
        return ui.html_format.get();
    }
    let name = wide("HTML Format");
    // SAFETY: a NUL-terminated UTF-16 buffer that outlives the call.
    let id = unsafe { RegisterClipboardFormatW(name.as_ptr()) };
    ui.html_format.set(id);
    id
}

/// §93. `RegisterClipboardFormatW("PNG")`: the name browsers, Office and the
/// Snipping Tool register, so it is the same number theirs is.
fn png_format(ui: &Rc<Ui>) -> u32 {
    if ui.png_format.get() != 0 {
        return ui.png_format.get();
    }
    let name = wide("PNG");
    // SAFETY: a NUL-terminated UTF-16 buffer that outlives the call.
    let id = unsafe { RegisterClipboardFormatW(name.as_ptr()) };
    ui.png_format.set(id);
    id
}

pub(crate) fn format_id(ui: &Rc<Ui>, format: SystemFormat) -> u32 {
    match format {
        SystemFormat::UnicodeText => CF_UNICODETEXT,
        SystemFormat::Html => html_format(ui),
        SystemFormat::HDrop => CF_HDROP,
        SystemFormat::Png => png_format(ui),
        SystemFormat::Dib => CF_DIB,
    }
}

/// Which of the formats this compositor carries are on the clipboard.
///
/// `IsClipboardFormatAvailable` rather than `EnumClipboardFormats`, because it
/// does **not** require the clipboard to be open — which matters more than the
/// three calls it saves. Opening the clipboard inside a `WM_CLIPBOARDUPDATE`
/// handler is a fight with whichever application has just finished writing to
/// it and has not yet closed it, and losing that fight would mean announcing an
/// empty clipboard to Wayland at precisely the moment something was copied.
fn available_formats(ui: &Rc<Ui>) -> Vec<SystemFormat> {
    SystemFormat::ALL
        .into_iter()
        .filter(|f| {
            let id = format_id(ui, *f);
            // SAFETY: a pure query taking a format number.
            id != 0 && unsafe { IsClipboardFormatAvailable(id) } != 0
        })
        .collect()
}

/// Run `f` with the clipboard open, or return `None`.
///
/// **The retry is not defensive coding.** The clipboard is a single global
/// lock held by whichever process is reading or writing it, `OpenClipboard`
/// fails outright rather than waiting, and the documented remedy is to try
/// again. Losing the race silently would mean a copy that did nothing, so the
/// failure is logged with the reason and the window that holds it.
fn with_clipboard<T>(ui: &Rc<Ui>, what: &str, f: impl FnOnce() -> T) -> Option<T> {
    let hwnd = ui.controller.get() as HWND;
    for attempt in 0..8 {
        // SAFETY: a live window handle owned by this thread.
        if unsafe { OpenClipboard(hwnd) } != 0 {
            let out = f();
            // SAFETY: the clipboard was opened by this thread just above.
            unsafe { CloseClipboard() };
            return Some(out);
        }
        if attempt < 7 {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }
    eprintln!(
        "wc-compositor: could not open the clipboard to {what}: {}",
        std::io::Error::last_os_error()
    );
    None
}

/// One format's bytes, copied out of the clipboard's global handle.
///
/// The handle belongs to the clipboard and is **not** freed here; only the lock
/// is released. Freeing it is how an application takes the clipboard down with
/// it, and it is the classic Win32 clipboard bug.
fn read_format(id: u32) -> Option<Vec<u8>> {
    // SAFETY: the clipboard is open (the caller is inside `with_clipboard`).
    let handle = unsafe { GetClipboardData(id) };
    if handle.is_null() {
        return None;
    }
    // SAFETY: a handle the clipboard just gave us.
    let ptr = unsafe { GlobalLock(handle) };
    if ptr.is_null() {
        return None;
    }
    // SAFETY: the same handle, still locked.
    let len = unsafe { GlobalSize(handle) };
    // SAFETY: `ptr` addresses `len` bytes for as long as the lock is held.
    let bytes = unsafe { std::slice::from_raw_parts(ptr.cast::<u8>(), len) }.to_vec();
    // SAFETY: balancing the lock taken above.
    unsafe { GlobalUnlock(handle) };
    Some(bytes)
}

/// Put one format on the clipboard. The clipboard must already be open and
/// emptied.
fn write_format(id: u32, bytes: &[u8]) -> bool {
    // SAFETY: an allocation request; the handle is checked below.
    let handle = unsafe { GlobalAlloc(GMEM_MOVEABLE, bytes.len().max(1)) };
    if handle.is_null() {
        return false;
    }
    // SAFETY: a handle just allocated here.
    let ptr = unsafe { GlobalLock(handle) };
    if ptr.is_null() {
        // SAFETY: ours, never handed to the clipboard.
        unsafe { GlobalFree(handle) };
        return false;
    }
    // SAFETY: `ptr` addresses at least `bytes.len()` bytes and the source is a
    // live slice; the two cannot overlap.
    unsafe {
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), ptr.cast::<u8>(), bytes.len());
        GlobalUnlock(handle);
    }
    // SAFETY: the clipboard is open and owned by this thread. **On success the
    // system owns `handle`** — which is why there is no `GlobalFree` on this
    // path and why there must be one on the other.
    if unsafe { SetClipboardData(id, handle) }.is_null() {
        // SAFETY: the call failed, so ownership never transferred.
        unsafe { GlobalFree(handle) };
        return false;
    }
    true
}

/// A Wayland selection as the numbered Windows formats it becomes.
///
/// The one place the conversion (`crate::clipboard`, which is arithmetic and is
/// tested on Linux) meets the numbering (which is Win32's and half of it is
/// minted at runtime). **Shared by the clipboard and the drag source on
/// purpose**: a drag's data and a selection's data are the same bytes in the
/// same formats, and building them twice would be two ways for one client's
/// selection to arrive differently.
pub(crate) fn system_formats(ui: &Rc<Ui>, payloads: &[(String, Vec<u8>)]) -> Vec<(u32, Vec<u8>)> {
    crate::clipboard::to_system(payloads)
        .into_iter()
        .filter_map(|(format, bytes)| {
            let id = format_id(ui, format);
            (id != 0).then_some((id, bytes))
        })
        .collect()
}

fn set_clipboard(ui: &Rc<Ui>, payloads: Vec<(String, Vec<u8>)>) {
    let system = crate::clipboard::to_system(&payloads);
    with_clipboard(ui, "publish a selection", || {
        // SAFETY: the clipboard is open. `EmptyClipboard` also makes this
        // process the owner, which is required before `SetClipboardData`.
        unsafe { EmptyClipboard() };
        for (format, bytes) in &system {
            let id = format_id(ui, *format);
            let ok = id != 0 && write_format(id, bytes);
            println!(
                "wc-compositor: clipboard set {} ({} bytes): {}",
                format.name(),
                bytes.len(),
                if ok { "ok" } else { "failed" }
            );
        }
        if system.is_empty() {
            println!("wc-compositor: clipboard cleared");
        }
    });
}

/// Hand the staged payloads to OLE as a drag source.
///
/// See [`super::drag`] for the `IDataObject`/`IDropSource` pair and for why
/// `DoDragDrop` is entered from a posted message rather than from here.
///
/// `run` answers immediately only when the drag could not be started at all;
/// otherwise the answer comes from [`WM_WC_DRAG`](super::drag::WM_WC_DRAG),
/// after the loop. Either way exactly one `DragFinished` is sent, which is what
/// stops a client's `wl_data_source` waiting forever.
fn start_drag(ui: &Rc<Ui>, payloads: Vec<(String, Vec<u8>)>, actions: u32) {
    if let Some(action) = super::drag::run(ui, &payloads, actions) {
        let _ = ui.events.send(FromUi::DragFinished { action });
    }
}

fn cancel_drag(ui: &Rc<Ui>) {
    super::drag::cancel(ui);
}

fn fetch_clipboard(ui: &Rc<Ui>, token: u64, from_drag: bool, mime: &str) {
    if from_drag {
        let data = super::drag::payload(ui, mime);
        println!(
            "wc-compositor: drag fetch token={token} mime={mime:?} -> {}",
            match &data {
                Some(b) => format!("{} bytes", b.len()),
                None => "nothing".to_owned(),
            }
        );
        let _ = ui.events.send(FromUi::ClipboardData { token, data });
        return;
    }
    let formats = available_formats(ui);
    let data = with_clipboard(ui, "answer a paste", || {
        formats.iter().filter(|f| crate::clipboard::can_answer(mime, **f)).find_map(|f| {
            let bytes = read_format(format_id(ui, *f))?;
            crate::clipboard::from_system(mime, *f, &bytes)
        })
    })
    .flatten();
    match &data {
        Some(b) => println!(
            "wc-compositor: clipboard fetch token={token} mime={mime:?} -> {} bytes",
            b.len()
        ),
        // The one case worth the extra open: the answer was nothing, and the
        // difference between "the clipboard is empty" and "it holds a bitmap"
        // is otherwise invisible.
        None => println!(
            "wc-compositor: clipboard fetch token={token} mime={mime:?} -> nothing; \
             the clipboard holds formats [{}]",
            describe_clipboard(ui)
        ),
    }
    let _ = ui.events.send(FromUi::ClipboardData { token, data });
}

/// Every clipboard format on the clipboard right now, by number, for the log.
///
/// Only called when `--trace`-ish curiosity is warranted — a paste that found
/// nothing — because it needs the clipboard open and the answer is a list of
/// integers that mean nothing without a registry lookup. It is here because
/// "the client asked for text/plain and got nothing" and "the clipboard held
/// only CF_BITMAP" are the same log line otherwise.
fn describe_clipboard(ui: &Rc<Ui>) -> String {
    with_clipboard(ui, "list its formats", || {
        let mut out = Vec::new();
        let mut id = 0u32;
        loop {
            // SAFETY: the clipboard is open; 0 starts the enumeration.
            id = unsafe { EnumClipboardFormats(id) };
            if id == 0 {
                break;
            }
            out.push(id.to_string());
        }
        out.join(", ")
    })
    .unwrap_or_default()
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
        if let ToUi::Quit { clients } = cmd {
            // STAGE 54, and **before** `PostQuitMessage`: the enumeration has to
            // happen while this thread still owns its windows. After the quit
            // message the pump unwinds and the process tears down, at which
            // point every window goes away for a reason that has nothing to do
            // with whether the compositor destroyed it.
            report_window_leaks(ui, clients);
            // SAFETY: ends this thread's `GetMessage` loop.
            unsafe { PostQuitMessage(0) };
            break;
        }
        handle(ui, cmd);
    }
    ui.draining.set(false);
}

/// How many `WaycomposeToplevel` windows **this process** still owns, asked of
/// Windows rather than of our own bookkeeping. STAGE 54.
///
/// This is the half that makes the leak report evidence instead of an
/// assertion. `made` and `gone` are what the compositor believes; this is what
/// the window manager will tell anybody who asks. A `Destroy` that removed the
/// map entry and then failed to destroy the window — `DestroyWindow` returning
/// false, a window owned by the wrong thread, a handle already replaced —
/// leaves those two numbers balanced and this one non-zero, and that gap is the
/// entire bug this stage exists to make visible.
///
/// Filtered by process id, not just by class: `wc-proxy.sh` and `wc-guest.sh`
/// run several compositors at once on a shared machine, and §16.8's rule is
/// that identity on a shared box is never a class of thing. A count that swept
/// up another run's windows would fail a harness for somebody else's leak.
fn live_toplevels() -> u32 {
    struct Count {
        pid: u32,
        n: u32,
    }
    unsafe extern "system" fn visit(hwnd: HWND, lparam: LPARAM) -> i32 {
        // SAFETY: `lparam` is the `&mut Count` handed to `EnumWindows` below,
        // which outlives the enumeration.
        let c = unsafe { &mut *(lparam as *mut Count) };
        let mut pid: u32 = 0;
        // SAFETY: a live handle from the enumerator and a valid out-pointer.
        unsafe { GetWindowThreadProcessId(hwnd, &mut pid) };
        if pid != c.pid {
            return 1;
        }
        let mut buf = [0u16; 64];
        // SAFETY: a live handle and a buffer with its own length.
        let n = unsafe { GetClassNameW(hwnd, buf.as_mut_ptr(), buf.len() as i32) };
        if n > 0 && String::from_utf16_lossy(&buf[..n as usize]) == CLASS_TOPLEVEL {
            c.n += 1;
        }
        1
    }
    let mut c = Count {
        // SAFETY: no arguments, no failure mode.
        pid: unsafe { windows_sys::Win32::System::Threading::GetCurrentProcessId() },
        n: 0,
    };
    // SAFETY: a real callback and a pointer that outlives the call.
    unsafe { EnumWindows(Some(visit), &mut c as *mut Count as LPARAM) };
    c.n
}

/// The line every harness asserts on. STAGE 54.
///
/// Printed on the way out, from the UI thread, because that is the thread that
/// owns the windows and the only one entitled to enumerate them.
fn report_window_leaks(ui: &Rc<Ui>, clients: u32) {
    let made = ui.made.get();
    let gone = ui.gone.get();
    let held = ui.windows.borrow().len();
    let live = live_toplevels();
    println!(
        "wc-compositor: windows at exit: {made} created, {gone} destroyed, \
         {held} still held, {live} still alive, {clients} client(s) still connected"
    );
    // ADDED FOR docs/waycompose-design.md §62. The same accounting for the
    // *other* per-window resource, and it rides on this line's coat-tails for
    // §56.2's reason: a stranded OLE registration has no symptom at all, so the
    // only way it is ever noticed is a number printed where a run already
    // looks.
    //
    // These three are the compositor's own account and are labelled `claims`
    // for the same reason §56 labels its first three: a bug in the teardown
    // path is exactly the circumstance in which the account is wrong. Unlike
    // the window count there is no external witness available — OLE's table is
    // not enumerable from another process — which §60.3 says out loud rather
    // than leaving the asymmetry to be noticed.
    let (registered, revoked, failed) = super::drag::drop_target_counts();
    println!(
        "wc-compositor: drop targets at exit: claims {registered} registered, \
         {revoked} revoked, {failed} revoke(s) failed"
    );
    // ADDED FOR docs/waycompose-design.md §64, the third per-window resource.
    //
    // `outstanding` is **not required to be zero** and saying so here is the
    // point: the cursor on screen is deliberately still alive, because
    // `set_cursor` frees the previous one only after the new one is installed.
    // A number that is allowed to be one and is printed as a number is a gate;
    // a number asserted to be zero would fail on every run that moved the mouse
    // and would then be "fixed" by deleting it.
    let made = ICON_OBJECTS_MADE.with(|c| c.get());
    let icons = ICONS_FREED.with(|c| c.get());
    let cursors = CURSORS_FREED.with(|c| c.get());
    println!(
        "wc-compositor: icon objects at exit: claims {made} created, \
         {icons} icon(s) and {cursors} cursor(s) destroyed, \
         {} outstanding (the shown cursor is one)",
        made.saturating_sub(icons + cursors)
    );
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
            aumid,
            shadow,
            kind,
            bounds,
        } => {
            if let Err(e) = create_window(ui, id, &title, &app_id, &aumid, shadow, kind, bounds) {
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
        ToUi::AppId { id, app_id, aumid } => {
            // The *pixels* for an app id are still somebody else's problem —
            // they are resolved on the machine the client is on and arrive as a
            // `ToUi::Icon` (§16.4). What arrives here is the identity, and
            // until stage 32 this arm did nothing at all.
            println!("wc-compositor: window {id} app_id -> {app_id:?}");
            if let Some(w) = window(ui, id) {
                apply_identity(&w, &aumid);
            }
        }
        ToUi::Icon { id, icons } => {
            if let Some(w) = window(ui, id) {
                set_icon(&w, &icons);
            }
        }
        ToUi::Map { id, width, height, fold_caption } => {
            if let Some(w) = window(ui, id) {
                map_window(&w, width, height, fold_caption);
            }
        }
        ToUi::Resize { id, width, height } => {
            if let Some(w) = window(ui, id) {
                follow_client_size(&w, width, height);
            }
        }
        ToUi::SizeHints { id, min, max } => {
            if let Some(w) = window(ui, id) {
                w.size_hints.set((min, max));
                println!(
                    "wc-compositor: window {id} size hints: min {}x{}, max {}x{} (0 = none)",
                    min.0, min.1, max.0, max.1
                );
            }
        }
        // ADDED FOR STAGE 13. `SWP_NOACTIVATE` and no `ShowWindow`: the window
        // is already up, and re-showing it would flicker.
        ToUi::MovePopup { id, x, y, width, height } => {
            if let Some(w) = window(ui, id) {
                w.popup_offset.set((x, y));
                let mut origin = POINT { x, y };
                // SAFETY: live handles; `ClientToScreen` converts in place and
                // the owner is the one set at creation.
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
                }
                update_layered(&w);
                println!(
                    "wc-compositor: window {id} repositioned to {},{} {width}x{height}",
                    origin.x, origin.y
                );
            }
        }
        // §45.11. The corner moved; the size did not (a size change arrives on
        // the ordinary present path) and neither did the z-order or the focus.
        ToUi::ShiftOrigin { id, dx, dy } => {
            if let Some(w) = window(ui, id) {
                let mut r = RECT { left: 0, top: 0, right: 0, bottom: 0 };
                // SAFETY: a live handle, and `GetWindowRect` only writes the
                // rectangle it is given.
                unsafe {
                    if GetWindowRect(w.hwnd(), &mut r) != 0 {
                        SetWindowPos(
                            w.hwnd(),
                            std::ptr::null_mut(),
                            r.left + dx,
                            r.top + dy,
                            0,
                            0,
                            SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE,
                        );
                        println!(
                            "wc-compositor: window {id} origin shifted by {dx},{dy} \
                             (its content did not move)"
                        );
                    }
                }
            }
        }
        ToUi::Present { id, damage } => {
            if let Some(w) = window(ui, id) {
                present(&w, &damage);
                dump_frame(ui, &w);
            }
        }
        // §104's follow-up.
        ToUi::Direct { id, frame, damage, serial } => {
            if let Some(w) = window(ui, id) {
                direct(ui, &w, frame, &damage, serial);
            } else {
                // Gone already: nothing draws from anything, and the Wayland
                // side is waiting on this answer to release the buffers.
                let _ = ui.events.send(FromUi::DirectShown { id, serial, ok: true });
            }
        }
        ToUi::Cursor { spec } => set_cursor(ui, spec),
        ToUi::SetWindowState {
            id,
            maximized,
            minimized,
        } => {
            if let Some(w) = window(ui, id) {
                set_window_state(&w, maximized, minimized);
            }
        }
        // §96.
        ToUi::Fullscreen { id, on } => {
            if let Some(w) = window(ui, id) {
                set_fullscreen(&w, on);
            }
        }
        // ADDED FOR STAGE 17. All three **post** rather than send, and that is
        // the only interesting thing about them.
        //
        // `SC_MOVE`, `SC_SIZE` and `TrackPopupMenu` each enter a modal loop
        // that does not return until the user lets go. This code runs inside
        // `drain`, which sets `ui.draining` for its duration and re-posts
        // `WM_WC_COMMAND` whenever another arrives while it is set — so a
        // `SendMessageW` here would hold `draining` for the whole gesture and
        // spin the pump re-posting a wake it cannot service. Posting lets the
        // drain finish; the loop is then entered from the pump, where §8.2's
        // finding applies and a posted wake still reaches the window.
        ToUi::BeginMove { id } => {
            if let Some(w) = window(ui, id) {
                begin_sysmenu_gesture(&w, SC_MOVE, "move");
            }
        }
        ToUi::BeginResize { id, edge } => {
            if let Some(w) = window(ui, id) {
                // Win32 encodes the edge in the low four bits of the
                // `WM_SYSCOMMAND` argument, as `WMSZ_*` + 1. Wayland encodes it
                // as a bitmask. Neither is derivable from the other, so this is
                // a table.
                let wmsz = match edge {
                    Edge::Left => 1,
                    Edge::Right => 2,
                    Edge::Top => 3,
                    Edge::TopLeft => 4,
                    Edge::TopRight => 5,
                    Edge::Bottom => 6,
                    Edge::BottomLeft => 7,
                    Edge::BottomRight => 8,
                    // No edge: `SC_SIZE` on its own is the keyboard-driven
                    // resize, which is what the protocol's `none` means.
                    Edge::None => 0,
                };
                begin_sysmenu_gesture(&w, SC_SIZE | wmsz, "resize");
            }
        }
        ToUi::WindowMenu { id, x, y } => {
            if let Some(w) = window(ui, id) {
                // Surface coordinates to screen coordinates. Surface and client
                // coordinates are the same thing (`to_surface`), so this is
                // `ClientToScreen` and nothing else.
                let mut p = POINT { x, y };
                // SAFETY: a live window handle; `ClientToScreen` converts in
                // place. The result is packed into an `LPARAM` the way
                // `WM_CONTEXTMENU` packs one, because that is what the handler
                // in `toplevel_proc` unpacks.
                unsafe {
                    ClientToScreen(w.hwnd(), &mut p);
                    PostMessageW(
                        w.hwnd(),
                        WM_WC_SYSMENU,
                        0,
                        ((p.x as u32 as isize) & 0xffff) | (((p.y as u32 as isize) & 0xffff) << 16),
                    );
                }
                println!("wc-compositor: window {id} window menu at {},{} on screen", p.x, p.y);
            }
        }
        // ADDED FOR STAGE 21.
        ToUi::IdleInhibit { on } => set_idle_inhibit(ui, on),
        // §96.
        ToUi::PointerHold { id, hold: h } => hold::set(ui, id, h),
        ToUi::Activate { id } => {
            if let Some(w) = window(ui, id) {
                activate_window(&w);
            }
        }
        ToUi::SetClipboard { payloads } => set_clipboard(ui, payloads),
        ToUi::FetchClipboard { token, from_drag, mime } => {
            fetch_clipboard(ui, token, from_drag, &mime)
        }
        ToUi::StartDrag { payloads, actions } => start_drag(ui, payloads, actions),
        ToUi::CancelDrag => cancel_drag(ui),
        // §42. The client under an incoming drag has answered; remember it, so
        // that the next `IDropTarget::DragOver` — which must answer OLE
        // synchronously and cannot wait for this — has something true to say.
        ToUi::DragFeedback { accepted, action } => super::drag::feedback(accepted, action),
        ToUi::Destroy { id } => {
            // §96: a clip on a window that is going away is let go first.
            hold::on_destroy(ui, id);
            let w = ui.windows.borrow_mut().remove(&id);
            if let Some(w) = w {
                // §102: a named toplevel leaves its place to the next window of
                // its identity (the nested desktop's session after its login
                // screen). Not a minimised one: it has no place on screen.
                if !matches!(w.kind, WindowKind::Popup { .. })
                    && w.mapped.get()
                    && !w.hwnd().is_null()
                    && super::leaves_succession(&w.identity.borrow())
                {
                    // SAFETY: a live window handle, not yet destroyed.
                    let (iconic, zoomed) =
                        unsafe { (IsIconic(w.hwnd()) != 0, IsZoomed(w.hwnd()) != 0) };
                    if !iconic {
                        let origin = window_origin(w.hwnd());
                        SUCCESSION.with(|s| {
                            s.replace(Some(super::Succession {
                                aumid: w.identity.borrow().clone(),
                                origin,
                                maximized: zoomed,
                                at: std::time::Instant::now(),
                            }))
                        });
                    }
                }
                // The window is about to go and the `HICON`s belong to this
                // process rather than to it. Not `set_icon(&w, &[])`: that is
                // the *client* saying it has no icon, and it says so in the
                // log, which on a teardown reads as a failure that has not
                // happened.
                release_icons(&w);
                let hwnd = w.hwnd();
                if !hwnd.is_null() {
                    // §42: **before** `DestroyWindow`. OLE keys a drop-target
                    // registration on the `HWND`, and a destroyed handle
                    // cannot be revoked — the entry would stay in OLE's table
                    // until the process exits, which on a compositor whose
                    // windows come and go with a client's menus grows with use.
                    super::drag::revoke(hwnd);
                    // SAFETY: the window was created on this thread, which is
                    // the only thread allowed to destroy it. This sends
                    // `WM_DESTROY`, which reclaims the `Rc` held in
                    // `GWLP_USERDATA`.
                    unsafe {
                        DestroyWindow(hwnd);
                    }
                }
                ui.gone.set(ui.gone.get() + 1);
                println!("wc-compositor: window {id} destroyed");
            }
        }
        ToUi::Quit { .. } => unreachable!("handled in drain"),
    }
}

// ---------------------------------------------------------------------------
// Idle inhibition and activation — stage 21
// ---------------------------------------------------------------------------

/// Keep the display awake while a client holds an idle inhibitor.
///
/// # `SetThreadExecutionState` is per *thread*, and this is the thread
///
/// The state is a property of the calling thread and the system takes the union
/// over every thread that has one. It is released when that thread exits, which
/// is exactly the cleanup this feature wants and exactly the trap it would be if
/// it were called from anywhere else: the Wayland thread is where the protocol
/// lives, and had the call been made there, a compositor whose Wayland thread
/// wound up first would drop the inhibition a moment before the process ended —
/// harmless — while a compositor that later moved that work to a worker thread
/// would silently stop inhibiting anything. So it is made from the UI thread,
/// which is `main`'s thread and outlives everything. `handle` runs on it, and
/// it is the only caller.
///
/// # `ES_CONTINUOUS` is what makes it a state rather than a nudge
///
/// Without it the call is a one-shot "the display is in use right now" that
/// resets the idle timer once. With it, the flags stay set until they are
/// cleared, which is what `zwp_idle_inhibitor_v1` means — the inhibitor lasts
/// until the client destroys it. Clearing is `ES_CONTINUOUS` **alone**: passing
/// zero is documented as an error, and passing `ES_CONTINUOUS` with no other
/// flag is how the state is reset.
///
/// # What is deliberately not asked for
///
/// `ES_SYSTEM_REQUIRED`, which would also stop the machine suspending. The
/// protocol asks the compositor not to blank, lock or screensave — display
/// idleness — and says nothing about whether the computer may sleep. Asking for
/// both would be this compositor deciding a power policy the client did not ask
/// for, on a machine that is a guest VM whose host can suspend it regardless.
///
/// # Why `WM_SYSCOMMAND` is handled too
///
/// `ES_DISPLAY_REQUIRED` resets the *display* idle timer. The screen saver has
/// its own timer, driven by input idleness, and a video playing is precisely the
/// case with no input — so on a machine with a screen saver configured, the
/// execution state alone leaves the hole open. `toplevel_proc` refuses
/// `SC_SCREENSAVE` and `SC_MONITORPOWER` while this is set, which is the
/// documented way for a foreground application to say no.
fn set_idle_inhibit(ui: &Rc<Ui>, on: bool) {
    if ui.idle_inhibited.replace(on) == on {
        return;
    }
    let flags = if on {
        ES_CONTINUOUS | ES_DISPLAY_REQUIRED
    } else {
        ES_CONTINUOUS
    };
    // SAFETY: a scalar in, a scalar out, no handles and no memory. The only
    // requirement is the thread one, which is discharged above.
    let previous = unsafe { SetThreadExecutionState(flags) };
    if previous == 0 {
        // The one documented failure. Said rather than swallowed, because the
        // symptom otherwise is "the screen blanked during a video" with nothing
        // anywhere to say the compositor tried.
        eprintln!(
            "wc-compositor: SetThreadExecutionState({flags:#x}) failed; the display will \
             blank on its own timer"
        );
        return;
    }
    println!(
        "wc-compositor: idle inhibit {} (SetThreadExecutionState {flags:#x}, was {previous:#x})",
        if on { "on" } else { "off" }
    );
}

/// `xdg_activation_v1.activate`: bring a window to the foreground.
///
/// # Windows decides, and here is when it says yes
///
/// `SetForegroundWindow` is not a command. Windows refuses it from a process
/// that is not entitled to steal the foreground, and the entitlement rules are
/// roughly: the calling process already owns the foreground window, or it
/// received the last input event, or there is no foreground window at all, or
/// the foreground lock timeout has expired.
///
/// **The common case is the permitted one, and that is a property of this
/// design rather than luck.** Every toplevel in this compositor is an `HWND`
/// owned by one process — wc-compositor — no matter how many Wayland clients
/// are behind them. So "the user clicks a link in one client's window and
/// another client's window raises" is a foreground request made by the process
/// that already owns the foreground window, which Windows allows. Two clients
/// on two machines, one Win32 process, one set of foreground rights.
///
/// The case that is refused is a client that asks unprompted while the user is
/// in some other application — Notepad, a browser outside the compositor. There
/// Windows says no, and it is right to: that is the behaviour the rule exists to
/// stop, and a Wayland compositor on Linux would apply a policy of its own for
/// the same reason. What the user gets instead is the flashing taskbar button,
/// which is the platform's own idiom for "this window wants you" — requested
/// explicitly here rather than relying on the system's implicit flash, so the
/// outcome does not depend on an undocumented courtesy.
///
/// `FLASHW_TIMERNOFG` means "flash until the window comes to the foreground",
/// so the notification clears itself when the user acts on it and there is
/// nothing to cancel.
///
/// # Why `AllowSetForegroundWindow` is not called
///
/// It grants foreground rights to a *different process*, by pid. There is no
/// different process: the request comes from a Wayland client over a socket and
/// the window is ours. Calling it would be handing a right to somebody who does
/// not need it. It becomes relevant the day the compositor launches a Windows
/// application on a client's behalf, and not before.
///
/// # The client cannot be told
///
/// `xdg_activation_v1` version 1 has no failure event: `activate` is a request
/// with no reply, and a client that asks has no way to learn that Windows said
/// no. So the log line below is the only place the answer exists, and it says
/// which of the two happened rather than just that something was attempted.
fn activate_window(w: &Rc<WindowState>) {
    let hwnd = w.hwnd();
    if hwnd.is_null() {
        return;
    }
    // SAFETY: a live window handle created on this thread, and this is that
    // thread. Every call below is a plain query or a window-manager request.
    unsafe {
        // A minimised window cannot be the foreground one. Restoring first is
        // what the user means by "raise it", and doing it before the foreground
        // request means the request is not spent on a window that is still an
        // icon.
        if IsIconic(hwnd) != 0 {
            w.self_driven.set(true);
            ShowWindow(hwnd, SW_RESTORE);
            w.self_driven.set(false);
        }
        if SetForegroundWindow(hwnd) != 0 {
            println!("wc-compositor: window {} activated (SetForegroundWindow)", w.id);
            return;
        }
        // Refused. `GetForegroundWindow` is read only to say *why* in the log:
        // a null result is "nobody has it", anything else is another
        // application's window and the answer is the foreground lock.
        let owner = GetForegroundWindow();
        println!(
            "wc-compositor: window {} asked to be activated and Windows refused (the \
             foreground belongs to {}); flashing its taskbar button instead",
            w.id,
            if owner.is_null() { "nothing" } else { "another application" }
        );
        let info = FLASHWINFO {
            cbSize: std::mem::size_of::<FLASHWINFO>() as u32,
            hwnd,
            dwFlags: FLASHW_TRAY | FLASHW_TIMERNOFG,
            uCount: 0,
            dwTimeout: 0,
        };
        FlashWindowEx(&info);
    }
}

fn create_window(
    ui: &Rc<Ui>,
    id: WindowId,
    title: &str,
    app_id: &str,
    aumid: &str,
    shadow: Arc<Mutex<Shadow>>,
    kind: WindowKind,
    bounds: PopupBounds,
) -> std::io::Result<()> {
    let state = Rc::new(WindowState {
        id,
        kind,
        bounds,
        hwnd: Cell::new(0),
        shadow,
        events: ui.events.clone(),
        frame: Cell::new(ui.frame),
        last_configured: Cell::new((0, 0)),
        maximized: Cell::new(false),
        minimized: Cell::new(false),
        mapped: Cell::new(false),
        unpainted_since: Cell::new(None),
        dibs: RefCell::new(Vec::new()),
        direct_on: Cell::new(None),
        self_driven: Cell::new(false),
        placing: Cell::new(false),
        maximize_at_map: Cell::new(false),
        fullscreen: Cell::new(None),
        user_sizing: Cell::new(false),
        size_hints: Cell::new(((0, 0), (0, 0))),
        icon_big: Cell::new(0),
        frames: Cell::new(0),
        icon_small: Cell::new(0),
        nc_top: Cell::new(0),
        caption_colour: Cell::new(None),
        corner_clear: Cell::new(None),
        fallback_said: Cell::new(false),
        popup_offset: Cell::new(match kind {
            WindowKind::Popup { x, y, .. } => (x, y),
            WindowKind::Toplevel => (0, 0),
        }),
        aumid: RefCell::new(String::new()),
        identity: RefCell::new(String::new()),
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
            // `WS_EX_LAYERED` on a popup is STAGE 13's, and it is the one
            // thing that gives a menu its shadow and its rounded corners
            // instead of a black rectangle. See `update_layered`.
            match kind {
                WindowKind::Toplevel => 0,
                WindowKind::Popup { .. } => WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW | WS_EX_LAYERED,
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
    // ADDED FOR TABS IN THE TITLE BAR, and it has to be *after* the line above: the
    // `WM_NCCALCSIZE` that `CreateWindowExW` already sent arrived while
    // `GWLP_USERDATA` was still 0, so `toplevel_proc` saw no state and let
    // `DefWindowProc` build an ordinary frame. This is the first moment the
    // window can be asked the question again with an answer available.
    apply_frame(&state);
    // STAGE 38: before the window is ever shown, because the first popup can
    // arrive before the first `WM_WINDOWPOSCHANGED` does. `CW_USEDEFAULT` has
    // put the window somewhere already, so `MonitorFromWindow` has an answer,
    // and it is refreshed on every move from here on.
    //
    // **After `apply_frame`**, and for the same reason it is after the
    // `GWLP_USERDATA` line: the frame decides where the client area is, and
    // this converts a screen rectangle *into* client coordinates. Publishing
    // first would publish a rectangle measured against a frame the window is
    // about to stop having.
    state.publish_popup_bounds();
    // §42: this window can be dropped onto, from now until it is destroyed.
    //
    // **Toplevels only.** A popup is a menu, not a destination — and
    // registering one would mean revoking it on a window whose lifetime is a
    // gesture, which is the revoke most likely to be missed. A drag that ends
    // over a menu ends over the toplevel underneath it, which is what happens
    // on the rest of the desktop too.
    if matches!(kind, WindowKind::Toplevel) {
        super::drag::register(hwnd, id);
    }
    ui.windows.borrow_mut().insert(id, state.clone());
    ui.made.set(ui.made.get() + 1);

    println!("wc-compositor: window {id} created (HWND {hwnd:p}) title={title:?} app_id={app_id:?}");
    // ADDED FOR STAGE 32, and *here* rather than at map for one reason: the
    // window is still hidden. The shell has not seen it, so it has no taskbar
    // button for it yet, so the identity it eventually reads is whatever is on
    // the property store at `ShowWindow`. Doing it now costs nothing and makes
    // the map-time call an idempotent no-op for every client that named itself
    // before its first buffer.
    apply_identity(&state, aumid);
    Ok(())
}

// ---------------------------------------------------------------------------
// TABS IN THE TITLE BAR: the caption, folded into the client area
// ---------------------------------------------------------------------------
//
// The goal document's second clause is Firefox's tab strip *inside* the
// window's own title bar rather than beneath it, which is what Firefox on
// Windows does natively. Three pieces do it, and only the first is
// load-bearing:
//
// 1. `WM_NCCALCSIZE` moves the client rectangle's **top edge** up to the top of
//    the window, so the caption's pixels belong to the client area. Everything
//    else — the side borders, the bottom border, the shadow, the rounded
//    corners, the snap-to-edge behaviour, `SC_MOVE`, `SC_SIZE`, the system menu
//    — is still Windows', because the window is still a plain
//    `WS_OVERLAPPEDWINDOW` and nothing else about the frame was touched.
// 2. `WM_NCHITTEST` puts the top resize edge back. It was non-client until item
//    1 took it, and a window that cannot be resized from the top is a
//    regression a picture does not show.
// 3. `DwmExtendFrameIntoClientArea` asks DWM to keep drawing its frame behind
//    the strip. This one is optional and `--dwm-glass off` is how that was
//    measured rather than assumed; see `FrameStyle::dwm_glass`.
//
// **What is deliberately not here is `HTCAPTION`.** §22.8 is the reason and it
// is a protocol fact: Wayland has no way for a client to publish its draggable
// region, so this compositor cannot know which parts of Firefox's tab strip are
// a title bar and which are a tab. Every drag therefore arrives as
// `xdg_toplevel.move` after the click, which §22.7 wired, and the surface stays
// `HTCLIENT` throughout. §31.8 says what that costs.

/// `DWMWA_CAPTION_BUTTON_BOUNDS`, the fifth `DWMWINDOWATTRIBUTE`.
///
/// Asking Windows where it drew its own minimise, maximise and close buttons.
/// There is no other way to know: the strip's width is the theme's, the DPI's
/// and — on Windows 11 — the *build's*, and every hard-coded
/// `3 * SM_CXSIZE` in the wild is wrong on some machine.
const DWMWA_CAPTION_BUTTON_BOUNDS: u32 = 5;

/// `DWMWA_BORDER_COLOR`, `DWMWA_CAPTION_COLOR` and `DWMWA_TEXT_COLOR`, the
/// three `DWMWINDOWATTRIBUTE`s Windows 11 added for exactly this arrangement.
///
/// They take a `COLORREF` — `0x00BBGGRR`, which is the *opposite* byte order
/// from the `BGRA` a client's buffer is in and from the `RGB` anybody writing
/// this from memory would produce. Getting it backwards gives a caption that is
/// a plausible colour and the wrong one, which is the failure mode that does
/// not look like a failure.
const DWMWA_BORDER_COLOR: u32 = 34;
const DWMWA_CAPTION_COLOR: u32 = 35;
const DWMWA_TEXT_COLOR: u32 = 36;

/// `MARGINS`, declared here rather than imported.
///
/// `windows-sys` files it under `Win32::UI::Controls` — with the common
/// controls, because that is where `Uxtheme.h` puts it — and pulling that
/// feature in for four `i32`s would add a large module to every build, which is
/// the same trade `WM_MOUSELEAVE` above is written out for.
#[repr(C)]
struct Margins {
    left: i32,
    right: i32,
    top: i32,
    bottom: i32,
}

/// One `user32` export, resolved by name and cached.
///
/// §82. The DPI entry points below are Windows 10 1607 and later, so they are
/// resolved rather than linked — the same treatment `GetDpiForWindow` gets in
/// `window_geometry` — and a missing one is a value the caller can fall back
/// from rather than a load failure at process start.
fn user32_fn(
    cache: &'static std::sync::OnceLock<Option<unsafe extern "system" fn() -> isize>>,
    symbol: &std::ffi::CStr,
) -> Option<unsafe extern "system" fn() -> isize> {
    *cache.get_or_init(|| {
        let name = wide("user32.dll");
        // SAFETY: a NUL-terminated module name; `user32` is already loaded and
        // the handle is deliberately not freed.
        let dll = unsafe { LoadLibraryW(name.as_ptr()) };
        if dll.is_null() {
            return None;
        }
        // SAFETY: an ASCII, NUL-terminated export name.
        unsafe { GetProcAddress(dll, symbol.as_ptr().cast()) }
    })
}

/// The DPI the *system* metrics are reported at: what `GetSystemMetrics` and
/// `AdjustWindowRectEx` answer in for a per-monitor-aware process. 96 when
/// `GetDpiForSystem` is missing, which is every Windows before 1607 and the
/// value those systems use.
fn system_dpi() -> u32 {
    static SYM: std::sync::OnceLock<Option<unsafe extern "system" fn() -> isize>> =
        std::sync::OnceLock::new();
    match user32_fn(&SYM, c"GetDpiForSystem") {
        // SAFETY: `GetDpiForSystem() -> UINT`.
        Some(f) => unsafe {
            let f: unsafe extern "system" fn() -> u32 = std::mem::transmute(f);
            f().max(1)
        },
        None => caption::BASE_DPI,
    }
}

/// The DPI of the monitor this window is on, as Windows assigned it.
///
/// **This is the number every caption metric must be at**, and before §82 none
/// of them were: `GetSystemMetrics` answers at the *system* DPI — the primary
/// monitor's at logon — so a window on a 150% monitor beside a 100% primary
/// was hit-tested with a 100% resize band and extended by a 100% caption.
/// §31.13 item 3 said this was right "by construction"; it was right only on a
/// machine with one monitor.
fn window_dpi(hwnd: HWND) -> u32 {
    static SYM: std::sync::OnceLock<Option<unsafe extern "system" fn() -> isize>> =
        std::sync::OnceLock::new();
    match user32_fn(&SYM, c"GetDpiForWindow") {
        // SAFETY: `GetDpiForWindow(HWND) -> UINT`, a live handle.
        Some(f) => unsafe {
            let f: unsafe extern "system" fn(HWND) -> u32 = std::mem::transmute(f);
            match f(hwnd) {
                0 => system_dpi(),
                d => d,
            }
        },
        None => system_dpi(),
    }
}

/// `GetSystemMetricsForDpi`, or `GetSystemMetrics` rescaled from the system DPI
/// where it does not exist.
///
/// The fallback is [`caption::rescale`], which is `MulDiv`'s rounding and is
/// unit-tested at 125% and 150%. It is a fallback and not an equal: Windows'
/// own per-DPI metrics are not always a linear scale of the 100% ones (the
/// caption height comes from a font), which is exactly why the real entry
/// point is preferred whenever it is there.
fn metric_for_dpi(index: i32, dpi: u32) -> i32 {
    static SYM: std::sync::OnceLock<Option<unsafe extern "system" fn() -> isize>> =
        std::sync::OnceLock::new();
    match user32_fn(&SYM, c"GetSystemMetricsForDpi") {
        // SAFETY: `GetSystemMetricsForDpi(int, UINT) -> int`.
        Some(f) => unsafe {
            let f: unsafe extern "system" fn(i32, u32) -> i32 = std::mem::transmute(f);
            f(index, dpi)
        },
        // SAFETY: `GetSystemMetrics` takes an index and returns an int.
        None => caption::rescale(unsafe { GetSystemMetrics(index as _) }, system_dpi(), dpi),
    }
}

/// The thickness of the resize border Windows draws around a sizing window,
/// **at this window's DPI**.
///
/// `SM_CXSIZEFRAME` alone is 4 px on a themed desktop and is *not* the grab
/// area: since Vista the sensitive band is that plus `SM_CXPADDEDBORDER`, which
/// is the invisible padding that makes an 8 px target out of a 4 px line. A
/// hit-test written against `SM_CXSIZEFRAME` on its own produces a window whose
/// top edge can be grabbed only by pixel-perfect aiming, which reads as a
/// resize that "sometimes" works.
///
/// §82: per window. At 150% the band is 12 px, not 8, and a hit test that used
/// the 100% number would put the top edge's last four pixels inside the tab
/// strip.
fn frame_border(hwnd: HWND) -> (i32, i32) {
    let dpi = window_dpi(hwnd);
    let pad = metric_for_dpi(SM_CXPADDEDBORDER, dpi);
    (
        caption::resize_band(metric_for_dpi(SM_CXSIZEFRAME, dpi), pad),
        caption::resize_band(metric_for_dpi(SM_CYSIZEFRAME, dpi), pad),
    )
}

/// The top frame this theme wants, in pixels: caption, buttons and top border,
/// **at this window's DPI**.
///
/// Measured with `AdjustWindowRectExForDpi` rather than assembled out of
/// `SM_CYCAPTION` and the border metrics, for the reason `map_window` already
/// gives: the sum of the metrics is not what Windows uses, and on a themed or
/// scaled desktop it is not even close. Used for the DWM margin, the fallback
/// button strip and the log line; the number `WM_NCCALCSIZE` hands the client
/// is still taken from `DefWindowProc`'s own answer.
///
/// §82: the window's DPI rather than the desktop's. The DWM margin is the
/// height of the region DWM composites through, and a 100% margin on a 150%
/// window is a hole 15 px shorter than the caption buttons it is meant to
/// uncover.
fn top_inset(hwnd: HWND) -> i32 {
    static SYM: std::sync::OnceLock<Option<unsafe extern "system" fn() -> isize>> =
        std::sync::OnceLock::new();
    let dpi = window_dpi(hwnd);
    let style = WS_OVERLAPPEDWINDOW | WS_CLIPCHILDREN | WS_CLIPSIBLINGS;
    let mut rc = RECT { left: 0, top: 0, right: 100, bottom: 100 };
    match user32_fn(&SYM, c"AdjustWindowRectExForDpi") {
        // SAFETY: `rc` is written in place; the transmute is to
        // `AdjustWindowRectExForDpi(LPRECT, DWORD, BOOL, DWORD, UINT) -> BOOL`.
        Some(f) => unsafe {
            let f: unsafe extern "system" fn(*mut RECT, u32, i32, u32, u32) -> i32 =
                std::mem::transmute(f);
            f(&mut rc, style, 0, 0, dpi);
            -rc.top
        },
        // SAFETY: as above, with the system-DPI entry point.
        None => unsafe {
            AdjustWindowRectEx(&mut rc, style, 0, 0);
            caption::rescale(-rc.top, system_dpi(), dpi)
        },
    }
}

fn is_maximized(hwnd: HWND) -> bool {
    // SAFETY: a live window handle. `GWL_STYLE` is a plain read.
    let style = unsafe { GetWindowLongPtrW(hwnd, GWL_STYLE) } as u32;
    style & WS_MAXIMIZE != 0
}

/// `WM_NCCALCSIZE`, and it is the whole of the arrangement.
///
/// Returns `None` when this window is not extended, which leaves
/// `DefWindowProc` in charge and is stage 17's window exactly.
///
/// # The three lines, in the order they matter
///
/// **The requested window rectangle is read before `DefWindowProc` runs**,
/// because `DefWindowProc` overwrites `rgrc[0]` in place: on entry it is the
/// window rectangle Windows is proposing, on exit it is the client rectangle it
/// decided on. The number this function needs — where the *window* starts — is
/// only available before the call.
///
/// **`DefWindowProc` still runs.** Returning a client rectangle out of nothing
/// would have to reproduce the side and bottom insets, which are the theme's
/// and the DPI's, and would get the maximised case wrong. Letting Windows do
/// the ordinary arithmetic and then moving one edge is the smallest change that
/// can be correct.
///
/// **A maximised window is inset by the border and a restored one is not.** A
/// maximised `WS_OVERLAPPEDWINDOW` is deliberately larger than the work area by
/// the resize border on every side — that is how Windows hides the border off
/// the edge of the screen — so a client rectangle whose top is the window's top
/// starts `SM_CYSIZEFRAME + SM_CXPADDEDBORDER` pixels *above the monitor*. The
/// visible symptom is a tab strip with its first row of pixels cut off, only
/// when maximised, which is exactly the kind of thing a single screenshot of a
/// restored window does not show.
fn nccalcsize(state: &WindowState, hwnd: HWND, wparam: WPARAM, lparam: LPARAM) -> Option<LRESULT> {
    if !state.frame.get().extend_top || wparam == 0 || matches!(state.kind, WindowKind::Popup { .. }) {
        return None;
    }
    let params = lparam as *mut NCCALCSIZE_PARAMS;
    if params.is_null() {
        return None;
    }
    // SAFETY: with `wParam` non-zero the `LPARAM` is an `NCCALCSIZE_PARAMS`
    // owned by the caller and valid for the duration of the message. Read
    // first, then written once, and never aliased — `DefWindowProcW` writes
    // through the same pointer, which is why the read happens before the call.
    unsafe {
        let window_top = (*params).rgrc[0].top;
        let r = DefWindowProcW(hwnd, WM_NCCALCSIZE, wparam, lparam);
        let ordinary_top = (*params).rgrc[0].top;
        let inset = if is_maximized(hwnd) { frame_border(hwnd).1 } else { 0 };
        (*params).rgrc[0].top = window_top + inset;
        state.nc_top.set(ordinary_top - window_top);
        Some(r)
    }
}

/// `WM_NCHITTEST` for a folded caption: the top resize edge, the caption
/// buttons, and the client's tab strip.
///
/// `DefWindowProc` answers first and its answer is kept for everything it still
/// owns — the side and bottom borders, the corners, and `HTNOWHERE` outside the
/// window. Only `HTCLIENT` is looked at again, because that is the answer
/// `WM_NCCALCSIZE` changed: the band at the top of the window used to be
/// non-client.
///
/// **DWM is asked before anything else**, and that is what puts the caption
/// buttons back. `DwmDefWindowProc` answers `HTMINBUTTON`, `HTMAXBUTTON` or
/// `HTCLOSE` for a point over the buttons it drew, and Windows 11 raises the
/// snap-layouts flyout on a hover that hit-tests as `HTMAXBUTTON` — so this one
/// call is the difference between §22.8's documented casualty and a window that
/// snaps like any other. See `dwm_defwindowproc`.
///
/// §82: the rest of the decision is [`caption::hit`], which is pure and unit
/// tested, with this window's DPI-scaled resize band and its button strip —
/// DWM's bounds, or the fallback strip this window draws itself when DWM will
/// not say (`WindowState::button_strip`). Answering the buttons here as well
/// as through DWM is not redundancy: it is the documented way a custom frame
/// keeps snap layouts, and on Wine it is the only way the buttons work at all.
///
/// **No `HTCAPTION`, and this part of §22.8 does still stand.** The compositor
/// cannot know which parts of Firefox's tab strip are draggable, because
/// Wayland has no protocol by which a client publishes its draggable region, so
/// the strip stays `HTCLIENT` and a drag arrives afterwards as
/// `xdg_toplevel.move` (§22.7); a double-click arrives as `set_maximized`. The
/// buttons are a different question and have a different answer only because
/// they are *Windows'* buttons.
fn nchittest(state: &WindowState, hwnd: HWND, wparam: WPARAM, lparam: LPARAM) -> Option<LRESULT> {
    if !state.frame.get().extend_top || matches!(state.kind, WindowKind::Popup { .. }) {
        return None;
    }
    if state.frame.get().caption_buttons
        && let Some(r) = dwm_defwindowproc(hwnd, WM_NCHITTEST, wparam, lparam)
    {
        return Some(r);
    }
    // SAFETY: a live window handle.
    let hit = unsafe { DefWindowProcW(hwnd, WM_NCHITTEST, wparam, lparam) };
    if hit as u32 != HTCLIENT {
        return Some(hit);
    }
    // Screen coordinates, signed: a window can be at a negative x on a
    // left-hand monitor, and the classic bug is an `as u16` that turns -3 into
    // 65533 and puts the hit test on the wrong side of the desktop.
    let x = (lparam & 0xffff) as i16 as i32;
    let y = ((lparam >> 16) & 0xffff) as i16 as i32;
    Some(caption::hit(&state.caption_geometry(), x, y).ht() as LRESULT)
}

/// `DwmExtendFrameIntoClientArea`, resolved rather than linked.
///
/// The same treatment `GetDpiForWindow` gets in `window_geometry`, and for a
/// stronger reason: `dwmapi.dll` is the one library in this file that a
/// non-Windows implementation of Win32 is most likely to answer with a stub, so
/// the return value is reported rather than discarded. A run whose log says the
/// call succeeded and whose picture shows no glass has learned something; a run
/// that never says is a run that has to be repeated.
fn dwm_extend_frame(hwnd: HWND, top: i32) -> Option<i32> {
    static SYM: std::sync::OnceLock<Option<unsafe extern "system" fn() -> isize>> = std::sync::OnceLock::new();
    let f = dwmapi(&SYM, c"DwmExtendFrameIntoClientArea")?;
    let margins = Margins { left: 0, right: 0, top, bottom: 0 };
    // SAFETY: a live window handle and a `MARGINS` that outlives the call. The
    // transmute is to `DwmExtendFrameIntoClientArea(HWND, const MARGINS*)`.
    let hr = unsafe {
        let f: unsafe extern "system" fn(HWND, *const Margins) -> i32 = std::mem::transmute(f);
        f(hwnd, &margins)
    };
    Some(hr)
}

/// `DwmDefWindowProc`, which is how a window with a custom frame keeps
/// **Windows' own** caption buttons rather than drawing three of its own.
///
/// This is the entry point the arrangement turns on. DWM draws the minimise,
/// maximise and close buttons into the frame it is compositing behind the
/// extended strip, and this procedure is what gives them their behaviour: the
/// hover highlight, the pressed state, the click, and — the reason it matters
/// here — `HTMAXBUTTON`, which is what Windows 11 watches for to raise the snap
/// layouts flyout. §22.8 recorded snap layouts as unreachable because *this
/// compositor* cannot know where a maximise button is. It does not have to
/// know: the buttons are Windows', so the hit test is Windows' too.
///
/// Returns `Some(result)` when DWM handled the message, exactly as the C API's
/// `BOOL` out-parameter says, and `None` when the caller must carry on.
fn dwm_defwindowproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> Option<LRESULT> {
    static SYM: std::sync::OnceLock<Option<unsafe extern "system" fn() -> isize>> = std::sync::OnceLock::new();
    let f = dwmapi(&SYM, c"DwmDefWindowProc")?;
    let mut result: LRESULT = 0;
    // SAFETY: a live window handle on its own thread; the transmute is to
    // `DwmDefWindowProc(HWND, UINT, WPARAM, LPARAM, LRESULT*) -> BOOL`, and the
    // out-parameter is written only when the return is non-zero.
    let handled = unsafe {
        let f: unsafe extern "system" fn(HWND, u32, WPARAM, LPARAM, *mut LRESULT) -> i32 =
            std::mem::transmute(f);
        f(hwnd, msg, wparam, lparam, &mut result)
    };
    (handled != 0).then_some(result)
}

/// One entry point out of `dwmapi.dll`, resolved by name.
///
/// Three of them are needed and none is linked, for `window_geometry`'s reason
/// applied harder: `dwmapi.dll` is the library a non-Windows implementation of
/// Win32 is likeliest to answer with a stub, so a missing export has to be a
/// value this code can see rather than a load failure at process start.
fn dwmapi(
    cache: &'static std::sync::OnceLock<Option<unsafe extern "system" fn() -> isize>>,
    symbol: &std::ffi::CStr,
) -> Option<unsafe extern "system" fn() -> isize> {
    // Cached, and that is not premature: `DwmDefWindowProc` is consulted on
    // every mouse message that touches the frame and `DwmGetWindowAttribute`
    // on every paint, so an uncached `LoadLibraryW` + `GetProcAddress` here
    // would be two string lookups per frame of every animation the client runs.
    *cache.get_or_init(|| dwmapi_uncached(symbol))
}

fn dwmapi_uncached(symbol: &std::ffi::CStr) -> Option<unsafe extern "system" fn() -> isize> {
    let name = wide("dwmapi.dll");
    // SAFETY: a NUL-terminated module name; the handle is deliberately not
    // freed, matching `window_geometry`'s treatment of `user32`.
    let dll = unsafe { LoadLibraryW(name.as_ptr()) };
    if dll.is_null() {
        return None;
    }
    // SAFETY: an ASCII, NUL-terminated export name.
    unsafe { GetProcAddress(dll, symbol.as_ptr().cast()) }
}

/// Set one of the Windows 11 frame colours. `None` when DWM has no such export
/// or refuses the attribute, which is every Windows before build 22000.
fn dwm_set_colour(hwnd: HWND, attribute: u32, colour: u32) -> Option<i32> {
    static SYM: std::sync::OnceLock<Option<unsafe extern "system" fn() -> isize>> =
        std::sync::OnceLock::new();
    let f = dwmapi(&SYM, c"DwmSetWindowAttribute")?;
    // SAFETY: a live window handle; the buffer is a `u32` and the size passed
    // is its own. The transmute is to
    // `DwmSetWindowAttribute(HWND, DWORD, LPCVOID, DWORD) -> HRESULT`.
    let hr = unsafe {
        let f: unsafe extern "system" fn(HWND, u32, *const c_void, u32) -> i32 =
            std::mem::transmute(f);
        f(
            hwnd,
            attribute,
            (&raw const colour).cast::<c_void>(),
            std::mem::size_of::<u32>() as u32,
        )
    };
    Some(hr)
}

/// Where Windows drew its caption buttons, in **client** coordinates.
///
/// ADDED FOR TABS IN THE TITLE BAR, and it is the piece that makes this arrangement the
/// Windows one rather than an imitation of it. `DWMWA_CAPTION_BUTTON_BOUNDS`
/// answers in window-relative coordinates; the client origin is not the window
/// origin — the side border is still Windows' — so the answer is translated
/// through `ClientToScreen` rather than assumed.
///
/// `None` when DWM will not say, which is a real case: a window with no caption
/// has no buttons, and an implementation of Win32 that stubs `dwmapi` returns a
/// failing `HRESULT` here. Either way the caller reserves nothing and the
/// client's pixels cover the whole strip, which is the arrangement
/// `--caption-buttons off` asks for on purpose.
fn caption_button_rect(hwnd: HWND) -> Option<RECT> {
    static SYM: std::sync::OnceLock<Option<unsafe extern "system" fn() -> isize>> = std::sync::OnceLock::new();
    let f = dwmapi(&SYM, c"DwmGetWindowAttribute")?;
    let mut r = RECT { left: 0, top: 0, right: 0, bottom: 0 };
    // SAFETY: the transmute is to
    // `DwmGetWindowAttribute(HWND, DWORD, PVOID, DWORD) -> HRESULT`; the buffer
    // is a `RECT` and the size passed is its own.
    let hr = unsafe {
        let f: unsafe extern "system" fn(HWND, u32, *mut c_void, u32) -> i32 =
            std::mem::transmute(f);
        f(
            hwnd,
            DWMWA_CAPTION_BUTTON_BOUNDS,
            (&raw mut r).cast::<c_void>(),
            std::mem::size_of::<RECT>() as u32,
        )
    };
    if hr != 0 || r.right <= r.left || r.bottom <= r.top {
        return None;
    }
    let mut wr = RECT { left: 0, top: 0, right: 0, bottom: 0 };
    let mut origin = POINT { x: 0, y: 0 };
    // SAFETY: live window handle; both are written in place.
    unsafe {
        GetWindowRect(hwnd, &mut wr);
        ClientToScreen(hwnd, &mut origin);
    }
    let dx = wr.left - origin.x;
    let dy = wr.top - origin.y;
    Some(RECT {
        left: r.left + dx,
        // Up to the top of the client area, whatever DWM said. The bounds it
        // reports start below the window's top border, and a strip of the
        // client's own pixels left above the buttons is a one-pixel line of
        // tab strip over the close button, which looks like a rendering fault.
        top: 0,
        right: r.right + dx,
        bottom: r.bottom + dy,
    })
}

/// Tell an existing window its frame has changed, and extend it if it should be.
///
/// `SWP_FRAMECHANGED` is the only way to make Windows ask `WM_NCCALCSIZE` again
/// for a window that is not being moved or sized. Without it the fold happens
/// on whatever resize comes next, which on a client that maps at its final size
/// is never — a window that looks like stage 17's until the user grabs a
/// corner, and then silently becomes right.
fn apply_frame(w: &WindowState) {
    if !frame_glass(w) {
        return;
    }
    // SAFETY: a live window handle on its own thread. Sends `WM_NCCALCSIZE`
    // and `WM_WINDOWPOSCHANGED` to `toplevel_proc` before returning, which is
    // sound here for the reason this module's header gives: no `&mut` to
    // per-window state exists.
    unsafe {
        SetWindowPos(
            w.hwnd(),
            std::ptr::null_mut(),
            0,
            0,
            0,
            0,
            SWP_FRAMECHANGED | SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE,
        );
    }
}

/// The DWM half of the fold, without the `SetWindowPos` that goes with it.
///
/// ADDED FOR §46, and splitting it out is a bug fix rather than tidying.
/// [`apply_frame`] is right for a window that is **already on screen** — §31
/// calls it at create and on a DPI change, and the extra `SetWindowPos` is the
/// only way to make Windows ask `WM_NCCALCSIZE` again for a window that is not
/// otherwise moving.
///
/// At map it is wrong twice over. `map_window` is *about* to call
/// `SetWindowPos` with the window's real size, so a second one is redundant;
/// and it lands **outside** the `self_driven` guard that call sets, so the
/// `WM_SIZE` it provokes goes back to the Wayland thread as a spurious
/// `xdg_toplevel.configure` — at map, before the window has been shown, at a
/// size derived from a window that has not been sized yet.
///
/// Measured: with the extra call, `wc-input.sh wine` was **flaky** — one run in
/// three or so came out with the client having decoded nothing at all, because
/// `weston-eventdemo` was reallocating for a configure it should never have
/// been sent while the harness was injecting into it. That is the shape of
/// every reentrancy bug this module's header warns about, and the fix is to
/// have one frame recalculation instead of two rather than to guard the second.
fn frame_glass(w: &WindowState) -> bool {
    if !w.frame.get().extend_top || matches!(w.kind, WindowKind::Popup { .. }) {
        return false;
    }
    let inset = top_inset(w.hwnd());
    let dwm = if w.frame.get().dwm_glass {
        dwm_extend_frame(w.hwnd(), inset)
    } else {
        None
    };
    println!(
        "wc-compositor: window {} folds {inset} px of caption into its client area{}",
        w.id,
        match dwm {
            // docs/waycompose-design.md §67.4, and the one Wine branch in this file. §31.7 named this
            // exact hole — "a run that extends the frame and finds `dwmapi`
            // stubbed would look exactly like `noglass.png` and would report
            // success ... the one failure mode of this arrangement with no
            // diagnostic" — and left it open because the `HRESULT` cannot
            // close it.
            //
            // **Measured, wine-11.14 staging, 2026-08-13.** With a plain
            // `WS_OVERLAPPEDWINDOW` and nothing else in the process:
            // `DwmIsCompositionEnabled` returns `S_OK` with `enabled = 1`,
            // `DwmExtendFrameIntoClientArea` returns `S_OK`, and
            // `DwmGetWindowAttribute(DWMWA_CAPTION_BUTTON_BOUNDS)` returns
            // `0x80004001` — `E_NOTIMPL` — leaving the `RECT` untouched. So two
            // of the three answer success and the only one that has to return
            // *data* admits it is a stub. There is no composition behind this
            // frame and `S_OK` says there is.
            //
            // Nothing about the window changes here: the branch corrects the
            // claim, not the behaviour, because `caption_button_rect` already
            // returns `None` under Wine and the fallback that produces is the
            // arrangement `--caption-buttons off` asks for on purpose. What was
            // broken was the evidence, and §35 is a whole section about gates
            // that reported success while something was wrong.
            Some(0) if crate::wine::under_wine() =>
                ", DwmExtendFrameIntoClientArea returned ok -- but this dwmapi is Wine's stub \
                 (DwmGetWindowAttribute is E_NOTIMPL) and nothing is composited behind the \
                 frame; see docs/waycompose-design.md §67.4",
            Some(0) => ", DwmExtendFrameIntoClientArea ok",
            Some(_) => ", DwmExtendFrameIntoClientArea FAILED",
            None if w.frame.get().dwm_glass => ", dwmapi.dll has no DwmExtendFrameIntoClientArea",
            None => ", without DWM glass (--dwm-glass off)",
        }
    );
    true
}

/// Hand a gesture the client started over to Windows' own modal loop.
///
/// ADDED FOR STAGE 17, and it is the whole of `xdg_toplevel.move` and
/// `xdg_toplevel.resize` on this platform.
///
/// `ReleaseCapture` first, and it is not optional: the client's drag began with
/// a `WM_LBUTTONDOWN` that `toplevel_proc` answered with `SetCapture`, and
/// `DefWindowProc` refuses to start a sizing or moving loop for a window that
/// holds the capture. The symptom is a window that simply does not move, with
/// nothing in any log.
///
/// # §82: a move is `SC_MOVE | HTCAPTION`, with the cursor in `lParam`
///
/// Until §82 a move was posted as bare `SC_MOVE` with `lParam = 0`, and that
/// is the **keyboard** move — Alt+Space, *Move* — which warps the cursor to
/// the middle of the caption and moves the window by the distance from
/// *there*. Nothing had dragged a window before §82, so nothing had seen it.
/// Measured under Wine with `wc-desk mouse --drag 800,15 920,95` on Firefox's
/// empty tab strip: the window moved by **(+286, +82)** for a drag of
/// (+120, +80) — the 166 px of x it gained is the distance from the press to
/// the middle of the 1288 px window. `SC_MOVE | HTCAPTION` (`0xF012`) is the
/// mouse move a caption click produces, and it follows the pointer from where
/// it is. Resizes already passed a `WMSZ_*` edge in the low bits, which is the
/// mouse form, and are unchanged.
/// §85. Put every mapped popup owned by window `owner` back at its offset in
/// the owner's client area, after the owner moved.
///
/// The popup's own `WM_WINDOWPOSCHANGED` does the same for *its* popups, so a
/// submenu follows its menu without this recursing. The list is copied out of
/// the registry before any `SetWindowPos`, which reenters this procedure.
fn follow_owner(ui: &Rc<Ui>, owner: WindowId, owner_hwnd: HWND) {
    let popups: Vec<Rc<WindowState>> = ui
        .windows
        .borrow()
        .values()
        .filter(|w| matches!(w.kind, WindowKind::Popup { parent, .. } if parent == owner))
        .filter(|w| w.mapped.get())
        .cloned()
        .collect();
    for p in popups {
        let (x, y) = p.popup_offset.get();
        let mut origin = POINT { x, y };
        let mut now = RECT { left: 0, top: 0, right: 0, bottom: 0 };
        // SAFETY: live handles on the thread that owns them; `ClientToScreen`
        // and `GetWindowRect` write only the structures they are given.
        unsafe {
            ClientToScreen(owner_hwnd, &mut origin);
            if GetWindowRect(p.hwnd(), &mut now) != 0 && (now.left, now.top) == (origin.x, origin.y) {
                continue;
            }
            SetWindowPos(
                p.hwnd(),
                std::ptr::null_mut(),
                origin.x,
                origin.y,
                0,
                0,
                SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE,
            );
        }
    }
}

fn begin_sysmenu_gesture(w: &Rc<WindowState>, command: u32, what: &str) {
    const HTCAPTION: u32 = 2;
    let (command, lparam) = if command == SC_MOVE {
        let mut pt = POINT { x: 0, y: 0 };
        // SAFETY: `pt` is written in place.
        unsafe { GetCursorPos(&mut pt) };
        (
            SC_MOVE | HTCAPTION,
            ((pt.y as u32 & 0xffff) << 16 | (pt.x as u32 & 0xffff)) as LPARAM,
        )
    } else {
        (command, 0)
    };
    // SAFETY: a live window handle, on the thread that created it.
    // `ReleaseCapture` is a no-op when this thread holds no capture.
    unsafe {
        ReleaseCapture();
        PostMessageW(w.hwnd(), WM_SYSCOMMAND, command as usize, lparam);
    }
    // §85. The modal loop eats the `WM_LBUTTONUP`, so the count `on_button`
    // keeps would stay at one for the rest of the window's life and the next
    // press would not take the capture. The compositor ends the press on its
    // side for the same reason (`State::hand_pointer_to_platform`).
    w.buttons_down.set(0);
    println!("wc-compositor: window {} handed a {what} to Windows", w.id);
}

/// Show the window for the first time, at the size the client chose.
/// `DefWindowProc` for a message it would answer by repainting the caption,
/// with `WS_VISIBLE` cleared for the call so that it paints nothing. §91.
///
/// The style bit is cleared with `SetWindowLongPtrW` and put back the same
/// way, with no `SetWindowPos`, so the window is never actually hidden: the
/// default handler only reads the bit to decide whether to draw. This is the
/// `_HandleSetTextOrIcon` of WPF's `WindowChrome`, which keeps DWM's caption
/// buttons over a client area extended into the frame, as this window does.
fn defproc_without_caption_paint(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    // SAFETY: a live window handle on its own thread; only the style word is
    // touched, and it is restored before returning.
    unsafe {
        let style = GetWindowLongPtrW(hwnd, GWL_STYLE);
        let visible = style & WS_VISIBLE as isize != 0;
        if visible {
            SetWindowLongPtrW(hwnd, GWL_STYLE, style & !(WS_VISIBLE as isize));
        }
        let r = DefWindowProcW(hwnd, msg, wparam, lparam);
        if visible {
            // Re-read: the default handler may have changed other bits.
            let now = GetWindowLongPtrW(hwnd, GWL_STYLE);
            SetWindowLongPtrW(hwnd, GWL_STYLE, now | WS_VISIBLE as isize);
        }
        r
    }
}

impl WindowState {
    /// Re-clear the caption-button strip on the next `WM_PAINT`, whatever drew
    /// over it. §91: one small paint per title change.
    fn invalidate_button_strip(&self) {
        if let Some((mut rc, _)) = self.button_strip() {
            rc.bottom = rc.bottom.max(self.nc_top.get());
            // SAFETY: a live window; the rectangle is read, not written.
            unsafe { InvalidateRect(self.hwnd(), &rc, 0) };
        }
    }
}

/// The outer (frame-included) size of a toplevel whose client area is
/// `width` x `height`, with this window's live style and caption fold.
fn outer_size(w: &WindowState, width: i32, height: i32) -> (i32, i32) {
    let mut rc = RECT { left: 0, top: 0, right: width, bottom: height };
    // SAFETY: a live window handle; `rc` is written in place.
    unsafe {
        let style = GetWindowLongPtrW(w.hwnd(), GWL_STYLE) as u32;
        AdjustWindowRectEx(&mut rc, style, 0, 0);
    }
    // See `map_window`: a folded caption gives the whole top inset back to the
    // client, so the window is not taller than its client by a caption.
    if w.frame.get().extend_top {
        rc.top = 0;
    }
    (rc.right - rc.left, rc.bottom - rc.top)
}

fn client_size(hwnd: HWND) -> (i32, i32) {
    let mut cr = RECT { left: 0, top: 0, right: 0, bottom: 0 };
    // SAFETY: a live window handle; written in place.
    unsafe { GetClientRect(hwnd, &mut cr) };
    (cr.right - cr.left, cr.bottom - cr.top)
}

/// After this process has placed a toplevel (`placing` was set across it),
/// decide what the client hears: nothing if it got the size it committed, and
/// one configure with the size it did get if not. §91.
fn settle_placement(w: &WindowState, width: i32, height: i32) {
    let got = client_size(w.hwnd());
    if got == (width, height) || got.0 <= 0 || got.1 <= 0 {
        w.last_configured.set((width, height));
        return;
    }
    // **Larger is not news.** Windows will not make an overlapped window
    // smaller than its minimum track size (166x47 at 125%), and Firefox's first
    // commit on the guest is a 1x1 geometry. Telling it the minimum took the
    // size away from it, and it stayed 148x38 (§91.6, measured on the guest).
    // A window the client is still sizing just shows its content in a larger
    // frame until the next commit, which `follow_client_size` then follows.
    // Only a placement smaller than the commit (the work area) must be told.
    if got.0 >= width && got.1 >= height {
        w.last_configured.set(got);
        return;
    }
    w.last_configured.set(got);
    let _ = w.events.send(FromUi::Resized { id: w.id, width: got.0, height: got.1 });
}

/// §91. The client changed its own size while it is the one choosing it (it
/// has never been sent a size), so the window follows, as a native window
/// follows its own `SetWindowPos`. Not while the user is dragging a border,
/// nor while the window is maximised or minimised, whose size is Windows'.
fn follow_client_size(w: &Rc<WindowState>, width: i32, height: i32) {
    if !w.mapped.get() || w.user_sizing.get() || matches!(w.kind, WindowKind::Popup { .. }) {
        return;
    }
    // SAFETY: live window handle; plain queries.
    if unsafe { IsZoomed(w.hwnd()) != 0 || IsIconic(w.hwnd()) != 0 } {
        return;
    }
    if client_size(w.hwnd()) == (width, height) {
        w.last_configured.set((width, height));
        return;
    }
    let (ow, oh) = outer_size(w, width, height);
    let (ox, oy) = window_origin(w.hwnd());
    let mut got = (ox, oy, ow, oh);
    if let Some(work) = work_area_of(w.hwnd()) {
        got = crate::platform::fit_to_work_area(got, (work.left, work.top, work.right, work.bottom));
    }
    w.placing.set(true);
    w.self_driven.set(true);
    // SAFETY: live window handle; `WM_SIZE` arrives synchronously and is held
    // back by `placing`.
    unsafe {
        SetWindowPos(
            w.hwnd(),
            std::ptr::null_mut(),
            got.0,
            got.1,
            got.2,
            got.3,
            SWP_NOZORDER | SWP_NOACTIVATE,
        );
    }
    w.self_driven.set(false);
    w.placing.set(false);
    settle_placement(w, width, height);
    println!(
        "wc-compositor: window {} follows its client to {width}x{height} (window {}x{} at {},{})",
        w.id, got.2, got.3, got.0, got.1
    );
}

fn map_window(w: &Rc<WindowState>, width: i32, height: i32, fold_caption: bool) {
    // §46, and it has to be here rather than at creation: the fact it is
    // derived from is `xdg_surface.set_window_geometry`, which a toplevel sends
    // after it exists. Here is still early enough — the window has never been
    // shown, so the `WM_NCCALCSIZE` below is the first one it ever answers and
    // no frame is recalculated under a user.
    //
    // A popup is never folded. It has no caption to fold and `apply_frame`
    // already refuses one; setting the flag anyway would make every reader
    // below need the popup test that three of them already have.
    if !matches!(w.kind, WindowKind::Popup { .. }) {
        let mut frame = w.frame.get();
        if frame.extend_top != fold_caption {
            frame.extend_top = fold_caption;
            w.frame.set(frame);
            println!(
                "wc-compositor: window {} frame: {} the caption",
                w.id,
                if fold_caption { "folding" } else { "keeping" }
            );
            // The DWM call only. `apply_frame`'s `SetWindowPos` must **not**
            // happen here: `map_window` is about to make one with the window's
            // real size, and a second one lands outside the `self_driven` guard
            // and sends the client a configure it should never receive. See
            // [`frame_glass`] for the measurement that says so.
            //
            // `SWP_FRAMECHANGED` is added to that later call instead, so the
            // fold and the sizing are one recalculation.
            frame_glass(w);
        }
    }
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
            // STAGE 13: the bitmap **before** the window is shown. A layered
            // window that has never been given one is not merely blank, it is
            // undefined — and `ShowWindow` on it is a flash of whatever DWM had
            // in that memory. `update_layered` is also what makes the window
            // visible at all in the layered model, so a popup whose shadow is
            // still empty is deliberately left hidden until it has pixels,
            // which is the same rule the toplevel path already follows.
            let painted = update_layered(w);
            if painted && !w.mapped.replace(true) {
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

    let mut rc = RECT {
        left: 0,
        top: 0,
        right: width,
        bottom: height,
    };
    // The Wayland size is the *content* size, which is Win32's *client* size —
    // the frame is Windows' and is not the client's business. So the outer
    // window has to be grown by whatever the current theme's border and caption
    // come to, which is what `AdjustWindowRectEx` is for and is why hard-coding
    // `GetSystemMetrics(SM_CYCAPTION)` would be wrong on a themed desktop.
    //
    // STAGE 38: the window's **live** style rather than the constant
    // `create_window` used. The two are the same today and would silently stop
    // being the same the moment a window is given a different frame — §31's
    // tabs-in-title-bar arrangement is exactly that — and the failure would be
    // a non-client inset that is wrong by a caption's height, which reads as a
    // window that is slightly too tall for no reason.
    // SAFETY: `rc` is written in place; a live window handle.
    let style = unsafe { GetWindowLongPtrW(w.hwnd(), GWL_STYLE) } as u32;
    unsafe {
        AdjustWindowRectEx(&mut rc, style, 0, 0);
    }

    // ADDED FOR TABS IN THE TITLE BAR, and it is one line because `AdjustWindowRectEx` has
    // already measured the thing that has to be undone. Its `rc.top` is the
    // negative of the top inset the theme wants — caption, buttons and top
    // border — and `WM_NCCALCSIZE` is about to give every pixel of it back to
    // the client area. Growing the window by it *and* handing it back would
    // make the window taller than the client asked for by exactly a caption,
    // once per map, which reads as a client that will not settle on a size.
    if w.frame.get().extend_top {
        rc.top = 0;
    }

    // STAGE 38: fit the window to the monitor's work area before it is shown.
    //
    // Two requirements, and the second is the sharper one:
    //
    //   * a window must not open larger than the work area, so it does not
    //     spill behind the taskbar or off the screen;
    //   * a window must open with its **title bar completely visible**,
    //     because a caption that is off the top or past an edge cannot be
    //     grabbed, and a window that cannot be grabbed cannot be moved back.
    //
    // Fitting the *whole* outer rectangle inside `rcWork` satisfies both, and
    // it is a stronger guarantee than "the caption is visible": there is no
    // arrangement in which the frame fits and the caption does not.
    //
    // `CW_USEDEFAULT` chose where this window is (`create_window`), and that
    // cascade is not wrong — it is simply unbounded, and a few windows into it
    // the next one is off the bottom right. So the position is *bounded* here
    // rather than replaced: a window that already fits is not moved at all.
    //
    // §91: **and it is read after `ShowWindow`, which is where it is decided.**
    // Read before, on a hidden `CW_USEDEFAULT` window, it was a placeholder that
    // the fit below clamped to the work area's corner, and the `SetWindowPos`
    // after `ShowWindow` then moved the window there over the cascade Windows
    // had just chosen: §90.7's second window, at 0,0.
    //
    // **`placing` covers both calls, and the client hears once, afterwards.**
    // `ShowWindow` on a `CW_USEDEFAULT` window sizes it to the cascade's default
    // and sends that `WM_SIZE`; the `SetWindowPos` sends the committed size.
    // Each went out as a configure, so the client was told the cascade size and
    // then its own size back — and a configure with a size is an instruction:
    // Firefox's second window committed 252x304 before reading its saved
    // geometry, was configured to 252x304, and obeyed for the rest of its life
    // (§90.7). This is the "configured twice at map" noted below, found. Now a
    // client that gets what it committed is told nothing, which leaves it free
    // to choose again (`follow_client_size`), and one the work area shrank is
    // told the size it got.
    // SAFETY: live window handle; both calls send messages to `toplevel_proc`
    // on this thread before returning.
    // §91: **a client that asked to be maximised before its first frame is
    // shown maximised**, rather than placed. Firefox opens a new window
    // maximised when the window it came from is (`set_maximized` before the
    // first commit), and `set_window_state` on a window nobody had shown yet
    // maximised a window with no pixels, which this function's `SW_SHOWNORMAL`
    // then restored: §90.7's second window, at its pre-layout 252x304. The
    // `WM_SIZE` that `SW_SHOWMAXIMIZED` sends is the size the client must lay
    // out at, so `placing` is off for it; `self_driven` holds back only the
    // echo of the state the client itself asked for.
    // §102: a first map that succeeds a closed window of the same identity
    // takes its place, maximised if it was.
    let succeeded = if w.mapped.get() {
        None
    } else {
        SUCCESSION.with(|s| {
            let found = super::succession_for(
                s.borrow().as_ref(),
                &w.identity.borrow(),
                std::time::Instant::now(),
            );
            if found.is_some() {
                s.replace(None);
            }
            found
        })
    };
    if let Some((origin, max)) = succeeded {
        println!(
            "wc-compositor: window {} takes the place of the last {} window to close: {},{}{}",
            w.id,
            w.identity.borrow(),
            origin.0,
            origin.1,
            if max { ", maximised" } else { "" }
        );
    }
    let maximize = !w.mapped.get()
        && (w.maximize_at_map.replace(false) || succeeded.is_some_and(|(_, max)| max));
    if maximize {
        w.mapped.set(true);
        // SAFETY: live window handle; the messages arrive synchronously.
        unsafe {
            if w.frame.get().extend_top {
                w.placing.set(true);
                SetWindowPos(
                    w.hwnd(),
                    std::ptr::null_mut(),
                    0,
                    0,
                    0,
                    0,
                    SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE | SWP_FRAMECHANGED,
                );
                w.placing.set(false);
            }
            w.self_driven.set(true);
            ShowWindow(w.hwnd(), SW_SHOWMAXIMIZED);
            w.self_driven.set(false);
        }
        println!(
            "wc-compositor: window {} shown maximised, as its client asked before its first frame",
            w.id
        );
    } else {
        w.placing.set(true);
        w.self_driven.set(true);
        if !w.mapped.replace(true) {
            // Prevent painting CW_USEDEFAULT's larger initial rectangle, which
            // leaves stale desktop pixels after resizing under Wine. Keep the
            // default position until ShowWindow chooses Windows' cascade.
            let (mut ww, mut wh) = (rc.right - rc.left, rc.bottom - rc.top);
            if let Some(work) = work_area_of(w.hwnd()) {
                ww = ww.min(work.right - work.left);
                wh = wh.min(work.bottom - work.top);
            }
            // SAFETY: a live hidden window; placing/self_driven suppress the
            // intermediate size echo. Frame recalculation remains below.
            unsafe {
                SetWindowPos(
                    w.hwnd(),
                    std::ptr::null_mut(),
                    0,
                    0,
                    ww,
                    wh,
                    SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE,
                );
                ShowWindow(w.hwnd(), SW_SHOWNORMAL);
            }
        }
        let (ox, oy) = succeeded.map_or_else(|| window_origin(w.hwnd()), |(origin, _)| origin);
        let asked = (ox, oy, rc.right - rc.left, rc.bottom - rc.top);
        let mut got = asked;
        if let Some(work) = work_area_of(w.hwnd()) {
            got = crate::platform::fit_to_work_area(
                asked,
                (work.left, work.top, work.right, work.bottom),
            );
            if got != asked {
                println!(
                    "wc-compositor: window {} asked for {}x{} at {},{} and the work area is \
                     {}x{} at {},{} -> {}x{} at {},{}",
                    w.id,
                    asked.2,
                    asked.3,
                    asked.0,
                    asked.1,
                    work.right - work.left,
                    work.bottom - work.top,
                    work.left,
                    work.top,
                    got.2,
                    got.3,
                    got.0,
                    got.1
                );
            }
        }
        let (x, y, ww, wh) = got;
        // SAFETY: as above.
        unsafe {
            SetWindowPos(
                w.hwnd(),
                std::ptr::null_mut(),
                x,
                y,
                ww,
                wh,
                // §46: `SWP_FRAMECHANGED` when this window's caption is folded, so
                // that this final `SetWindowPos` both places the window and asks
                // `WM_NCCALCSIZE` again. `frame_glass` above did the DWM half and
                // deliberately no `SetWindowPos` of its own — two of them at map is
                // what made the Wine keyboard leg flaky.
                SWP_NOZORDER
                    | SWP_NOACTIVATE
                    | if w.frame.get().extend_top { SWP_FRAMECHANGED } else { 0 },
            );
        }
        w.self_driven.set(false);
        w.placing.set(false);
        settle_placement(w, width, height);
        // What the window actually turned out to be, once, in the log — because
        // the placement above is the one thing here that Windows is entitled to
        // overrule, and a line that says so is the difference between finding that
        // out in a minute and finding it out in a run.
        {
            let mut rc = RECT { left: 0, top: 0, right: 0, bottom: 0 };
            // SAFETY: a live window handle; the rectangle is written in place.
            unsafe { GetWindowRect(w.hwnd(), &mut rc) };
            if (rc.right - rc.left, rc.bottom - rc.top, rc.left, rc.top) != (ww, wh, x, y) {
                println!(
                    "wc-compositor: window {} was placed at {ww}x{wh} at {x},{y} and Windows made \
                     it {}x{} at {},{}",
                    w.id,
                    rc.right - rc.left,
                    rc.bottom - rc.top,
                    rc.left,
                    rc.top
                );
            }
        }
    
    }
    // TABS IN THE TITLE BAR, and after `ShowWindow` on purpose: DWM has no caption buttons
    // to report the bounds of until the window has been shown, and asking in
    // `apply_frame` — which runs at create, before the window exists on a
    // screen — answers `None` on a window that will have them a moment later.
    if w.frame.get().extend_top {
        match (w.frame.get().caption_buttons, caption_button_rect(w.hwnd())) {
            (true, Some(rc)) => println!(
                "wc-compositor: window {} keeps Windows' own caption buttons: \
                 client rect {},{} {}x{} left transparent for DWM",
                w.id,
                rc.left,
                rc.top,
                rc.right - rc.left,
                rc.bottom - rc.top
            ),
            (true, None) => println!(
                "wc-compositor: window {} asked for Windows' caption buttons and DWM \
                 would not say where they are: the client's pixels cover the whole caption",
                w.id
            ),
            (false, _) => println!(
                "wc-compositor: window {} has no caption buttons (--caption-buttons off): \
                 the client's pixels cover the whole caption",
                w.id
            ),
        }
    }

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
        "wc-compositor: window {} mapped {width}x{height}{}{}",
        w.id,
        if w.frame.get().extend_top {
            format!(
                " (tabs in the title bar: {} px of caption is the client's)",
                w.nc_top.get()
            )
        } else {
            String::new()
        },
        if alpha {
            " (argb: alpha ignored by GDI, drawn opaque)"
        } else {
            " (xrgb)"
        }
    );

    // What the window turned out to be, in the units the window manager uses.
    // docs/waycompose-design.md §8.11 item 5 asks what happens on a scaled display, and the
    // answer is three numbers rather than a yes: the DPI Windows assigned this
    // window, the outer rectangle it gave us for the client size we asked for,
    // and the client rectangle it ended up with. On a 150% desktop a virtualised
    // window reports 96 here and a frame whose non-client thickness is the
    // 100%-scale one; an aware window reports 144 and a frame 1.5x as thick.
    // Printed once per window, at map, because that is when the frame exists and
    // the size is the client's own rather than a drag's.
    let (dpi, wr, cr) = window_geometry(w.hwnd());
    println!(
        "wc-compositor: window {} dpi {dpi} ({}%) window {}x{} client {}x{} nonclient {}x{}",
        w.id,
        dpi * 100 / 96,
        wr.0,
        wr.1,
        cr.0,
        cr.1,
        wr.0 - cr.0,
        wr.1 - cr.1
    );
}

/// The work area of the monitor this window is on, in **screen** coordinates.
///
/// ADDED FOR STAGE 38, and the first thing in this project that asks Windows
/// where anything is. Three choices, each of which can be got wrong silently:
///
/// **`MonitorFromWindow`, not the primary monitor.** `GetSystemMetrics` with
/// `SM_CXSCREEN` is shorter and describes the *primary* display, so on a
/// two-monitor desktop every window on the second screen would be pushed back
/// towards the first — a bug that is invisible on the machine most people test
/// on.
///
/// **`MONITOR_DEFAULTTONEAREST`.** A window dragged mostly off the desktop
/// belongs to the monitor it is nearest; `MONITOR_DEFAULTTONULL` would answer
/// nothing and every caller here would then decline to do anything, which is
/// the pre-stage-38 behaviour arriving by accident.
///
/// **`rcWork`, not `rcMonitor`.** The work area is the monitor minus the
/// taskbar and any other appbar. It is the number Windows' own menus and its
/// own window placement respect, and using `rcMonitor` would put the bottom of
/// every maximised-ish window and every long menu behind the taskbar.
fn work_area_of(hwnd: HWND) -> Option<RECT> {
    let mut info = MONITORINFO {
        cbSize: std::mem::size_of::<MONITORINFO>() as u32,
        rcMonitor: RECT { left: 0, top: 0, right: 0, bottom: 0 },
        rcWork: RECT { left: 0, top: 0, right: 0, bottom: 0 },
        dwFlags: 0,
    };
    // SAFETY: a live window handle. `cbSize` is set, which `GetMonitorInfoW`
    // requires and whose absence is the documented way it fails.
    unsafe {
        let monitor = MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST);
        if monitor.is_null() || GetMonitorInfoW(monitor, &mut info) == 0 {
            return None;
        }
    }
    if info.rcWork.right <= info.rcWork.left || info.rcWork.bottom <= info.rcWork.top {
        return None;
    }
    Some(info.rcWork)
}

/// Where a window's outer rectangle starts, in screen coordinates.
fn window_origin(hwnd: HWND) -> (i32, i32) {
    let mut rc = RECT { left: 0, top: 0, right: 0, bottom: 0 };
    // SAFETY: a live window handle; the rectangle is written in place.
    unsafe {
        GetWindowRect(hwnd, &mut rc);
    }
    (rc.left, rc.top)
}

/// `(dpi, (window w, h), (client w, h))` for a live window.
///
/// `GetDpiForWindow` is a Windows 10 1607 entry point and is resolved rather
/// than linked, for the reason in `announce_dpi_awareness`. A 0 means the symbol
/// was not there, which is a different thing from 96 and is printed as such.
fn window_geometry(hwnd: HWND) -> (u32, (i32, i32), (i32, i32)) {
    let lib = wide("user32.dll");
    // SAFETY: a live NUL-terminated name for an already-loaded module.
    let user32 = unsafe { LoadLibraryW(lib.as_ptr()) };
    let dpi = if user32.is_null() {
        0
    } else {
        // SAFETY: an ASCII, NUL-terminated symbol name; the transmute is to
        // `GetDpiForWindow(HWND) -> UINT`.
        match unsafe { GetProcAddress(user32, c"GetDpiForWindow".as_ptr().cast()) } {
            Some(f) => unsafe {
                let f: unsafe extern "system" fn(HWND) -> u32 = std::mem::transmute(f);
                f(hwnd)
            },
            None => 0,
        }
    };
    let mut wr = RECT { left: 0, top: 0, right: 0, bottom: 0 };
    let mut cr = RECT { left: 0, top: 0, right: 0, bottom: 0 };
    // SAFETY: live window handle; both rectangles are written in place.
    unsafe {
        GetWindowRect(hwnd, &mut wr);
        GetClientRect(hwnd, &mut cr);
    }
    (
        dpi,
        (wr.right - wr.left, wr.bottom - wr.top),
        (cr.right - cr.left, cr.bottom - cr.top),
    )
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
    // §104's follow-up: a window drawn from its client's buffer is dumped from
    // that buffer, which is what it shows.
    let dibs = w.dibs.borrow();
    let written = match w.direct_on.get().and_then(|k| dibs.iter().find(|d| d.key == k)) {
        Some(d) => {
            let len = (d.height as usize - 1) * d.stride as usize + d.width as usize * 4;
            // SAFETY: the DIB's bits are `stride * height` bytes and live as
            // long as `d`, which the borrow holds.
            let bytes = unsafe { std::slice::from_raw_parts(d.bits, len) };
            crate::surface::write_ppm_bgra(&path, d.width, d.height, d.stride, bytes)
        }
        None => {
            let shadow = w.shadow.lock().unwrap_or_else(|e| e.into_inner());
            crate::surface::write_ppm(&path, &shadow)
        }
    };
    match written {
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

/// Put the shadow on a layered window, alpha and all.
///
/// ADDED FOR STAGE 13, and it is the fix for a menu with a black border.
///
/// # What was wrong
///
/// `SetDIBitsToDevice` on a 32-bit `BI_RGB` DIB ignores the fourth byte —
/// `map_window` has said so in every log since stage 3 (*"argb: alpha ignored
/// by GDI, drawn opaque"*). For an opaque toplevel that costs nothing. For a
/// GTK menu it is the whole appearance: the surface is `ARGB8888` and its
/// outer margin is the client's drop shadow, fully transparent, so `A=0` pixels
/// are drawn with whatever `R`, `G` and `B` happen to be — which for a cleared
/// buffer is **black**. The rounded corners are the same pixels and the same
/// black. It reads as a rendering bug in the client and it is our blit.
///
/// # Why a layered window rather than `AlphaBlend`
///
/// `AlphaBlend` composites the source against *what is already in the target
/// DC*, and what is in a window's DC is that window's own background. It cannot
/// show the desktop behind the window because the window is opaque to the
/// system. Per-pixel transparency of a **window** is a window-manager property
/// on Win32, and `WS_EX_LAYERED` plus `UpdateLayeredWindow` is the only
/// mechanism for it: the bitmap becomes the window's shape and DWM composites
/// it against whatever is behind.
///
/// # What it costs, and why only popups get it
///
/// `UpdateLayeredWindow` replaces `WM_PAINT` entirely and takes the **whole**
/// window every time — damage cannot be used, so a layered window redraws
/// everything on every commit. A menu is small and changes rarely. A toplevel
/// is neither, and a layered toplevel would also lose its native frame, since
/// the bitmap becomes the entire window and there is no non-client area left
/// for Windows to draw a caption in. That is the opposite of what
/// `docs/goal-firefox.md` asks for, so toplevels keep `SetDIBitsToDevice` and
/// keep Windows' frame.
///
/// The toplevel's own transparent margin — GTK's shadow inset — is dealt with
/// at the other end instead: `default_mode(Server)` (see `state.rs`) makes
/// Firefox stop drawing one at all, which removes the pixels rather than
/// compositing them.
///
/// # The alpha convention
///
/// `AC_SRC_ALPHA` is defined on **premultiplied** alpha, which is what
/// `wl_shm`'s `ARGB8888` already is, so the bytes go across untouched. An
/// `XRGB8888` surface has no alpha at all and its fourth byte is undefined;
/// those are forced to 255, because a shaped window built from undefined bytes
/// is a window with holes in it that move.
fn update_layered(w: &Rc<WindowState>) -> bool {
    use windows_sys::Win32::Graphics::Gdi::{
        CreateCompatibleDC, CreateDIBSection, DeleteDC, GetDC, ReleaseDC, SelectObject,
    };

    let (pixels, width, height, opaque) = {
        let shadow = w.shadow.lock().unwrap_or_else(|e| e.into_inner());
        if shadow.is_empty() || shadow.width() <= 0 || shadow.height() <= 0 {
            return false;
        }
        (
            shadow.pixels().to_vec(),
            shadow.width(),
            shadow.height(),
            !shadow.format().has_alpha(),
        )
    };
    let need = (width as usize) * (height as usize) * 4;
    if pixels.len() < need {
        return false;
    }

    let mut bmi: BITMAPINFO = unsafe { std::mem::zeroed() };
    bmi.bmiHeader = BITMAPINFOHEADER {
        biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
        biWidth: width,
        biHeight: -height,
        biPlanes: 1,
        biBitCount: 32,
        biCompression: BI_RGB,
        biSizeImage: (width as u32) * (height as u32) * 4,
        biXPelsPerMeter: 0,
        biYPelsPerMeter: 0,
        biClrUsed: 0,
        biClrImportant: 0,
    };

    // SAFETY: every handle below is checked before use and every one that is
    // created is released on all paths.
    unsafe {
        let screen = GetDC(std::ptr::null_mut());
        if screen.is_null() {
            return false;
        }
        let mem = CreateCompatibleDC(screen);
        let mut bits: *mut c_void = std::ptr::null_mut();
        let dib = CreateDIBSection(
            screen,
            &bmi,
            DIB_RGB_COLORS,
            &mut bits,
            std::ptr::null_mut(),
            0,
        );
        if mem.is_null() || dib.is_null() || bits.is_null() {
            if !dib.is_null() {
                DeleteObject(dib as _);
            }
            if !mem.is_null() {
                DeleteDC(mem);
            }
            ReleaseDC(std::ptr::null_mut(), screen);
            return false;
        }
        let dst = std::slice::from_raw_parts_mut(bits.cast::<u8>(), need);
        dst.copy_from_slice(&pixels[..need]);
        if opaque {
            for px in dst.chunks_exact_mut(4) {
                px[3] = 0xff;
            }
        }
        let old = SelectObject(mem, dib as _);

        let size = SIZE {
            cx: width,
            cy: height,
        };
        let src = POINT { x: 0, y: 0 };
        let blend = BLENDFUNCTION {
            BlendOp: AC_SRC_OVER as u8,
            BlendFlags: 0,
            SourceConstantAlpha: 255,
            AlphaFormat: AC_SRC_ALPHA as u8,
        };
        // A null destination point keeps the window where it is, which is what
        // `map_window` and `SetWindowPos` already decided; passing one here
        // would make this a second authority on the window's position.
        let ok = UpdateLayeredWindow(
            w.hwnd(),
            screen,
            std::ptr::null(),
            &size,
            mem,
            &src,
            0,
            &blend,
            ULW_ALPHA,
        ) != 0;

        SelectObject(mem, old);
        DeleteObject(dib as _);
        DeleteDC(mem);
        ReleaseDC(std::ptr::null_mut(), screen);
        ok
    }
}

/// Whether this window's pixels go through [`update_layered`].
/// Whether a *locked* modifier is currently on.
///
/// ADDED FOR STAGE 13. **The low bit, not the high bit.** `GetKeyState`
/// returns the physical up/down of the key in bit 15 and the toggle state in
/// bit 0, and for Caps Lock it is the toggle that matters: the key is up
/// almost all the time and the lock is on regardless. Reading the sign of the
/// return value — the reflex, because that is how every other key is tested —
/// reports the lock as off unless the user is holding the key down at the
/// instant of the call.
fn lock_on(vk: u16) -> bool {
    // SAFETY: a virtual-key constant; the call has no pointer arguments.
    unsafe { GetKeyState(vk as i32) & 1 != 0 }
}

fn is_layered(w: &Rc<WindowState>) -> bool {
    matches!(w.kind, WindowKind::Popup { .. })
}

/// §104's follow-up: a DIB section over one client buffer, selected into a
/// memory DC, which is what `WindowState::blit_direct` draws from.
///
/// # Why a DIB section over the client's section is sound
///
/// `CreateDIBSection` with a section handle makes a bitmap whose bits *are*
/// the section's bytes at `offset`: GDI maps the object itself, and nothing is
/// copied. The client may not write a buffer between committing it and its
/// `wl_buffer.release`, and the Wayland side releases a buffer drawn this way
/// only after this thread has said (`FromUi::DirectShown`) that it draws from
/// a newer one, so every `BitBlt` from it reads a finished frame. The bitmap
/// is the *source* of a blit and is never drawn into.
///
/// A DIB's rows are its width times four bytes, DWORD-aligned; the client's
/// stride may be larger (Intel's LINEAR images pad rows), so the bitmap is
/// `stride / 4` pixels wide and only the client's `width` columns are ever
/// blitted. `offset` must be a multiple of four, which the Wayland side checks.
struct DirectDib {
    key: u64,
    dc: HDC,
    bitmap: windows_sys::Win32::Graphics::Gdi::HBITMAP,
    old: windows_sys::Win32::Graphics::Gdi::HGDIOBJ,
    /// The bitmap's bits, for `dump_frame`.
    bits: *const u8,
    width: i32,
    height: i32,
    stride: i32,
}

impl DirectDib {
    fn new(frame: &super::DirectFrame) -> Result<DirectDib, String> {
        use windows_sys::Win32::Graphics::Gdi::{CreateCompatibleDC, CreateDIBSection, SelectObject};
        let mut bmi: BITMAPINFO = unsafe { std::mem::zeroed() };
        bmi.bmiHeader = BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: frame.stride / 4,
            biHeight: -frame.height,
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB,
            biSizeImage: (frame.stride as u32) * (frame.height as u32),
            biXPelsPerMeter: 0,
            biYPelsPerMeter: 0,
            biClrUsed: 0,
            biClrImportant: 0,
        };
        let mut bits: *mut c_void = std::ptr::null_mut();
        // SAFETY: `bmi` is a complete 32-bit header; the section handle is
        // live for the call (the `Arc` in `frame`), and GDI holds its own
        // reference to the object from here on. No DC is needed for
        // `DIB_RGB_COLORS`.
        let bitmap = unsafe {
            CreateDIBSection(
                std::ptr::null_mut(),
                &bmi,
                DIB_RGB_COLORS,
                &mut bits,
                frame.section.raw(),
                frame.offset,
            )
        };
        if bitmap.is_null() || bits.is_null() {
            return Err(format!("CreateDIBSection failed: {}", std::io::Error::last_os_error()));
        }
        // SAFETY: a memory DC compatible with the screen; the bitmap is ours.
        let dc = unsafe { CreateCompatibleDC(std::ptr::null_mut()) };
        if dc.is_null() {
            // SAFETY: the bitmap was just made and is selected nowhere.
            unsafe { DeleteObject(bitmap as _) };
            return Err(format!("CreateCompatibleDC failed: {}", std::io::Error::last_os_error()));
        }
        // SAFETY: both live; the old object is put back before either goes.
        let old = unsafe { SelectObject(dc, bitmap as _) };
        Ok(DirectDib {
            key: frame.key,
            dc,
            bitmap,
            old,
            bits: bits.cast_const().cast(),
            width: frame.width,
            height: frame.height,
            stride: frame.stride,
        })
    }
}

impl Drop for DirectDib {
    fn drop(&mut self) {
        use windows_sys::Win32::Graphics::Gdi::{DeleteDC, SelectObject};
        // SAFETY: made in `new` and released once, the bitmap deselected
        // before it is deleted.
        unsafe {
            SelectObject(self.dc, self.old);
            DeleteDC(self.dc);
            DeleteObject(self.bitmap as _);
        }
    }
}

/// How many buffers' DIBs a window keeps: KWin's swapchain is two or three,
/// and a buffer the client stopped using ages out.
const DIBS_KEPT: usize = 4;

/// `ToUi::Direct`: draw `w` from this buffer from now on, or from its shadow
/// again; then say so, which is what lets the Wayland side release the buffer
/// drawn from before.
fn direct(ui: &Rc<Ui>, w: &Rc<WindowState>, frame: Option<super::DirectFrame>, damage: &[Rect], serial: u64) {
    // A window going away is told with no damage (`State::drop_direct`), and
    // its shadow was never brought up to date: there is no frame to dump.
    let shows = frame.is_some() || !damage.is_empty();
    let ok = match frame {
        Some(frame) => {
            let have = w.dibs.borrow().iter().any(|d| d.key == frame.key);
            let made = if have { Ok(()) } else { DirectDib::new(&frame).map(|d| w.dibs.borrow_mut().push(d)) };
            match made {
                Ok(()) => {
                    w.direct_on.set(Some(frame.key));
                    let mut dibs = w.dibs.borrow_mut();
                    while dibs.len() > DIBS_KEPT {
                        let Some(i) = dibs.iter().position(|d| d.key != frame.key) else { break };
                        dibs.remove(i);
                    }
                    drop(dibs);
                    present(w, damage);
                    true
                }
                Err(e) => {
                    eprintln!("wc-compositor: window {} cannot be drawn from its client's buffer: {e}", w.id);
                    false
                }
            }
        }
        None => {
            w.direct_on.set(None);
            w.dibs.borrow_mut().clear();
            present(w, damage);
            true
        }
    };
    if ok && shows {
        dump_frame(ui, w);
    }
    let _ = w.events.send(FromUi::DirectShown { id: w.id, serial, ok });
}

fn present(w: &Rc<WindowState>, damage: &[Rect]) {
    if !w.mapped.get() {
        return;
    }
    if w.unpainted_since.get().is_none() && sv_transport::frametrace::enabled() {
        w.unpainted_since.set(Some(std::time::Instant::now()));
    }
    // STAGE 13. A layered window has no `WM_PAINT`: its content is whatever
    // was last handed to `UpdateLayeredWindow`, so invalidating a rectangle
    // would do nothing at all and the menu would keep its first frame forever.
    if is_layered(w) {
        update_layered(w);
        return;
    }
    for r in damage {
        let rc = RECT {
            left: r.x,
            top: r.y,
            right: r.x + r.w,
            bottom: r.y + r.h,
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
    // §95: and the caption-button strip with every frame, so whatever painted
    // over it outside `WM_PAINT` (see `WM_NCPAINT` in `toplevel_proc`) lasts
    // until the client's next frame at most. 146x31 pixels at 100%: one small
    // `SetDIBitsToDevice` and one `FillRect` per frame, inside a paint that is
    // happening anyway.
    if !damage.is_empty() {
        w.invalidate_button_strip();
    }
}

/// §96. `xdg_toplevel.set_fullscreen` and `unset_fullscreen`: the borderless
/// window over the whole monitor that every Windows game calls fullscreen.
///
/// **Not exclusive fullscreen**, which does not exist for a window that is
/// drawn with `SetDIBitsToDevice`; this is the "borderless fullscreen" Windows
/// itself has used for games since DWM made the two indistinguishable. The
/// caption and the frame go (`WS_POPUP`), the window covers `rcMonitor` —
/// the monitor, not its work area, so the taskbar is under it — and the size
/// reaches the client through the `WM_SIZE` that `SetWindowPos` sends, as a
/// configure carrying the fullscreen state the Wayland side has already set.
/// Leaving puts back the style and the placement it had.
///
/// **The placement, not the rectangle.** The first version saved
/// `GetWindowRect` and moved the window over the monitor with `SetWindowPos`,
/// which is only what the window looks like *now*. Windows keeps a second
/// answer, `WINDOWPLACEMENT`: the normal rectangle and whether the window is
/// maximised, and a restore from the taskbar uses that one. GLFW minimises a
/// fullscreen window when it loses focus (`GLFW_AUTO_ICONIFY`, on Wayland as
/// on Win32), so a fullscreen game is restored from the taskbar all the time.
/// On the guest, a game made fullscreen from a maximised window came back
/// maximised: a 1296x768 borderless window at -8,-8, 8 px cut off at the left
/// and top and the taskbar showing below it. One made fullscreen from a normal
/// window would have come back at its windowed size. So for as long as the
/// window is fullscreen its normal placement is the monitor
/// ([`fullscreen_placement`]), which also takes it out of the maximised state,
/// and the saved placement goes back on the way out, maximised or not.
fn set_fullscreen(w: &Rc<WindowState>, on: bool) {
    let hwnd = w.hwnd();
    if on == w.fullscreen.get().is_some() {
        return;
    }
    if on {
        let mut info = MONITORINFO {
            cbSize: std::mem::size_of::<MONITORINFO>() as u32,
            rcMonitor: RECT { left: 0, top: 0, right: 0, bottom: 0 },
            rcWork: RECT { left: 0, top: 0, right: 0, bottom: 0 },
            dwFlags: 0,
        };
        // SAFETY: a plain C struct, for which all-zero is a valid value;
        // `length` is set before the call that reads it.
        let mut was: WINDOWPLACEMENT = unsafe { std::mem::zeroed() };
        was.length = std::mem::size_of::<WINDOWPLACEMENT>() as u32;
        // SAFETY: a live window handle on its own thread; every out-parameter
        // is written in place, and `cbSize` and `length` are set.
        unsafe {
            let monitor = MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST);
            if monitor.is_null() || GetMonitorInfoW(monitor, &mut info) == 0 {
                println!("wc-compositor: window {} cannot go fullscreen: no monitor", w.id);
                return;
            }
            if GetWindowPlacement(hwnd, &mut was) == 0 {
                println!("wc-compositor: window {} cannot go fullscreen: no placement", w.id);
                return;
            }
            let style = GetWindowLongPtrW(hwnd, GWL_STYLE);
            w.fullscreen.set(Some((style, was)));
            let bare = (style as u32 & !WS_OVERLAPPEDWINDOW) | WS_POPUP;
            SetWindowLongPtrW(hwnd, GWL_STYLE, bare as isize);
            let ex = GetWindowLongPtrW(hwnd, GWL_EXSTYLE) as u32;
            let full = fullscreen_placement(&was, info.rcMonitor, info.rcWork, ex);
            SetWindowPlacement(hwnd, &full);
            let m = info.rcMonitor;
            SetWindowPos(
                hwnd,
                std::ptr::null_mut(),
                m.left,
                m.top,
                m.right - m.left,
                m.bottom - m.top,
                SWP_FRAMECHANGED | SWP_NOZORDER,
            );
        }
        println!(
            "wc-compositor: window {} fullscreen on its monitor ({}x{} at {},{}){}",
            w.id,
            info.rcMonitor.right - info.rcMonitor.left,
            info.rcMonitor.bottom - info.rcMonitor.top,
            info.rcMonitor.left,
            info.rcMonitor.top,
            if was.showCmd == SW_SHOWMAXIMIZED as u32 { ", from maximised" } else { "" }
        );
    } else if let Some((style, mut was)) = w.fullscreen.take() {
        // SAFETY: a live window handle; the style and placement are the ones
        // read from this window when it went fullscreen.
        unsafe {
            SetWindowLongPtrW(hwnd, GWL_STYLE, style);
            // A minimised window stays minimised: leaving fullscreen while on
            // the taskbar must not pop the window up behind the user's back.
            was.showCmd = show_cmd(super::fullscreen::leave_show(
                show_of(was.showCmd),
                IsIconic(hwnd) != 0,
            ));
            SetWindowPlacement(hwnd, &was);
            SetWindowPos(
                hwnd,
                std::ptr::null_mut(),
                0,
                0,
                0,
                0,
                SWP_FRAMECHANGED | SWP_NOZORDER | SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
            );
        }
        println!(
            "wc-compositor: window {} left fullscreen{}",
            w.id,
            if was.showCmd == SW_SHOWMAXIMIZED as u32 { ", maximised again" } else { "" }
        );
    }
}

/// §96: the placement a fullscreen window keeps, decided by
/// `platform::fullscreen` (pure, tested on every target): the saved one with
/// the monitor as its normal rectangle, normal rather than maximised, and no
/// `WPF_RESTORETOMAXIMIZED`.
fn fullscreen_placement(
    was: &WINDOWPLACEMENT,
    monitor: RECT,
    work: RECT,
    ex_style: u32,
) -> WINDOWPLACEMENT {
    use super::fullscreen as fs;
    let r = |r: RECT| fs::Rect { left: r.left, top: r.top, right: r.right, bottom: r.bottom };
    let n = fs::normal_rect(r(monitor), r(work), ex_style & WS_EX_TOOLWINDOW != 0);
    let mut p = *was;
    p.rcNormalPosition = RECT { left: n.left, top: n.top, right: n.right, bottom: n.bottom };
    p.showCmd = show_cmd(fs::fullscreen_show(show_of(was.showCmd)));
    p.flags &= !WPF_RESTORETOMAXIMIZED;
    p
}

fn show_of(cmd: u32) -> super::fullscreen::Show {
    use super::fullscreen::Show;
    if cmd == SW_SHOWMAXIMIZED as u32 {
        Show::Maximized
    } else if cmd == SW_SHOWMINIMIZED as u32 {
        Show::Minimized
    } else {
        Show::Normal
    }
}

fn show_cmd(show: super::fullscreen::Show) -> u32 {
    use super::fullscreen::Show;
    (match show {
        Show::Normal => SW_SHOWNORMAL,
        Show::Maximized => SW_SHOWMAXIMIZED,
        Show::Minimized => SW_SHOWMINIMIZED,
    }) as u32
}

fn set_window_state(w: &Rc<WindowState>, maximized: Option<bool>, minimized: Option<bool>) {
    // §91: a window that has not been shown has nothing to maximise yet.
    // `ShowWindow(SW_MAXIMIZE)` here would show it with no pixels, and the map
    // would then restore it. The request is kept for `map_window`.
    if !w.mapped.get() {
        if let Some(m) = maximized {
            w.maximize_at_map.set(m);
        }
        return;
    }
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

// ---------------------------------------------------------------------------
// Taskbar identity, stage 32
// ---------------------------------------------------------------------------
//
// WHY THERE IS COM IN A FILE WHOSE MANIFEST SAYS THERE IS NONE
//
// Every window this process makes is made by *this process*, and the shell
// groups taskbar buttons by process unless a window says otherwise. Saying
// otherwise is one property on one window — `PKEY_AppUserModel_ID` — and the
// only way to set a property on an `HWND` is `SHGetPropertyStoreForWindow`,
// which returns an `IPropertyStore`. There is no flat entry point for it.
//
// The process-wide alternative, `SetCurrentProcessExplicitAppUserModelID`, *is*
// a flat entry point and is exactly the wrong shape: it gives the whole process
// one identity, which is the state we are already in and the defect being
// fixed.
//
// windows-sys 0.61 generates no COM interfaces at all — only functions,
// structs and constants — so the vtable below is declared by hand from
// `propsys.h`. That is eight function pointers in a fixed order and two GUIDs;
// getting the order wrong is a call through the wrong slot, which is why the
// declaration is a transcription with the header's own comments kept.

/// `IID_IPropertyStore`, from `propsys.h`.
const IID_PROPERTY_STORE: GUID = GUID::from_u128(0x886d8eeb_8cf2_4446_8d02_cdba1dbdcf99);

/// The `fmtid` every `PKEY_AppUserModel_*` shares, from `propkey.h`.
const FMTID_APP_USER_MODEL: GUID = GUID::from_u128(0x9f4c2855_9f79_4b39_a8d0_e1d42de1d5f3);

/// `PKEY_AppUserModel_ID`: the string the shell groups taskbar buttons by.
const PKEY_APP_USER_MODEL_ID: PROPERTYKEY = PROPERTYKEY {
    fmtid: FMTID_APP_USER_MODEL,
    pid: 5,
};

/// `PKEY_AppUserModel_PreventPinning`: no "Pin to taskbar" on this button.
///
/// **Set unless the launcher has said what a pin should run** (§92). Pinning
/// writes a shortcut whose target is what the shell can work out about the
/// window, and relaunching it would start `sv-guest.exe`, not the Wayland
/// client the button appears to be. The application is a process on another
/// machine, and this compositor has no command line that would bring it back.
/// The launcher that started it does: it writes a [`crate::taskbar::Relaunch`]
/// per application into `--relaunch-dir`, and a window whose identity has one
/// gets the three properties below instead of this one. A window without one
/// still cannot be pinned, because a pin that silently does the wrong thing is
/// worse than an absent affordance.
const PKEY_PREVENT_PINNING: PROPERTYKEY = PROPERTYKEY {
    fmtid: FMTID_APP_USER_MODEL,
    pid: 9,
};

/// `PKEY_AppUserModel_RelaunchCommand`: what a pin of this window runs. §92.
const PKEY_RELAUNCH_COMMAND: PROPERTYKEY = PROPERTYKEY {
    fmtid: FMTID_APP_USER_MODEL,
    pid: 2,
};

/// `PKEY_AppUserModel_RelaunchIconResource`: `path,index` of the pin's icon.
const PKEY_RELAUNCH_ICON: PROPERTYKEY = PROPERTYKEY {
    fmtid: FMTID_APP_USER_MODEL,
    pid: 3,
};

/// `PKEY_AppUserModel_RelaunchDisplayNameResource`: the pin's name. Windows
/// ignores a relaunch command that has none.
const PKEY_RELAUNCH_NAME: PROPERTYKEY = PROPERTYKEY {
    fmtid: FMTID_APP_USER_MODEL,
    pid: 4,
};

/// `VT_LPWSTR`.
const VT_LPWSTR: u16 = 31;
/// `VT_BOOL`.
const VT_BOOL: u16 = 11;
/// `VARIANT_TRUE`, which is `-1` and not `1`.
const VARIANT_TRUE: i16 = -1;

/// `PROPVARIANT`, as much of it as two property types need.
///
/// The real union is sixteen bytes after an eight-byte header. Only the first
/// eight of those are ever written here — a `LPWSTR` or a `VARIANT_BOOL` — and
/// the rest is zero, which is what an unset `PROPVARIANT` looks like anyway.
/// The size assertion is the check that this agrees with the ABI; it is written
/// for a 64-bit target on purpose, because a 32-bit build would need a
/// different one and should fail loudly rather than pass a mis-shaped struct to
/// the shell.
#[repr(C)]
#[derive(Clone, Copy)]
struct PropVariant {
    vt: u16,
    reserved: [u16; 3],
    value: [u64; 2],
}

const _: () = assert!(std::mem::size_of::<PropVariant>() == 24);

impl PropVariant {
    const fn empty() -> Self {
        PropVariant { vt: 0, reserved: [0; 3], value: [0; 2] }
    }

    /// A `VT_LPWSTR` **borrowing** `s`.
    ///
    /// Borrowing is sound because `IPropertyStore::SetValue` copies the value
    /// it is given (it is a store, not a sink) and because this variant is
    /// never handed to `PropVariantClear`. So the `Vec<u16>` behind `s` owns
    /// the memory, frees it on drop, and no allocator crosses the boundary in
    /// either direction.
    fn lpwstr(s: *const u16) -> Self {
        PropVariant { vt: VT_LPWSTR, reserved: [0; 3], value: [s as u64, 0] }
    }

    fn boolean(v: bool) -> Self {
        let raw = if v { VARIANT_TRUE } else { 0 } as u16;
        PropVariant { vt: VT_BOOL, reserved: [0; 3], value: [u64::from(raw), 0] }
    }

    /// The string out of a variant a `GetValue` filled in, and the shell's
    /// allocation with it.
    ///
    /// Consuming, because the pointer is freed here: `PROPVARIANT` strings come
    /// from `CoTaskMemAlloc`, which is exactly what `PropVariantClear` would
    /// free, and taking the one case this file can produce is cheaper than
    /// pulling in the whole of `Win32_System_Com_StructuredStorage` for it.
    fn take_string(self) -> Option<String> {
        if self.vt != VT_LPWSTR || self.value[0] == 0 {
            return None;
        }
        let p = self.value[0] as *mut u16;
        let mut n = 0usize;
        // SAFETY: a NUL-terminated UTF-16 string from the property store.
        unsafe {
            while *p.add(n) != 0 {
                n += 1;
            }
            let s = String::from_utf16_lossy(std::slice::from_raw_parts(p, n));
            CoTaskMemFree(p.cast());
            Some(s)
        }
    }
}

#[repr(C)]
struct PropertyStore {
    vtbl: *const PropertyStoreVtbl,
}

/// `IPropertyStoreVtbl`, transcribed from `propsys.h` in its own order.
#[repr(C)]
struct PropertyStoreVtbl {
    // IUnknown
    query_interface:
        unsafe extern "system" fn(*mut PropertyStore, *const GUID, *mut *mut c_void) -> HRESULT,
    add_ref: unsafe extern "system" fn(*mut PropertyStore) -> u32,
    release: unsafe extern "system" fn(*mut PropertyStore) -> u32,
    // IPropertyStore
    get_count: unsafe extern "system" fn(*mut PropertyStore, *mut u32) -> HRESULT,
    get_at: unsafe extern "system" fn(*mut PropertyStore, u32, *mut PROPERTYKEY) -> HRESULT,
    get_value:
        unsafe extern "system" fn(*mut PropertyStore, *const PROPERTYKEY, *mut PropVariant)
            -> HRESULT,
    set_value:
        unsafe extern "system" fn(*mut PropertyStore, *const PROPERTYKEY, *const PropVariant)
            -> HRESULT,
    commit: unsafe extern "system" fn(*mut PropertyStore) -> HRESULT,
}

thread_local! {
    /// Whether this thread has called `CoInitializeEx`.
    static COM_READY: Cell<bool> = const { Cell::new(false) };
}

/// Put this thread in a single-threaded apartment, once.
///
/// Lazy rather than at startup so that a compositor which never makes a window
/// — the headless and the `--probe` paths — never initialises COM at all, and
/// so that the HRESULT is logged next to the first thing that needed it.
/// Apartment-threaded because this is a thread with a message loop, which is
/// what an STA is; `RPC_E_CHANGED_MODE` (0x80010106) would mean somebody had
/// already made it multi-threaded, and is reported rather than treated as
/// fatal because the property store works either way.
///
/// There is no `CoUninitialize`. This thread runs until the process exits.
fn com_init() {
    COM_READY.with(|ready| {
        if ready.replace(true) {
            return;
        }
        // SAFETY: a plain call with a null reserved argument, on the thread
        // that will use the apartment.
        // `COINIT_*` is generated as a signed `COINIT`; the entry point takes
        // an unsigned flag word. The cast is the whole of the difference.
        let hr = unsafe { CoInitializeEx(std::ptr::null(), COINIT_APARTMENTTHREADED as u32) };
        println!("wc-compositor: COM apartment on the UI thread: hr 0x{:08x}", hr as u32);
    });
}

/// Which taskbar button this window belongs on.
///
/// Returns the identity the property store reads back, which is a stronger
/// claim than "`SetValue` returned `S_OK`" and costs one more call.
///
/// # Ordering, which is the part that is easy to get wrong
///
/// The shell creates a taskbar button when the window is **shown** and reads
/// this property then. Setting it afterwards leaves the button where it was.
/// So every call site here is before a `ShowWindow` — at creation, and at map
/// just before `map_window` — and the one path that cannot be, a `set_app_id`
/// that arrives after the window is up, rebuilds the button by hiding and
/// re-showing it (see the `ToUi::AppId` arm).
fn set_taskbar_identity(w: &Rc<WindowState>, aumid: &str) -> Result<String, (&'static str, HRESULT)> {
    com_init();
    let mut store: *mut PropertyStore = std::ptr::null_mut();
    // SAFETY: a live window handle; the out pointer is written only on success
    // and is checked below.
    let hr = unsafe {
        SHGetPropertyStoreForWindow(
            w.hwnd(),
            &IID_PROPERTY_STORE,
            (&mut store as *mut *mut PropertyStore).cast(),
        )
    };
    if hr < 0 || store.is_null() {
        return Err(("SHGetPropertyStoreForWindow", hr));
    }
    let id = wide(aumid);
    // SAFETY: `store` is a live `IPropertyStore` from the call above and is
    // released on every path out. `id` outlives every use of the borrowed
    // pointer inside it, because `SetValue` copies before returning.
    unsafe {
        let vtbl = &*(*store).vtbl;
        let mut result = Ok(String::new());
        let hr = (vtbl.set_value)(store, &PKEY_APP_USER_MODEL_ID, &PropVariant::lpwstr(id.as_ptr()));
        if hr < 0 {
            result = Err(("SetValue(PKEY_AppUserModel_ID)", hr));
        }
        // §92. Pinnable when the launcher said what a pin runs, and not
        // otherwise. Every key is written on every stamp, so a window that
        // moves from one identity to another cannot keep the old one's
        // relaunch command: an absent value is written as `VT_EMPTY`, which
        // removes it.
        let relaunch = crate::taskbar::relaunch_for(aumid);
        let wides = relaunch.as_ref().map(|r| (wide(&r.command), wide(&r.name), wide(&r.icon)));
        if result.is_ok() {
            let keys: [(&PROPERTYKEY, PropVariant, &'static str); 4] = match &wides {
                Some((command, name, icon)) => [
                    (&PKEY_RELAUNCH_COMMAND, PropVariant::lpwstr(command.as_ptr()), "RelaunchCommand"),
                    (&PKEY_RELAUNCH_NAME, PropVariant::lpwstr(name.as_ptr()), "RelaunchDisplayNameResource"),
                    (
                        &PKEY_RELAUNCH_ICON,
                        if icon.len() > 1 { PropVariant::lpwstr(icon.as_ptr()) } else { PropVariant::empty() },
                        "RelaunchIconResource",
                    ),
                    (&PKEY_PREVENT_PINNING, PropVariant::boolean(false), "PreventPinning"),
                ],
                None => [
                    (&PKEY_RELAUNCH_COMMAND, PropVariant::empty(), "RelaunchCommand"),
                    (&PKEY_RELAUNCH_NAME, PropVariant::empty(), "RelaunchDisplayNameResource"),
                    (&PKEY_RELAUNCH_ICON, PropVariant::empty(), "RelaunchIconResource"),
                    (&PKEY_PREVENT_PINNING, PropVariant::boolean(true), "PreventPinning"),
                ],
            };
            for (key, value, what) in keys {
                let hr = (vtbl.set_value)(store, key, &value);
                if hr < 0 {
                    eprintln!(
                        "wc-compositor: window {} SetValue(PKEY_AppUserModel_{what}) returned 0x{:08x}",
                        w.id, hr as u32
                    );
                    if what == "PreventPinning" || relaunch.is_some() {
                        result = Err(("SetValue(a PKEY_AppUserModel_Relaunch* key)", hr));
                        break;
                    }
                }
            }
            match &relaunch {
                Some(r) => println!(
                    "wc-compositor: window {} can be pinned: relaunch {:?} as {:?}, icon {:?}",
                    w.id, r.command, r.name, r.icon
                ),
                None => println!(
                    "wc-compositor: window {} cannot be pinned: no relaunch file for {aumid:?}",
                    w.id
                ),
            }
        }
        if result.is_ok() {
            let hr = (vtbl.commit)(store);
            if hr < 0 {
                result = Err(("Commit", hr));
            }
        }
        // The read-back. What the store answers is what the shell will read,
        // and it is the difference between "the call succeeded" and "the
        // property is there" — a distinction §16 paid for once already, where
        // both `WM_SETICON`s succeeded and the taskbar showed neither.
        if result.is_ok() {
            let mut pv = PropVariant::empty();
            let hr = (vtbl.get_value)(store, &PKEY_APP_USER_MODEL_ID, &mut pv);
            result = if hr < 0 {
                Err(("GetValue(PKEY_AppUserModel_ID)", hr))
            } else {
                Ok(pv.take_string().unwrap_or_default())
            };
        }
        (vtbl.release)(store);
        result
    }
}

/// Stamp an identity on a window, and say what happened.
///
/// `rebuild` is for the late case: the window is already on screen, so the
/// button exists and the property alone will not move it. Hiding and re-showing
/// is what makes the shell drop the button and make a new one; it is done only
/// when the identity actually changed, because it costs a flicker and — since
/// hiding the foreground window gives the focus away — a deactivation the
/// client can see.
fn apply_identity(w: &Rc<WindowState>, aumid: &str) {
    if aumid.is_empty() {
        // A popup. `WS_EX_TOOLWINDOW` with an owner has no taskbar button at
        // all, so there is nothing to group and nothing to say.
        return;
    }
    w.identity.replace(aumid.to_string());
    if *w.aumid.borrow() == aumid {
        return;
    }
    let rebuild = w.mapped.get();
    match set_taskbar_identity(w, aumid) {
        Ok(read_back) => {
            w.aumid.replace(aumid.to_string());
            if read_back == aumid {
                println!("wc-compositor: window {} taskbar identity <- {aumid:?}", w.id);
            } else {
                // The store took something other than what it was given. Not
                // seen, and said out loud if it ever is, because a truncated or
                // rejected identity is a window quietly back on the shared
                // button.
                println!(
                    "wc-compositor: window {} taskbar identity <- {aumid:?} but the store \
                     reads back {read_back:?}",
                    w.id
                );
            }
        }
        Err((call, hr)) => {
            eprintln!(
                "wc-compositor: window {} could not take the taskbar identity {aumid:?}: \
                 {call} returned 0x{:08x}",
                w.id, hr as u32
            );
            return;
        }
    }
    if rebuild {
        // SAFETY: a live window handle on its own thread. Both calls reenter
        // `toplevel_proc` before returning, which is what `self_driven` is for:
        // the `WM_SIZE` this provokes is ours and is not a user action.
        //
        // `SW_SHOWNA` rather than `SW_SHOWNORMAL`: hiding the window has
        // already cost it the focus, and taking the foreground back would be a
        // window stealing activation for a reason the user cannot see.
        w.self_driven.set(true);
        unsafe {
            ShowWindow(w.hwnd(), SW_HIDE);
            ShowWindow(w.hwnd(), SW_SHOWNA);
        }
        w.self_driven.set(false);
        println!(
            "wc-compositor: window {} was already on screen, so its taskbar button was \
             rebuilt under the new identity",
            w.id
        );
    }
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
/// # Two slots, two sizes, one set
///
/// `WM_SETICON` is sent twice because Windows keeps two icons per window and
/// shows them in different places: `ICON_SMALL` is the caption and the Alt-Tab
/// row (`SM_CXSMICON`, 16 px at 96 dpi), `ICON_BIG` is the taskbar button and
/// the switcher (`SM_CXICON`, 32). Sending one and leaving the other unset
/// leaves the taskbar on the generic icon while the caption looks right, which
/// is a confusing half-success and was worth the second call from the start.
///
/// Which image fills which slot is [`crate::icon::best_match`], on the Wayland
/// side, because it is arithmetic and this file is not testable on the machine
/// that runs `cargo test`.
///
/// An empty set clears both slots. That is the path a client with no icon takes
/// and it must reach the *generic* icon rather than a blank button, which is
/// what destroying the `HICON`s and never setting a new one does.
fn set_icon(w: &Rc<WindowState>, icons: &[IconImage]) {
    release_icons(w);
    if icons.is_empty() {
        // Both slots are already back to 0, and a window with no icon of its
        // own gets its class icon — which is `IDI_APPLICATION`, the generic
        // one stage 8 photographed. Said out loud because "no icon" and "the
        // icon failed to build" produce the same picture and should not
        // produce the same log.
        println!("wc-compositor: window {} has no icon; the taskbar keeps the generic one", w.id);
        return;
    }
    let mut set = 0;
    for (which, slot) in [(ICON_BIG, &w.icon_big), (ICON_SMALL, &w.icon_small)] {
        let (tw, th) = if which == ICON_BIG {
            // SAFETY: pure queries.
            unsafe { (GetSystemMetrics(SM_CXICON), GetSystemMetrics(SM_CYICON)) }
        } else {
            unsafe { (GetSystemMetrics(SM_CXSMICON), GetSystemMetrics(SM_CYSMICON)) }
        };
        let (tw, th) = (tw.max(1), th.max(1));
        let Some(icon) = crate::icon::best_match(icons, tw.max(th)) else {
            continue;
        };
        let Some(hicon) = make_icon(icon, tw, th) else {
            eprintln!(
                "wc-compositor: window {} could not build a {tw}x{th} HICON from {}x{} pixels",
                w.id, icon.width, icon.height
            );
            continue;
        };
        slot.set(hicon);
        // SAFETY: live window; `WM_SETICON` takes the handle as `lParam` and
        // does not take ownership of it.
        unsafe {
            SendMessageW(w.hwnd(), WM_SETICON, which as WPARAM, hicon);
        }
        set += 1;
        println!(
            "wc-compositor: window {} icon {} <- {}x{} pixels scaled to {tw}x{th}",
            w.id,
            if which == ICON_BIG { "big" } else { "small" },
            icon.width,
            icon.height
        );
    }
    println!(
        "wc-compositor: window {} icon set: {} of 2 slots from {} image(s) [{}]",
        w.id,
        set,
        icons.len(),
        icons
            .iter()
            .map(|i| format!("{}x{}", i.width, i.height))
            .collect::<Vec<_>>()
            .join(" ")
    );
}

/// Destroy the `HICON`s this window is holding, if any.
///
/// `WM_SETICON` does not take ownership — the window borrows the handle and the
/// process still owns it — so every icon this file creates has to be destroyed
/// here exactly once, both when it is replaced and when the window goes.
fn release_icons(w: &Rc<WindowState>) {
    for slot in [&w.icon_big, &w.icon_small] {
        let old = slot.replace(0);
        if old != 0 {
            // SAFETY: `old` came from `CreateIconIndirect` and is destroyed
            // exactly once, because the slot is cleared in the same breath.
            unsafe {
                DestroyIcon(old as _);
            }
            ICONS_FREED.with(|c| c.set(c.get() + 1));
        }
    }
}

/// Build an `HICON` from BGRA pixels, nearest-neighbour scaled to `tw` x `th`.
///
/// Nearest-neighbour because this is a fallback path for an icon that arrived
/// at the wrong size; the right answer is for whatever resolves the app-id to
/// ship the sizes Windows asks for, which is what an icon theme has, and
/// `crate::icon::best_match` has already picked the closest of them.
///
/// **The alpha is premultiplied on arrival and is not touched here.** That is
/// `wl_shm`'s ARGB8888 convention, `wc_icon_manager_v1` requires the same, and
/// it is what a 32-bit `HICON` needs: the shell composites icons with
/// `AlphaBlend`, which is defined on premultiplied colour. A straight-alpha
/// source would show as a pale halo, not as a failure.
fn make_icon(icon: &IconImage, tw: i32, th: i32) -> Option<isize> {
    make_hicon(icon, tw, th, None)
}

/// The body of [`make_icon`], and of [`make_cursor`].
///
/// SPLIT OUT FOR STAGE 12. An icon and a cursor are the same `ICONINFO` with
/// `fIcon` flipped and a hotspot filled in — Win32 models them as one object
/// and `HICON` and `HCURSOR` are the same handle type — so writing the DIB
/// twice would have been two places for the premultiplied-alpha convention to
/// drift apart.
///
/// `hotspot` is `None` for an icon and `Some((x, y))` for a cursor, in pixels
/// from the top-left of the *target* bitmap.
fn make_hicon(icon: &IconImage, tw: i32, th: i32, hotspot: Option<(i32, i32)>) -> Option<isize> {
    use windows_sys::Win32::Graphics::Gdi::{CreateBitmap, CreateDIBSection};

    if tw <= 0 || th <= 0 {
        return None;
    }
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
    //
    // **The zeroes are supplied rather than assumed.** `CreateBitmap`'s
    // documentation says that with a null bit pointer "the contents of the new
    // bitmap is undefined" — the zero-fill every Windows to date has actually
    // done is not contractual, and this is the kind of thing Wine has already
    // lied about four times (docs/waycompose-design.md §13.6). For an icon a stray mask
    // bit is one wrong pixel in a taskbar button; for a *cursor* the AND mask
    // is what selects between "transparent" and "invert the screen", so a
    // garbage mask is a cursor with inverted holes punched through it. Cheap
    // to be sure: a 32x32 mask is 128 bytes.
    //
    // Scan lines of a `CreateBitmap` bitmap are **word**-aligned, not
    // dword-aligned as they would be in a DIB.
    let mask_stride = ((tw as usize).div_ceil(16)) * 2;
    let mask_bits = vec![0u8; mask_stride.checked_mul(th as usize)?];
    // SAFETY: a 1bpp monochrome bitmap whose bit buffer is exactly the size
    // Win32 computes for these dimensions, and which outlives the call.
    let mask = unsafe { CreateBitmap(tw, th, 1, 1, mask_bits.as_ptr().cast()) };

    let (f_icon, (hx, hy)) = match hotspot {
        None => (1, (0, 0)),
        // Clamped into the bitmap. A hotspot outside the image is not a Win32
        // error and not a Wayland one either — the protocol says only that it
        // is "in surface-local coordinates" — but it makes the pointer act at
        // a place that is not on the pointer, so a client that sends nonsense
        // gets the nearest sensible thing rather than an unusable cursor.
        Some((x, y)) => (0, (x.clamp(0, tw - 1) as u32, y.clamp(0, th - 1) as u32)),
    };

    let info = ICONINFO {
        fIcon: f_icon,
        xHotspot: hx,
        yHotspot: hy,
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
        // ADDED FOR docs/waycompose-design.md §64. Counted here rather than at
        // the two callers because this is the *only* `CreateIconIndirect` in
        // the compositor: window icons and mouse cursors are the same kind of
        // GDI object and are built by this one function. The two are freed by
        // different calls — `DestroyIcon` in `release_icons`, `DestroyCursor`
        // when the pointer changes — so the account is one creation counter and
        // two destruction counters, which is also the shape of the bug it would
        // catch: a path that creates through here and frees through neither.
        ICON_OBJECTS_MADE.with(|c| c.set(c.get() + 1));
        Some(hicon as isize)
    }
}

thread_local! {
    /// Every `HICON`/`HCURSOR` this process has built, and every one it has
    /// given back. ADDED FOR docs/waycompose-design.md §64.
    ///
    /// §53 gave the window handle an outside witness and §60 gave the OLE
    /// drop-target registration an inside count. This is the third and last
    /// per-window resource `ToUi::Destroy` releases, and it had **neither** —
    /// `release_icons` ran, destroyed what it found, and said nothing, so a
    /// window icon that was never released was invisible in exactly the way
    /// §60's stranded registration was.
    ///
    /// The difference at exit is not required to be zero: the cursor currently
    /// on screen is deliberately *not* destroyed — `set_cursor` frees the
    /// previous one only once the new one is installed, because destroying a
    /// cursor that is showing leaves the pointer drawn from freed memory. So
    /// the invariant is "outstanding is at most the one being shown", which is
    /// what `scripts/wc-drag.sh` asserts across seventeen cursor changes.
    static ICON_OBJECTS_MADE: Cell<u32> = const { Cell::new(0) };
    static ICONS_FREED: Cell<u32> = const { Cell::new(0) };
    static CURSORS_FREED: Cell<u32> = const { Cell::new(0) };
}

// ---------------------------------------------------------------------------
// The cursor
// ---------------------------------------------------------------------------

/// Install what the client asked the pointer to look like.
///
/// ADDED FOR STAGE 12 (docs/waycompose-design.md §23).
///
/// # The invariant this exists to keep
///
/// **Exactly one visible cursor, and it is a real Win32 cursor.** Nothing here
/// draws anything: a client's cursor surface is turned into an `HCURSOR` and
/// handed to Windows, which composites it above every window on the desktop
/// with the right hotspot, the right refresh behaviour and no tearing, for
/// free. The alternative — blitting the cursor image into the window as
/// content — is what produced the two-cursor bug the user reported, and it is
/// wrong for a reason no amount of care fixes: the client's cursor would be
/// clipped to the window, lag the real pointer by a frame, and still leave the
/// system arrow on top of it.
///
/// # Why the cursor is installed twice
///
/// Once here, and once from `WM_SETCURSOR`, and both are needed:
///
/// * `WM_SETCURSOR` is the only way to *keep* it. Windows re-asks on every
///   mouse message and `DefWindowProc` answers with the class cursor, so a
///   cursor set only here survives until the pointer moves one pixel.
/// * This call is the only way to *apply* it now. `WM_SETCURSOR` needs a mouse
///   message, and the interesting changes happen while the pointer is still —
///   a page finishing loading, a client hiding the pointer as the user starts
///   typing, a `wl_pointer.leave` restoring the arrow.
fn set_cursor(ui: &Rc<Ui>, spec: CursorSpec) {
    // Build the new one **before** destroying the old, so a failed build
    // leaves the previous cursor up rather than leaving the pointer invisible.
    let (handle, owned, what) = match &spec {
        CursorSpec::Hidden => (0isize, false, "hidden".to_string()),
        CursorSpec::Named(icon) => {
            let stock = crate::cursor::stock_for(*icon);
            let h = stock_cursor(stock.idc);
            if h == 0 {
                // `LoadCursorW` of a stock cursor failing is not something that
                // happens on Windows; it is exactly the kind of thing that
                // happens on Wine. Keep whatever is up and say so.
                eprintln!(
                    "wc-compositor: LoadCursorW({}) failed for shape {:?}; cursor unchanged",
                    stock.name,
                    icon.name()
                );
                return;
            }
            (h, false, format!("{} ({})", icon.name(), stock.name))
        }
        CursorSpec::Image { image, hotspot_x, hotspot_y } => {
            match make_cursor(image, *hotspot_x, *hotspot_y) {
                Some(h) => (
                    h,
                    true,
                    format!(
                        "{}x{} pixels, hotspot {hotspot_x},{hotspot_y}",
                        image.width, image.height
                    ),
                ),
                None => {
                    eprintln!(
                        "wc-compositor: could not build an HCURSOR from {}x{} pixels; \
                         cursor unchanged",
                        image.width, image.height
                    );
                    return;
                }
            }
        }
    };

    let previous = ui.cursor.replace(handle);
    let previously_owned = ui.owned_cursor.replace(if owned { handle } else { 0 });

    if ui.pointer_inside.get() {
        // SAFETY: 0 is documented as "hide the cursor"; anything else is a
        // handle produced immediately above and stored in `ui.cursor`, so it
        // outlives the call.
        unsafe {
            SetCursor(handle as _);
        }
    }

    // **After** `SetCursor`, and only if it is not the one still showing.
    // Destroying a cursor that is on screen is legal but leaves the pointer
    // drawn from freed memory until the next `SetCursor`, which is the classic
    // way to get a corrupted pointer that survives the process.
    if previously_owned != 0 && previously_owned != handle {
        // SAFETY: came from `CreateIconIndirect`, is no longer installed, and
        // the cell that held it has already been overwritten so this runs once.
        unsafe {
            DestroyCursor(previously_owned as _);
        }
        CURSORS_FREED.with(|c| c.set(c.get() + 1));
    }

    if previous != handle {
        println!("wc-compositor: cursor -> {what}");
    }
}

/// One of Win32's built-in cursors, by `IDC_*` ordinal.
///
/// **Not destroyed, ever.** Stock cursors are shared process-wide and owned by
/// the system; `DestroyCursor` on one is documented as an error and in practice
/// takes the arrow away from every window in the process.
///
/// The ordinal is cast to a pointer rather than named as a `windows-sys`
/// constant because the table that chose it lives in `crate::cursor`, which
/// compiles on Linux and must not mention `windows-sys` (see that file). This
/// is `MAKEINTRESOURCE`, which has always been "the integer, in the pointer".
fn stock_cursor(idc: u16) -> isize {
    // SAFETY: a null instance means "the system's own resources", which is
    // where the `IDC_*` cursors live, and the resource id is an integer
    // ordinal rather than a string pointer.
    unsafe { LoadCursorW(std::ptr::null_mut(), idc as usize as *const u16) as isize }
}

/// Build an `HCURSOR` from a client's cursor surface.
///
/// # Why the image is not scaled
///
/// [`make_icon`] resamples, because Windows asks for two specific icon sizes
/// and a client's icon is whatever size the theme had. A cursor has no such
/// requirement: `CreateIconIndirect` accepts any dimensions, and the sizes
/// clients actually send — 24x24 and 32x32 from the XCursor themes, 48x48 and
/// 64x64 at scale 2 — are all displayed as sent. Not scaling means the hotspot
/// arrives as an exact pixel rather than as a rounded one, which matters:
/// the hotspot *is* the pointer, and half a pixel of drift on a 1-pixel-wide
/// I-beam is the difference between clicking before and after a character.
///
/// # The alpha
///
/// `wl_shm`'s `Argb8888` is **premultiplied**, and a 32-bit `CreateIconIndirect`
/// wants premultiplied, so there is nothing to convert — but there is
/// something not to do, which is to discard it. A cursor is mostly transparent
/// and almost entirely antialiased edge; dropping the alpha channel gives a
/// black rectangle with a cursor somewhere in it, and forcing it opaque gives
/// a cursor with a hard jagged outline. Both are worse than the arrow this
/// replaces, so the ARGB path is the only one implemented.
fn make_cursor(image: &IconImage, hotspot_x: i32, hotspot_y: i32) -> Option<isize> {
    let h = make_hicon(
        image,
        image.width,
        image.height,
        Some((hotspot_x, hotspot_y)),
    )?;
    verify_cursor_once(h);
    Some(h)
}

/// Read the first cursor back out of Win32 and say what it became.
///
/// ADDED FOR STAGE 12, and it is a harness affordance rather than a check the
/// compositor needs — but it has to live here, because **an `HCURSOR` from
/// `CreateIconIndirect` belongs to the process that made it**. Measured:
/// `GetIconInfo` on this handle from `scripts/probe-cursor.c`, a different
/// process on the same desktop, fails with 1402 `ERROR_INVALID_CURSOR_HANDLE`.
/// So the probe can say "the cursor Win32 is showing is not one of the sixteen
/// stock ones", which is real evidence and is all it can get, and this says
/// what the thing it is showing is made of.
///
/// Three facts, and each is a different way for the cursor to be wrong in a way
/// nobody would see as a failure:
///
/// * `fIcon == 0`. An `HICON` and an `HCURSOR` are the same handle type, and
///   `SetCursor` accepts an icon — it just draws it with the hotspot at 0,0.
/// * The hotspot. Off by a few pixels means the pointer clicks somewhere other
///   than where it is drawn, which reads as a badly calibrated screen.
/// * 32 bits per pixel in the colour bitmap. Anything else means the alpha
///   channel was dropped somewhere, and a cursor without alpha is a black
///   rectangle with a shape in it.
///
/// Once per process: this is a hot-ish path — an animated cursor rebuilds
/// several times a second — and the interesting question is whether the
/// conversion works at all, which the first one answers.
fn verify_cursor_once(hcursor: isize) {
    use std::sync::atomic::{AtomicBool, Ordering};
    use windows_sys::Win32::Graphics::Gdi::{GetObjectW, BITMAP};
    use windows_sys::Win32::UI::WindowsAndMessaging::GetIconInfo;

    static DONE: AtomicBool = AtomicBool::new(false);
    if DONE.swap(true, Ordering::Relaxed) {
        return;
    }

    let mut info: ICONINFO = unsafe { std::mem::zeroed() };
    // SAFETY: a handle this process created, and `info` is written by the call.
    if unsafe { GetIconInfo(hcursor as _, &mut info) } == 0 {
        eprintln!(
            "wc-compositor: GetIconInfo on our own HCURSOR failed ({})",
            std::io::Error::last_os_error()
        );
        return;
    }

    let mut bm: BITMAP = unsafe { std::mem::zeroed() };
    // SAFETY: `GetIconInfo` hands over two bitmaps this process now owns, and
    // `bm` is written by the call.
    let bits = if info.hbmColor.is_null() {
        0
    } else if unsafe {
        GetObjectW(
            info.hbmColor as _,
            std::mem::size_of::<BITMAP>() as i32,
            (&mut bm as *mut BITMAP).cast(),
        )
    } != 0
    {
        bm.bmBitsPixel
    } else {
        0
    };

    println!(
        "wc-compositor: cursor round-trip: fIcon={} hotspot={},{} colour={}x{} at {} bpp",
        info.fIcon, info.xHotspot, info.yHotspot, bm.bmWidth, bm.bmHeight, bits
    );

    // `GetIconInfo` **creates** the two bitmaps it returns and the caller owns
    // them. Not deleting them here would leak two GDI objects, which is the
    // documented behaviour that is easiest to miss because nothing ever
    // complains about it.
    // SAFETY: both handles came from the call above and are deleted once.
    unsafe {
        if !info.hbmColor.is_null() {
            DeleteObject(info.hbmColor as _);
        }
        if !info.hbmMask.is_null() {
            DeleteObject(info.hbmMask as _);
        }
    }
}

/// How fast Windows itself repeats a held key, as `wl_keyboard.repeat_info`
/// wants it: a delay in milliseconds and a rate in keys per second.
///
/// ADDED FOR STAGE 12. Until now these were the constants 500 and 30 — Windows'
/// *defaults*, written out — which meant a user who had turned the repeat rate
/// up in the Control Panel got the default in every Wayland client and the
/// system rate in every native one. The compositor does not repeat anything
/// itself (see `state.rs`), so this number is not a knob the compositor honours
/// but a fact about the machine that it forwards; inventing it was the one
/// thing that could not be right.
///
/// # The two scales, neither of which is in the units Wayland wants
///
/// `SPI_GETKEYBOARDDELAY` returns **0 to 3**, meaning 250 ms to 1000 ms in
/// 250 ms steps. That is the documented mapping and it is exact.
///
/// `SPI_GETKEYBOARDSPEED` returns **0 to 31**, and Microsoft documents only the
/// endpoints: 0 is "approximately 2.5 repetitions per second" and 31 is
/// "approximately 30". The scale between them is linear in the *rate*, which is
/// how `keybd.cpl`'s slider has always behaved. So this interpolates, and says
/// so, rather than presenting a table of thirty-two numbers it would have made
/// up. The endpoints are exact; the middle is an estimate, and the error at
/// worst is a fraction of a repeat per second — well inside what a user could
/// perceive as anything but "about right".
///
/// A failed call keeps the old constants. `SystemParametersInfo` failing is not
/// something Windows does, and is something Wine might (docs/waycompose-design.md §13.6).
pub fn keyboard_repeat() -> (i32, i32) {
    let mut delay: u32 = 1;
    let mut speed: u32 = 31;

    // SAFETY: both actions are documented as writing a `u32` through `pvParam`;
    // `uiParam` is unused and `fWinIni` of 0 means "do not broadcast".
    let ok_delay = unsafe {
        SystemParametersInfoW(
            SPI_GETKEYBOARDDELAY,
            0,
            (&mut delay as *mut u32).cast(),
            0,
        ) != 0
    };
    // SAFETY: as above.
    let ok_speed = unsafe {
        SystemParametersInfoW(
            SPI_GETKEYBOARDSPEED,
            0,
            (&mut speed as *mut u32).cast(),
            0,
        ) != 0
    };

    if !ok_delay || !ok_speed {
        eprintln!(
            "wc-compositor: SystemParametersInfo could not read the keyboard repeat settings; \
             advertising Windows' defaults"
        );
        return (500, 30);
    }

    let delay_ms = 250 * (delay.min(3) as i32 + 1);
    // 2.5/s at 0, 30/s at 31, linear in between, rounded to the nearest whole
    // repeat because `repeat_info` counts in whole ones.
    let rate_hz = ((25 + (speed.min(31) as i32) * 275 / 31) + 5) / 10;

    println!(
        "wc-compositor: keyboard repeat: SPI delay {delay} -> {delay_ms} ms, \
         SPI speed {speed} -> {rate_hz}/s"
    );
    (delay_ms, rate_hz)
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
    hide_own_console();
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
        made: Cell::new(0),
        gone: Cell::new(0),
        controller: Cell::new(controller as isize),
        idle_inhibited: Cell::new(false),
        html_format: Cell::new(0),
        png_format: Cell::new(0),
        dump_frames: config.dump_frames.clone(),
        // The arrow, so that the very first `WM_SETCURSOR` — which arrives
        // before any client has said anything about a cursor — installs the
        // same shape the window class would have. Starting at 0 would mean a
        // window with no pointer at all until the first client spoke.
        cursor: Cell::new(stock_cursor(crate::cursor::IDC_ARROW.idc)),
        owned_cursor: Cell::new(0),
        pointer_inside: Cell::new(false),
        slots: RefCell::new(crate::input::touch::Slots::new()),
        hold: hold::HoldState::default(),
    });
    UI.with(|slot| *slot.borrow_mut() = Some(ui.clone()));

    // Watch the clipboard from here on. A message-only window is a perfectly
    // good listener — the notification is posted to the handle rather than
    // broadcast to visible windows — and it is the right one, because the
    // clipboard belongs to the compositor and not to any client's window.
    //
    // A failure is logged and shrugged off: without it the Windows-to-Wayland
    // direction stops being automatic and nothing else changes, which is worth
    // a degraded run rather than a refusal to start.
    // SAFETY: a live window handle owned by this thread.
    if unsafe { AddClipboardFormatListener(controller) } == 0 {
        eprintln!(
            "wc-compositor: AddClipboardFormatListener failed ({}); \
             Windows-to-Wayland clipboard changes will not be announced",
            std::io::Error::last_os_error()
        );
    } else {
        println!("wc-compositor: watching the system clipboard (WM_CLIPBOARDUPDATE)");
    }
    // §93. **And say what is on it already.** `WM_CLIPBOARDUPDATE` reports a
    // change, so whatever the user copied *before* this compositor started —
    // a URL copied in another program, then a double-click on the shortcut —
    // was never offered to Wayland, and the first paste in a new session
    // pasted nothing until something was copied again. A session ends a
    // minute after its last window closes, so every reopened window started
    // that way. One announcement now, of the clipboard as it is.
    on_clipboard_update(&ui, "at start-up, the clipboard already holds");

    Ok(assemble(
        tx,
        Wake {
            hwnd: controller as isize,
        },
        PumpInner { ui },
        primary_work_area(),
    ))
}

/// The primary monitor's work area, for the one `wl_output` to describe.
///
/// ADDED FOR STAGE 38. `SPI_GETWORKAREA` rather than `MonitorFromWindow`
/// because this runs before any window exists, and it answers for the
/// **primary** monitor — which is the right one and the only one, since this
/// compositor advertises exactly one output. A second monitor would need a
/// second `wl_output` and everything that follows from one, and that is not
/// this stage (§38.6).
///
/// `SM_CXSCREEN`/`SM_CYSCREEN` are what `announce_dpi_awareness` prints and
/// they are deliberately **not** used here: they are the full screen, taskbar
/// included, so a client that filled the output would have its bottom rows
/// behind the taskbar.
fn primary_work_area() -> Option<(i32, i32)> {
    let mut rc = RECT { left: 0, top: 0, right: 0, bottom: 0 };
    // SAFETY: `SPI_GETWORKAREA` writes a `RECT` through `pvParam`; the other
    // two arguments are unused for this action.
    let got = unsafe {
        SystemParametersInfoW(
            SPI_GETWORKAREA,
            0,
            (&mut rc as *mut RECT).cast::<c_void>(),
            0,
        )
    };
    if got == 0 {
        return None;
    }
    let (w, h) = (rc.right - rc.left, rc.bottom - rc.top);
    if w <= 0 || h <= 0 {
        return None;
    }
    println!(
        "wc-compositor: the primary monitor's work area is {w}x{h} at {},{} \
         (the screen minus the taskbar)",
        rc.left, rc.top
    );
    Some((w, h))
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
        // The cursor we built, if the last thing a client asked for was its own
        // pixels. Nothing is showing it any more — every window is gone — and
        // the process may outlive the pump by however long the Wayland thread
        // takes to wind up.
        let owned = self.ui.owned_cursor.replace(0);
        if owned != 0 {
            // SAFETY: came from `CreateIconIndirect` in `make_cursor`, is
            // destroyed once because the cell is cleared in the same breath,
            // and is not installed anywhere: no window remains.
            unsafe {
                DestroyCursor(owned as _);
            }
        }

        let controller = self.ui.controller.replace(0);
        if controller != 0 {
            // SAFETY: created on this thread, destroyed once, and no `Ui`
            // handle can post to it after `Stopped` is observed. The clipboard
            // registration is removed first: `DestroyWindow` would take it with
            // the window, and unhooking explicitly is what makes a run that
            // ends cleanly leave nothing behind in the window station — which
            // matters on a guest several worktrees share (§16.8).
            unsafe {
                RemoveClipboardFormatListener(controller as HWND);
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

/// Ask for per-monitor DPI awareness, and say **why** it did or did not happen.
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
/// degradation for a failure. Everything else read here is resolved the same
/// way and for the same reason.
///
/// # Why "refused" was not enough (docs/waycompose-design.md §10.13 item 3)
///
/// §10.8 recorded that Wine reports `ok` and the guest reports failure, and
/// that the compositor could not tell an **already-aware** process from an
/// **unavailable context**. Those are one fine case and one bug, and they are
/// distinguished by exactly two things the old code threw away: `GetLastError`,
/// and the awareness the process actually ended up with.
///
/// * `ERROR_ACCESS_DENIED` (5) is documented as "the default API awareness mode
///   for the process has already been set", so the awareness printed *before*
///   the call is the one that will be used, and the question becomes whether
///   that one is good enough rather than whether the call worked.
/// * `ERROR_INVALID_PARAMETER` (87) is the context itself not being available,
///   which on a scaled display is the bug.
///
/// So this prints the awareness before, the call, the error, and the awareness
/// after — and then the display metrics that the choice actually decides,
/// because a per-monitor-aware process and a virtualised one disagree about
/// `SM_CXSCREEN` and that disagreement is the whole observable effect.
/// Hide the console window, but only if this process is the one it was created
/// for.
///
/// # Why a compositor cares about a console window
///
/// Because the shell groups taskbar buttons **by process**, and a console
/// window is a window. Stage 11 spent a run on this: the client's icon was
/// built, `WM_SETICON` succeeded for both slots, the window's caption showed
/// the icon — and the taskbar button showed Windows' generic application icon
/// anyway. The reason is that the process also owned the console window the
/// task scheduler gave it, the shell put both windows under one button, and the
/// icon on a grouped button comes from the *executable* rather than from either
/// window's `WM_SETICON`. `wc-compositor.exe` has no icon resource, so the
/// group got `IDI_APPLICATION`.
///
/// So this is not tidiness. It is the difference between the taskbar showing
/// the client's icon and showing the generic one, and it was invisible to every
/// check that looked at the compositor's own logs (docs/waycompose-design.md §16.6).
///
/// **`GetConsoleProcessList` is what makes this safe.** A console shared with
/// anything else — a `cmd.exe` or an ssh session that started us — belongs to
/// that other process, and hiding it would take away the user's terminal. A
/// console with exactly one process attached was created for this process and
/// dies with it.
fn hide_own_console() {
    use windows_sys::Win32::System::Console::{GetConsoleProcessList, GetConsoleWindow};
    use windows_sys::Win32::UI::WindowsAndMessaging::SW_HIDE;

    // SAFETY: no arguments, and a null return is the documented "no console".
    let hwnd = unsafe { GetConsoleWindow() };
    if hwnd.is_null() {
        return;
    }
    let mut pids = [0u32; 4];
    // SAFETY: a live buffer whose length is passed truthfully. The return is
    // the number of processes attached, which may exceed the buffer.
    let n = unsafe { GetConsoleProcessList(pids.as_mut_ptr(), pids.len() as u32) };
    if n != 1 {
        println!(
            "wc-compositor: the console has {n} processes attached, so it is somebody else's \
             and is left alone"
        );
        return;
    }
    // SAFETY: a live window handle from `GetConsoleWindow`.
    unsafe {
        ShowWindow(hwnd, SW_HIDE);
    }
    println!(
        "wc-compositor: hid this process's own console window, so the shell does not group \
         the toplevel with it and give the group the executable's icon"
    );
}

fn announce_dpi_awareness() {
    // The documented `DPI_AWARENESS_CONTEXT` pseudo-handles. They are opaque
    // negative values rather than pointers, which is why they can be constants.
    const PER_MONITOR_AWARE_V2: isize = -4;

    let lib = wide("user32.dll");
    // SAFETY: a live NUL-terminated name; user32 is already loaded, so this is
    // a reference-count bump on a module that cannot go away.
    let user32 = unsafe { LoadLibraryW(lib.as_ptr()) };
    if user32.is_null() {
        return;
    }
    // SAFETY (all four): ASCII, NUL-terminated symbol names, and each
    // `transmute` below is to that entry point's documented signature. A
    // missing symbol comes back as `None` and is handled.
    let set = unsafe { GetProcAddress(user32, c"SetProcessDpiAwarenessContext".as_ptr().cast()) };
    let get_thread =
        unsafe { GetProcAddress(user32, c"GetThreadDpiAwarenessContext".as_ptr().cast()) };
    let get_awareness =
        unsafe { GetProcAddress(user32, c"GetAwarenessFromDpiAwarenessContext".as_ptr().cast()) };
    let dpi_for_system = unsafe { GetProcAddress(user32, c"GetDpiForSystem".as_ptr().cast()) };

    // `DPI_AWARENESS`: 0 unaware, 1 system-aware, 2 per-monitor-aware. -1 is
    // the documented "invalid" return and is what a missing symbol reads as.
    let awareness = || -> i32 {
        let (Some(t), Some(a)) = (get_thread, get_awareness) else {
            return -1;
        };
        // `GetProcAddress` already hands back an `unsafe extern "system" fn() -> isize`,
        // which is `GetThreadDpiAwarenessContext`'s signature exactly, so only the
        // second entry point needs a transmute.
        // SAFETY: `GetAwarenessFromDpiAwarenessContext(ctx) -> DPI_AWARENESS`.
        unsafe {
            let a: unsafe extern "system" fn(isize) -> i32 = std::mem::transmute(a);
            a(t())
        }
    };
    let name = |a: i32| match a {
        0 => "unaware",
        1 => "system-aware",
        2 => "per-monitor-aware",
        _ => "unknown",
    };

    let before = awareness();
    match set {
        Some(f) => {
            // SAFETY: the documented signature of
            // `SetProcessDpiAwarenessContext`: one `DPI_AWARENESS_CONTEXT`
            // (an opaque `isize`), returning `BOOL`.
            let f: unsafe extern "system" fn(isize) -> i32 = unsafe { std::mem::transmute(f) };
            let ok = unsafe { f(PER_MONITOR_AWARE_V2) };
            // Read immediately: anything in between, including the `println!`,
            // is entitled to clobber the thread's last error.
            let err = std::io::Error::last_os_error().raw_os_error().unwrap_or(0);
            let after = awareness();
            let verdict = if ok != 0 {
                "ok"
            } else {
                match err {
                    // ERROR_ACCESS_DENIED: already set, and `after` says to what.
                    5 => "refused: awareness was already set",
                    // ERROR_INVALID_PARAMETER: this context is not available.
                    87 => "refused: PER_MONITOR_AWARE_V2 is not an available context",
                    _ => "refused",
                }
            };
            println!(
                "wc-compositor: SetProcessDpiAwarenessContext(PER_MONITOR_AWARE_V2) -> {verdict} \
                 (err {err}); awareness {} -> {}",
                name(before),
                name(after)
            );
        }
        None => {
            println!(
                "wc-compositor: no SetProcessDpiAwarenessContext; windows will be DPI-virtualised"
            );
        }
    }

    // The observable consequence, printed whichever way the call went. On a
    // 150%-scaled 1920x1200 panel an unaware process is told 1280x800 and a
    // per-monitor-aware one is told 1920x1200, so this line is what says which
    // of the two the compositor is about to size windows in.
    let sysdpi = match dpi_for_system {
        // SAFETY: `GetDpiForSystem() -> UINT`, no arguments.
        Some(f) => unsafe {
            let f: unsafe extern "system" fn() -> u32 = std::mem::transmute(f);
            f()
        },
        None => 0,
    };
    // SAFETY: two documented system-metric indices; the call cannot fail.
    let (cx, cy) = unsafe { (GetSystemMetrics(SM_CXSCREEN), GetSystemMetrics(SM_CYSCREEN)) };
    println!(
        "wc-compositor: primary screen {cx}x{cy}, system dpi {sysdpi} ({}%)",
        if sysdpi == 0 { 0 } else { sysdpi * 100 / 96 }
    );
}
