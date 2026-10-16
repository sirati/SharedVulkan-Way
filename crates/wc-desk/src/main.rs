//! Look at the guest's interactive desktop.
//!
//! ```text
//!   wc-desk station                     where am I, and what does the screen say
//!   wc-desk windows                     every top-level window, and which are taskbar entries
//!   wc-desk shot --out F.bmp [--title T | --class C | --hwnd H | --screen]
//!   wc-desk tick [--samples N]          what a millisecond costs on this machine
//! ```
//!
//! # Why this exists
//!
//! docs/waycompose-design.md §10.12 item 3: *the window has never been seen*. Everything
//! the compositor has ever done on the guest ran inside an ssh session, whose
//! window station is not `WinSta0` and has no display behind it —
//! `CreateWindowExW` succeeds there, `SetDIBitsToDevice` succeeds there, and
//! nothing is ever on a screen. §8.11 items 2, 4, 5 and 6 all reduce to "look at
//! it", and none of them had run.
//!
//! This is the binary that looks. It is separate from the compositor because it
//! has to be able to run when the compositor is *not* — the first thing to
//! establish was that an interactive desktop is reachable at all, which is a
//! question about the machine rather than about this project.
//!
//! # The three things that made this harder than "take a screenshot"
//!
//! 1. **An ssh session cannot see the desktop, and does not say so.** Its
//!    window station is `Service-0x0-xxxx$`, whose one desktop has no display
//!    association; `GetSystemMetrics(SM_CXSCREEN)` there answers 1024x768 for a
//!    screen that does not exist, and a `BitBlt` from `GetDC(NULL)` fails with
//!    `ERROR_ACCESS_DENIED`. Both look like ordinary answers. `station` prints
//!    the names so that a capture can be attributed rather than trusted.
//! 2. **Getting onto `WinSta0` is a scheduling problem, not an API.** A
//!    process cannot walk into another session's window station; something
//!    already in that session has to start it. `scripts/wc-desktop.sh` uses the
//!    task scheduler's interactive-token flag, which is the one route that needs
//!    neither a service nor a password.
//! 3. **The desktop may be covered.** On this guest it is: the session is
//!    logged on, `explorer.exe` and its taskbar are running, and a full-screen
//!    lock-screen window sits on top of them with Windows Hello waiting for a
//!    face nobody is going to show it. A screen grab is therefore a picture of
//!    the lock screen and says nothing about our window. `PrintWindow` with
//!    `PW_RENDERFULLCONTENT` reads a window's DWM redirection surface instead of
//!    the framebuffer, so it returns the real composited pixels of a window that
//!    is occluded — including the taskbar's. That is what makes the evidence in
//!    this stage possible without a credential.
//!
//! # What a `PrintWindow` capture is and is not evidence of
//!
//! It **is** evidence about the window: the frame is drawn by DWM and the
//! theme, at the window's DPI, and what comes back is the same surface the
//! screen would show. The caption, the border thickness, the button glyphs and
//! the client area are all the system's own work, and a wrong one is visible.
//!
//! It is **not** evidence about the screen: z-order, whether anything is on top,
//! and whether the monitor is even on are outside it. Where that distinction
//! matters below, it is stated.

fn main() -> std::process::ExitCode {
    imp::main()
}

#[cfg(not(windows))]
mod imp {
    /// The Linux build exists so that `cargo clippy --workspace --all-targets`
    /// lints this file on the machine that builds it, the same arrangement
    /// `wc-handle`'s Unix half is there for. There is no desktop to look at.
    pub fn main() -> std::process::ExitCode {
        eprintln!("wc-desk: Windows only — there is nothing here to look at");
        std::process::ExitCode::FAILURE
    }
}

#[cfg(windows)]
mod imp {
    use std::ffi::c_void;

    use windows_sys::Win32::Foundation::{HANDLE, HWND, LPARAM, RECT};
    use windows_sys::Win32::Graphics::Gdi::{
        BitBlt, CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject, GetDC, ReleaseDC,
        SelectObject, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, CAPTUREBLT, DIB_RGB_COLORS, HBITMAP,
        HDC, SRCCOPY,
    };
    use windows_sys::Win32::Graphics::Dwm::{DwmGetWindowAttribute, DWMWA_EXTENDED_FRAME_BOUNDS};
    use windows_sys::Win32::Storage::Xps::PrintWindow;
    use windows_sys::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
    use windows_sys::Win32::System::Performance::{
        QueryPerformanceCounter, QueryPerformanceFrequency,
    };
    use windows_sys::Win32::System::RemoteDesktop::ProcessIdToSessionId;
    // ADDED FOR STAGE 32: reading a window's taskbar identity back out of the
    // shell. See `aumid`.
    use windows_sys::Win32::System::Com::{
        CoInitializeEx, CoTaskMemFree, COINIT_APARTMENTTHREADED,
    };
    use windows_sys::Win32::UI::Shell::PropertiesSystem::SHGetPropertyStoreForWindow;
    use windows_sys::Win32::Foundation::PROPERTYKEY;
    use windows_sys::core::{GUID, HRESULT};
    use windows_sys::Win32::System::StationsAndDesktops::{
        CloseDesktop, GetProcessWindowStation, GetThreadDesktop, GetUserObjectInformationW,
        OpenInputDesktop, UOI_NAME,
    };
    use windows_sys::Win32::System::Threading::{
        CreateWaitableTimerExW, GetCurrentProcessId, GetCurrentThreadId, SetWaitableTimer, Sleep,
        WaitForSingleObject, CREATE_WAITABLE_TIMER_HIGH_RESOLUTION, TIMER_ALL_ACCESS,
    };
    // §82: `mouse`, which drives a real drag through `SendInput`.
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
        SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBDINPUT, KEYEVENTF_KEYUP,
        KEYEVENTF_SCANCODE, MOUSEEVENTF_ABSOLUTE, MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP,
        MOUSEEVENTF_MOVE, MOUSEINPUT,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        EnumWindows, FindWindowW, GetClassNameW, GetSystemMetrics, GetWindow, GetWindowLongPtrW,
        GetWindowRect, GetWindowTextW, GetWindowThreadProcessId, IsIconic, IsWindowVisible,
        PostMessageW, SendMessageTimeoutW, IsZoomed, SetForegroundWindow, WM_NCHITTEST,
        GWL_EXSTYLE, GWL_STYLE, GW_OWNER, PW_RENDERFULLCONTENT, SC_SIZE, SMTO_ABORTIFHUNG,
        SM_CXSCREEN, SM_CXVIRTUALSCREEN, SM_CYSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN,
        SM_YVIRTUALSCREEN, WM_KEYDOWN, WM_KEYUP, WM_NULL, WM_SYSCOMMAND, WS_EX_APPWINDOW,
        WS_EX_TOOLWINDOW, WS_VISIBLE,
    };

    // ---------------------------------------------------------------- args --

    fn usage() -> &'static str {
        "\
usage: wc-desk <command> [options]

  station                  window station, desktop, session, screen metrics
  windows                  every top-level window, and which are taskbar entries
  shot --out FILE          capture, as a 32-bit BMP
       --title SUBSTRING     the first visible window whose title contains this
       --class NAME          the first window of this class (Shell_TrayWnd, ...)
       --hwnd 0xN            exactly this window
       --pid N               a window of this process. Takes precedence over
                             everything else, and --class then filters within
                             it: on a guest two worktrees share, the class name
                             alone selects somebody else's compositor.
       --screen              the whole virtual screen, through GetDC(NULL)
       --visible             crop to what DWM draws (DWMWA_EXTENDED_FRAME_BOUNDS):
                             without it the picture includes Windows 11's
                             invisible resize borders, black in a capture
  tick [--samples N]       what Sleep(1), Sleep(5) and a high-resolution
                           waitable timer actually cost (default 20 samples)
  drag --pid N             put a window into Windows' modal sizing loop, grow it
       [--steps N]           from the keyboard, and leave it there for --hold-ms
       [--hold-ms N]         so that something else can watch it repaint
  hittest --pid N --at X,Y [--at X,Y ...]
                           WM_NCHITTEST at window-relative points; a negative
                           coordinate counts from the right or bottom edge
  mouse --pid N [--drag A B] [--click A] [--dblclick A] [--hover A MS] [--step-ms N]
                           [--key NAME] [--hold NAME MS] [--rel DX DY N] [--wait MS]
                           (§96: keys by scan code, relative motion for a locked pointer)
                           real input through SendInput, the window rectangle
                           printed before and after each gesture

  --unaware                do NOT ask for per-monitor DPI awareness first.
                           Every command takes it; it is how a capture in real
                           pixels and one in virtualised pixels are told apart.
"
    }

    pub fn main() -> std::process::ExitCode {
        let argv: Vec<String> = std::env::args().skip(1).collect();
        let mut aware = true;
        let mut args: Vec<String> = Vec::new();
        for a in argv {
            if a == "--unaware" {
                aware = false;
            } else {
                args.push(a);
            }
        }
        let Some(cmd) = args.first().cloned() else {
            eprint!("{}", usage());
            return std::process::ExitCode::FAILURE;
        };

        let dpi = dpi_report(aware);

        let r = match cmd.as_str() {
            "station" => cmd_station(&dpi),
            "windows" => cmd_windows(),
            "shot" => cmd_shot(&args[1..], &dpi),
            "tick" => cmd_tick(&args[1..]),
            "drag" => cmd_drag(&args[1..]),
            "hittest" => cmd_hittest(&args[1..]),
            "mouse" => cmd_mouse(&args[1..]),
            other => {
                eprintln!("wc-desk: unknown command {other:?}");
                eprint!("{}", usage());
                Err(std::io::Error::other("bad command"))
            }
        };
        match r {
            Ok(()) => std::process::ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("wc-desk: {e}");
                std::process::ExitCode::FAILURE
            }
        }
    }

    fn opt<'a>(args: &'a [String], name: &str) -> Option<&'a str> {
        args.iter()
            .position(|a| a == name)
            .and_then(|i| args.get(i + 1))
            .map(String::as_str)
    }

    // ----------------------------------------------------------------- dpi --

    struct Dpi {
        before: i32,
        after: i32,
        called: bool,
        ok: bool,
        err: i32,
        system: u32,
    }

    /// Ask for per-monitor DPI awareness, exactly as `wc-compositor` does, and
    /// keep the whole story.
    ///
    /// # Why this is a second copy of the compositor's call and not a shared one
    ///
    /// Because it is a **control**, and a control that shared an implementation
    /// with the thing it controls would not be one. §10.8 recorded that the
    /// compositor's call is refused on the guest and succeeds under Wine, and
    /// left open whether that is a property of the machine or of that binary —
    /// an app-compat shim keyed on an image name, a manifest, an earlier call.
    /// A second, differently-named executable making the identical call answers
    /// that: if both are refused it is the machine, and if only one is it is the
    /// binary. The call is four lines; the fact it establishes is not available
    /// any other way.
    fn dpi_report(ask: bool) -> Dpi {
        const PER_MONITOR_AWARE_V2: isize = -4;
        let lib: Vec<u16> = "user32.dll\0".encode_utf16().collect();
        // SAFETY: a live NUL-terminated name; user32 is already loaded.
        let user32 = unsafe { LoadLibraryW(lib.as_ptr()) };
        // SAFETY (all four): ASCII NUL-terminated symbol names.
        let (set, get_thread, get_awareness, for_system) = unsafe {
            (
                GetProcAddress(user32, c"SetProcessDpiAwarenessContext".as_ptr().cast()),
                GetProcAddress(user32, c"GetThreadDpiAwarenessContext".as_ptr().cast()),
                GetProcAddress(user32, c"GetAwarenessFromDpiAwarenessContext".as_ptr().cast()),
                GetProcAddress(user32, c"GetDpiForSystem".as_ptr().cast()),
            )
        };
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

        let before = awareness();
        let (called, ok, err) = match (ask, set) {
            (true, Some(f)) => {
                // SAFETY: `SetProcessDpiAwarenessContext(ctx) -> BOOL`.
                let f: unsafe extern "system" fn(isize) -> i32 = unsafe { std::mem::transmute(f) };
                let ok = unsafe { f(PER_MONITOR_AWARE_V2) };
                let err = std::io::Error::last_os_error().raw_os_error().unwrap_or(0);
                (true, ok != 0, err)
            }
            _ => (false, false, 0),
        };
        let system = match for_system {
            // SAFETY: `GetDpiForSystem() -> UINT`, no arguments.
            Some(f) => unsafe {
                let f: unsafe extern "system" fn() -> u32 = std::mem::transmute(f);
                f()
            },
            None => 0,
        };
        Dpi {
            before,
            after: awareness(),
            called,
            ok,
            err,
            system,
        }
    }

    fn awareness_name(a: i32) -> &'static str {
        match a {
            0 => "unaware",
            1 => "system-aware",
            2 => "per-monitor-aware",
            _ => "unknown",
        }
    }

    fn print_dpi(d: &Dpi) {
        if d.called {
            let verdict = if d.ok {
                "ok".to_string()
            } else {
                match d.err {
                    5 => "refused: awareness was already set (ERROR_ACCESS_DENIED)".to_string(),
                    87 => "refused: context unavailable (ERROR_INVALID_PARAMETER)".to_string(),
                    e => format!("refused: error {e}"),
                }
            };
            println!("dpi.call          SetProcessDpiAwarenessContext(PER_MONITOR_AWARE_V2) -> {verdict}");
        } else {
            println!("dpi.call          not made (--unaware)");
        }
        println!(
            "dpi.awareness     {} -> {}",
            awareness_name(d.before),
            awareness_name(d.after)
        );
        println!(
            "dpi.system        {} ({}%)",
            d.system,
            if d.system == 0 { 0 } else { d.system * 100 / 96 }
        );
    }

    // ------------------------------------------------------------- station --

    /// The name of a window station or desktop handle.
    ///
    /// `GetUserObjectInformationW` takes a **byte** count and writes UTF-16, and
    /// getting that wrong returns a string that looks like a one-character name
    /// rather than an error — which cost a round here, in PowerShell, before
    /// this was written in a language with a byte count in the type system.
    fn object_name(h: HANDLE) -> String {
        let mut buf = [0u16; 128];
        let mut need: u32 = 0;
        // SAFETY: `buf` is 128 `u16`s and the length is passed in bytes, as
        // documented; `need` is written on failure and ignored.
        let ok = unsafe {
            GetUserObjectInformationW(
                h,
                UOI_NAME,
                buf.as_mut_ptr().cast::<c_void>(),
                (buf.len() * 2) as u32,
                &mut need,
            )
        };
        if ok == 0 {
            return format!("<error {}>", std::io::Error::last_os_error());
        }
        let n = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
        String::from_utf16_lossy(&buf[..n])
    }

    fn cmd_station(dpi: &Dpi) -> std::io::Result<()> {
        let pid = unsafe { GetCurrentProcessId() };
        let mut session: u32 = u32::MAX;
        // SAFETY: `session` is written on success and left alone on failure.
        unsafe {
            ProcessIdToSessionId(pid, &mut session);
        }
        println!("proc.pid          {pid}");
        println!("proc.session      {session}");

        // SAFETY: both return borrowed handles that must not be closed.
        let winsta = unsafe { GetProcessWindowStation() };
        let desk = unsafe { GetThreadDesktop(GetCurrentThreadId()) };
        println!("winsta            {}", object_name(winsta));
        println!("desktop           {}", object_name(desk));

        // The input desktop is the one the keyboard and mouse are attached to.
        // Opening it needs no privilege and failing to open it is itself the
        // answer: from a service window station it is `ERROR_INVALID_FUNCTION`,
        // and from the wrong desktop of the right station it is access denied.
        // SAFETY: `DESKTOP_READOBJECTS`; the handle is closed below.
        let input = unsafe { OpenInputDesktop(0, 0, 0x0001) };
        if input.is_null() {
            println!(
                "inputdesktop      <unreachable: {}>",
                std::io::Error::last_os_error()
            );
        } else {
            println!("inputdesktop      {}", object_name(input));
            // SAFETY: opened just above and not used afterwards.
            unsafe {
                CloseDesktop(input);
            }
        }

        print_dpi(dpi);

        // SAFETY: documented metric indices; these calls cannot fail.
        let (cx, cy, vx, vy, vw, vh) = unsafe {
            (
                GetSystemMetrics(SM_CXSCREEN),
                GetSystemMetrics(SM_CYSCREEN),
                GetSystemMetrics(SM_XVIRTUALSCREEN),
                GetSystemMetrics(SM_YVIRTUALSCREEN),
                GetSystemMetrics(SM_CXVIRTUALSCREEN),
                GetSystemMetrics(SM_CYVIRTUALSCREEN),
            )
        };
        println!("screen.primary    {cx}x{cy}");
        println!("screen.virtual    {vw}x{vh} at {vx},{vy}");

        // Whether a screen grab is even possible here. A service window station
        // has no display device behind it and this is where that shows up, as a
        // failing `BitBlt` rather than as a black image — which is worth
        // printing because "black" and "refused" are different diagnoses.
        match capture_screen() {
            Ok(cap) => println!("screen.capture    ok, {}x{}", cap.w, cap.h),
            Err(e) => println!("screen.capture    FAILS: {e}"),
        }
        Ok(())
    }

    // ------------------------------------------------------------- windows --

    struct Win {
        hwnd: HWND,
        title: String,
        class: String,
        rect: RECT,
        style: isize,
        exstyle: isize,
        owner: HWND,
        visible: bool,
        iconic: bool,
    }

    fn text_of(hwnd: HWND, f: unsafe extern "system" fn(HWND, *mut u16, i32) -> i32) -> String {
        let mut buf = [0u16; 512];
        // SAFETY: a live handle and a 512-`u16` buffer whose length is passed
        // in characters, as both entry points document.
        let n = unsafe { f(hwnd, buf.as_mut_ptr(), buf.len() as i32) };
        String::from_utf16_lossy(&buf[..n.max(0) as usize])
    }

    unsafe extern "system" fn collect(hwnd: HWND, lparam: LPARAM) -> i32 {
        // SAFETY: `lparam` is the `&mut Vec<Win>` handed to `EnumWindows` below,
        // which outlives the enumeration and is not aliased during it.
        let out = unsafe { &mut *(lparam as *mut Vec<Win>) };
        let mut rect = RECT { left: 0, top: 0, right: 0, bottom: 0 };
        // SAFETY: `hwnd` is live for the duration of the callback.
        unsafe {
            GetWindowRect(hwnd, &mut rect);
            out.push(Win {
                hwnd,
                title: text_of(hwnd, GetWindowTextW),
                class: text_of(hwnd, GetClassNameW),
                rect,
                style: GetWindowLongPtrW(hwnd, GWL_STYLE),
                exstyle: GetWindowLongPtrW(hwnd, GWL_EXSTYLE),
                owner: GetWindow(hwnd, GW_OWNER),
                visible: IsWindowVisible(hwnd) != 0,
                iconic: IsIconic(hwnd) != 0,
            });
        }
        1
    }

    fn top_level() -> Vec<Win> {
        let mut v: Vec<Win> = Vec::new();
        // SAFETY: `collect` matches `WNDENUMPROC` and `v` outlives the call.
        unsafe {
            EnumWindows(Some(collect), &mut v as *mut Vec<Win> as LPARAM);
        }
        v
    }

    // ------------------------------------------------------- taskbar identity --
    //
    // ADDED FOR STAGE 32, and it is the instrument rather than the fix: the fix
    // is in `wc-compositor`'s `win32.rs`, and this is how a run says what the
    // shell will actually read. The two declarations are deliberately
    // duplicated — this binary depends on nothing but `windows-sys`, which is
    // what lets it run on a guest where the compositor cannot, and a shared
    // crate for eight function pointers would cost that.
    //
    // windows-sys 0.61 generates no COM interfaces, so `IPropertyStore` is
    // transcribed from `propsys.h`; the GUID and the property key are from
    // `propsys.h` and `propkey.h`.

    const IID_PROPERTY_STORE: GUID = GUID::from_u128(0x886d8eeb_8cf2_4446_8d02_cdba1dbdcf99);
    const PKEY_APP_USER_MODEL_ID: PROPERTYKEY = PROPERTYKEY {
        fmtid: GUID::from_u128(0x9f4c2855_9f79_4b39_a8d0_e1d42de1d5f3),
        pid: 5,
    };
    const VT_LPWSTR: u16 = 31;

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct PropVariant {
        vt: u16,
        reserved: [u16; 3],
        value: [u64; 2],
    }

    #[repr(C)]
    struct PropertyStore {
        vtbl: *const PropertyStoreVtbl,
    }

    #[repr(C)]
    struct PropertyStoreVtbl {
        query_interface: unsafe extern "system" fn(
            *mut PropertyStore,
            *const GUID,
            *mut *mut core::ffi::c_void,
        ) -> HRESULT,
        add_ref: unsafe extern "system" fn(*mut PropertyStore) -> u32,
        release: unsafe extern "system" fn(*mut PropertyStore) -> u32,
        get_count: unsafe extern "system" fn(*mut PropertyStore, *mut u32) -> HRESULT,
        get_at: unsafe extern "system" fn(*mut PropertyStore, u32, *mut PROPERTYKEY) -> HRESULT,
        get_value: unsafe extern "system" fn(
            *mut PropertyStore,
            *const PROPERTYKEY,
            *mut PropVariant,
        ) -> HRESULT,
        set_value: unsafe extern "system" fn(
            *mut PropertyStore,
            *const PROPERTYKEY,
            *const PropVariant,
        ) -> HRESULT,
        commit: unsafe extern "system" fn(*mut PropertyStore) -> HRESULT,
    }

    /// The Application User Model ID set on this window, if it has one.
    ///
    /// `None` covers three different things and does not distinguish them,
    /// because for this instrument they mean the same: no property, an empty
    /// property, or a shell that refused the question. All three leave the
    /// window grouped by its process, which is the state this stage is about.
    fn aumid(hwnd: HWND) -> Option<String> {
        window_string(hwnd, 5)
    }

    /// §92. `PKEY_AppUserModel_RelaunchCommand`: what a pin made from this
    /// window would run. Present only on a window the launcher made pinnable.
    fn relaunch(hwnd: HWND) -> Option<String> {
        window_string(hwnd, 2)
    }

    /// One `PKEY_AppUserModel_*` string off a window's property store.
    fn window_string(hwnd: HWND, pid: u32) -> Option<String> {
        // Once per process, on this thread; `wc-desk` is a one-shot command and
        // never uninitialises.
        static ONCE: std::sync::Once = std::sync::Once::new();
        // SAFETY: a plain call with a null reserved argument.
        ONCE.call_once(|| unsafe {
            CoInitializeEx(std::ptr::null(), COINIT_APARTMENTTHREADED as u32);
        });
        let mut store: *mut PropertyStore = std::ptr::null_mut();
        // SAFETY: a live handle from `EnumWindows`; the out pointer is checked.
        let hr = unsafe {
            SHGetPropertyStoreForWindow(
                hwnd,
                &IID_PROPERTY_STORE,
                (&mut store as *mut *mut PropertyStore).cast(),
            )
        };
        if hr < 0 || store.is_null() {
            return None;
        }
        // SAFETY: `store` is live until `release`, which every path takes. The
        // string the store writes is `CoTaskMemAlloc`ed and freed here, which
        // is what `PropVariantClear` would do for the one type read.
        unsafe {
            let vtbl = &*(*store).vtbl;
            let mut pv = PropVariant { vt: 0, reserved: [0; 3], value: [0; 2] };
            let key = PROPERTYKEY { fmtid: PKEY_APP_USER_MODEL_ID.fmtid, pid };
            let hr = (vtbl.get_value)(store, &key, &mut pv);
            let out = if hr < 0 || pv.vt != VT_LPWSTR || pv.value[0] == 0 {
                None
            } else {
                let p = pv.value[0] as *mut u16;
                let mut n = 0usize;
                while *p.add(n) != 0 {
                    n += 1;
                }
                let s = String::from_utf16_lossy(std::slice::from_raw_parts(p, n));
                CoTaskMemFree(p.cast());
                if s.is_empty() { None } else { Some(s) }
            };
            (vtbl.release)(store);
            out
        }
    }

    /// Does Windows give this window a taskbar button?
    ///
    /// This is the shell's documented rule rather than a guess, and it is worth
    /// stating because `docs/goal-firefox.md` asks for a correct taskbar entry
    /// and the compositor gets one **by not doing anything**: a visible,
    /// unowned `WS_OVERLAPPEDWINDOW` without `WS_EX_TOOLWINDOW` is a taskbar
    /// entry, and every one of those clauses is already true of the windows
    /// `create_window` makes. `WS_EX_APPWINDOW` forces the button back on for a
    /// window that would otherwise be excluded.
    fn taskbar_entry(w: &Win) -> bool {
        if !w.visible {
            return false;
        }
        if w.exstyle & (WS_EX_APPWINDOW as isize) != 0 {
            return true;
        }
        w.owner.is_null() && w.exstyle & (WS_EX_TOOLWINDOW as isize) == 0
    }

    fn cmd_windows() -> std::io::Result<()> {
        let all = top_level();
        println!("{:<12} {:<5} {:<26} {:<20} title", "hwnd", "task", "class", "rect");
        for w in &all {
            if !w.visible && w.title.is_empty() {
                continue;
            }
            println!(
                "0x{:<10x} {:<5} {:<26} {:<20} {:?}{}{}{}",
                w.hwnd as usize,
                if taskbar_entry(w) { "yes" } else { "no" },
                w.class,
                format!(
                    "{}x{}+{}+{}",
                    w.rect.right - w.rect.left,
                    w.rect.bottom - w.rect.top,
                    w.rect.left,
                    w.rect.top
                ),
                w.title,
                if w.iconic { " [minimised]" } else { "" },
                // ADDED FOR STAGE 32, and appended rather than columned so that
                // every harness already grepping this table keeps working.
                match aumid(w.hwnd) {
                    Some(id) => format!(" aumid={id:?}"),
                    None => String::new(),
                },
                // §92: what a pin of this window would run.
                match relaunch(w.hwnd) {
                    Some(r) => format!(" relaunch={r:?}"),
                    None => String::new(),
                }
            );
        }
        // The claim stage 32 exists to make, counted rather than looked at: how
        // many taskbar *buttons* the windows on this desktop come to. Windows
        // with an identity of their own group by it; windows without one fall
        // back to their process, which is what every window this compositor
        // makes used to do.
        let mut groups: Vec<String> = all
            .iter()
            .filter(|w| taskbar_entry(w))
            .map(|w| match aumid(w.hwnd) {
                Some(id) => id,
                None => format!("(process {})", pid_of(w.hwnd)),
            })
            .collect();
        groups.sort();
        groups.dedup();
        println!("windows.groups    {}", groups.len());
        for g in &groups {
            println!("  group           {g}");
        }
        println!("windows.total     {}", all.len());
        println!(
            "windows.taskbar   {}",
            all.iter().filter(|w| taskbar_entry(w)).count()
        );
        // Printed even when it is zero: `WS_VISIBLE` on a window whose station
        // has no display is still `WS_VISIBLE`, so this is the count that a
        // capture has to agree with.
        println!(
            "windows.visible   {}",
            all.iter()
                .filter(|w| w.style & (WS_VISIBLE as isize) != 0)
                .count()
        );
        Ok(())
    }

    // ---------------------------------------------------------------- shot --

    struct Capture {
        w: i32,
        h: i32,
        /// Bottom-up 32-bit BGRX, exactly the layout a `.bmp` file wants, which
        /// is why nothing here reverses rows.
        bits: Vec<u8>,
    }

    /// A `BITMAPINFO` for a `w`x`h` bottom-up 32-bit `BI_RGB` surface.
    fn bmi(w: i32, h: i32) -> BITMAPINFO {
        let mut info: BITMAPINFO = unsafe { std::mem::zeroed() };
        info.bmiHeader = BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: w,
            // Positive: bottom-up. The compositor's own DIB is top-down
            // (negative) because Wayland's origin is top-left; this one is
            // bottom-up because a BMP file is, and the two must not be confused.
            biHeight: h,
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB,
            biSizeImage: 0,
            biXPelsPerMeter: 0,
            biYPelsPerMeter: 0,
            biClrUsed: 0,
            biClrImportant: 0,
        };
        info
    }

    /// Draw something into a `w`x`h` DIB section and take the bytes away.
    ///
    /// `draw` gets the memory DC with the section selected into it and returns
    /// whether it worked. Everything is released before the bits are copied out,
    /// because GDI batches and a `BitBlt` is not guaranteed to have landed until
    /// the DC is done with.
    fn into_dib(w: i32, h: i32, draw: impl FnOnce(HDC) -> bool) -> std::io::Result<Capture> {
        if w <= 0 || h <= 0 {
            return Err(std::io::Error::other(format!("empty rectangle {w}x{h}")));
        }
        let info = bmi(w, h);
        let mut bits: *mut c_void = std::ptr::null_mut();
        // SAFETY: a null reference DC asks for a DIB compatible with the
        // screen; `bits` receives the section's mapping, owned by the bitmap.
        let (dc, bitmap) = unsafe {
            let dc = CreateCompatibleDC(std::ptr::null_mut());
            let bitmap: HBITMAP = CreateDIBSection(
                std::ptr::null_mut(),
                &info,
                DIB_RGB_COLORS,
                &mut bits,
                std::ptr::null_mut(),
                0,
            );
            (dc, bitmap)
        };
        if dc.is_null() || bitmap.is_null() || bits.is_null() {
            let e = std::io::Error::last_os_error();
            // SAFETY: each handle is deleted only if it was created.
            unsafe {
                if !bitmap.is_null() {
                    DeleteObject(bitmap.cast());
                }
                if !dc.is_null() {
                    DeleteDC(dc);
                }
            }
            return Err(e);
        }
        // SAFETY: both handles are live and the old bitmap is put back below.
        let old = unsafe { SelectObject(dc, bitmap.cast()) };
        let ok = draw(dc);
        let err = std::io::Error::last_os_error();
        let len = (w as usize) * (h as usize) * 4;
        // SAFETY: `bits` points at exactly `len` bytes owned by `bitmap`, which
        // is still alive; the copy happens before either handle is released.
        let out = unsafe { std::slice::from_raw_parts(bits.cast::<u8>(), len).to_vec() };
        // SAFETY: restoring the original selection before deleting, as required.
        unsafe {
            SelectObject(dc, old);
            DeleteObject(bitmap.cast());
            DeleteDC(dc);
        }
        if !ok {
            return Err(err);
        }
        Ok(Capture { w, h, bits: out })
    }

    /// The whole virtual screen, through the desktop DC.
    ///
    /// This is the call that **fails** in an ssh session's window station, and
    /// that failure is the cleanest single proof that such a session has no
    /// display: `GetDC(NULL)` returns a DC there like anywhere else, and the
    /// `BitBlt` out of it comes back with `ERROR_ACCESS_DENIED`. Measured, not
    /// assumed — the first guess was `ERROR_INVALID_HANDLE`, and it was wrong.
    fn capture_screen() -> std::io::Result<Capture> {
        // SAFETY: documented metric indices.
        let (x, y, w, h) = unsafe {
            (
                GetSystemMetrics(SM_XVIRTUALSCREEN),
                GetSystemMetrics(SM_YVIRTUALSCREEN),
                GetSystemMetrics(SM_CXVIRTUALSCREEN),
                GetSystemMetrics(SM_CYVIRTUALSCREEN),
            )
        };
        // SAFETY: a null window is the desktop; the DC is released below.
        let screen = unsafe { GetDC(std::ptr::null_mut()) };
        let r = into_dib(w, h, |dc| {
            // `CAPTUREBLT` includes layered windows, which is most of a modern
            // desktop.
            // SAFETY: both DCs are live for the duration of the call.
            unsafe { BitBlt(dc, 0, 0, w, h, screen, x, y, SRCCOPY | CAPTUREBLT) != 0 }
        });
        // SAFETY: obtained from `GetDC` above.
        unsafe {
            ReleaseDC(std::ptr::null_mut(), screen);
        }
        r
    }

    /// One window, from DWM's redirection surface.
    fn capture_window(hwnd: HWND) -> std::io::Result<Capture> {
        let mut rect = RECT { left: 0, top: 0, right: 0, bottom: 0 };
        // SAFETY: a live window handle; the rectangle is written in place.
        unsafe {
            GetWindowRect(hwnd, &mut rect);
        }
        let (w, h) = (rect.right - rect.left, rect.bottom - rect.top);
        into_dib(w, h, |dc| {
            // `PW_RENDERFULLCONTENT` is the flag that makes this work for a
            // DWM-composited window and for one that is covered. Without it the
            // window is asked to paint itself into the DC, which loses the
            // non-client frame — and the non-client frame is the thing §8.11
            // item 2 is about.
            // SAFETY: a live window and a live DC.
            unsafe { PrintWindow(hwnd, dc, PW_RENDERFULLCONTENT) != 0 }
        })
    }

    fn write_bmp(path: &str, cap: &Capture) -> std::io::Result<()> {
        let header = 14 + 40;
        let size = header + cap.bits.len();
        let mut out = Vec::with_capacity(size);
        out.extend_from_slice(b"BM");
        out.extend_from_slice(&(size as u32).to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes()); // both reserved words
        out.extend_from_slice(&(header as u32).to_le_bytes());
        out.extend_from_slice(&40u32.to_le_bytes());
        out.extend_from_slice(&cap.w.to_le_bytes());
        out.extend_from_slice(&cap.h.to_le_bytes());
        out.extend_from_slice(&1u16.to_le_bytes());
        out.extend_from_slice(&32u16.to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes()); // BI_RGB
        out.extend_from_slice(&(cap.bits.len() as u32).to_le_bytes());
        out.extend_from_slice(&[0u8; 16]); // resolution, palette counts
        out.extend_from_slice(&cap.bits);
        std::fs::write(path, out)
    }

    /// Restrict a search to one process.
    ///
    /// # Why this is not optional in practice
    ///
    /// `--class WaycomposeToplevel` selects *a* compositor window, and on this
    /// guest there is routinely more than one: another worktree drives the same
    /// machine at the same time, its compositor registers the same window class,
    /// and its `weston-terminal` has the same caption and very nearly the same
    /// size. A run that selected by class alone therefore photographed somebody
    /// else's window, compared it against its own frame dumps, got a large RMSE
    /// in both orientations and reported "undecided" — and, worse, put *their*
    /// window into a modal resize loop. Both symptoms read as flakiness.
    ///
    /// So the pid is the selector and the class is the filter, never the other
    /// way round.
    fn pid_of(hwnd: HWND) -> u32 {
        let mut pid: u32 = 0;
        // SAFETY: a live window handle; `pid` is written by the call.
        unsafe {
            GetWindowThreadProcessId(hwnd, &mut pid);
        }
        pid
    }

    fn find_window(args: &[String]) -> std::io::Result<HWND> {
        let want_pid: Option<u32> = opt(args, "--pid").and_then(|p| p.parse().ok());
        if let Some(pid) = want_pid {
            let class = opt(args, "--class");
            let hit = top_level().into_iter().find(|w| {
                w.visible
                    && pid_of(w.hwnd) == pid
                    && class.map(|c| w.class == c).unwrap_or(true)
            });
            return match hit {
                Some(w) => Ok(w.hwnd),
                None => Err(std::io::Error::other(format!(
                    "no visible window belonging to pid {pid}"
                ))),
            };
        }
        if let Some(h) = opt(args, "--hwnd") {
            let h = h.trim_start_matches("0x");
            let v = usize::from_str_radix(h, 16)
                .map_err(|e| std::io::Error::other(format!("--hwnd: {e}")))?;
            return Ok(v as HWND);
        }
        if let Some(c) = opt(args, "--class") {
            let w: Vec<u16> = c.encode_utf16().chain(std::iter::once(0)).collect();
            // SAFETY: a live NUL-terminated class name; a null title matches any.
            let h = unsafe { FindWindowW(w.as_ptr(), std::ptr::null()) };
            if h.is_null() {
                return Err(std::io::Error::other(format!("no window of class {c:?}")));
            }
            return Ok(h);
        }
        if let Some(t) = opt(args, "--title") {
            // Visible windows only, and the topmost match: `EnumWindows`
            // enumerates in z-order, so a stale hidden window of the same title
            // cannot shadow the one on screen.
            let hit = top_level()
                .into_iter()
                .find(|w| w.visible && w.title.contains(t));
            return match hit {
                Some(w) => Ok(w.hwnd),
                None => Err(std::io::Error::other(format!(
                    "no visible window whose title contains {t:?}"
                ))),
            };
        }
        Err(std::io::Error::other("shot: need --title, --class, --hwnd or --screen"))
    }

    /// §96: the part of a window capture that DWM actually draws.
    ///
    /// `GetWindowRect` on Windows 10 and 11 includes the invisible resize
    /// borders, 8 px left, right and bottom at 96 dpi, and `PrintWindow` leaves
    /// them black. The first Minecraft captures on the guest showed exactly that,
    /// and it looked like a client that did not fill its window. It wasn't one:
    /// the client area was 854x480, as the game said. `DWMWA_EXTENDED_FRAME_BOUNDS`
    /// is the drawn rectangle, in physical pixels like the per-monitor-aware
    /// `GetWindowRect` it is cropped against.
    fn crop_to_visible(hwnd: HWND, rect: &RECT, cap: Capture) -> std::io::Result<Capture> {
        let mut ext = RECT { left: 0, top: 0, right: 0, bottom: 0 };
        // SAFETY: a live window; the attribute is a RECT and its size is passed.
        let hr = unsafe {
            DwmGetWindowAttribute(
                hwnd,
                DWMWA_EXTENDED_FRAME_BOUNDS as u32,
                (&mut ext as *mut RECT).cast(),
                std::mem::size_of::<RECT>() as u32,
            )
        };
        if hr != 0 {
            return Err(std::io::Error::other(format!("DwmGetWindowAttribute: 0x{hr:08x}")));
        }
        let x0 = (ext.left - rect.left).clamp(0, cap.w);
        let y0 = (ext.top - rect.top).clamp(0, cap.h);
        let x1 = (ext.right - rect.left).clamp(x0, cap.w);
        let y1 = (ext.bottom - rect.top).clamp(y0, cap.h);
        let (w, h) = (x1 - x0, y1 - y0);
        let mut bits = Vec::with_capacity((w * h * 4) as usize);
        // Bottom-up rows in both: the new bottom row is the old row `y1 - 1`.
        for j in 0..h {
            let src_row = (cap.h - y1 + j) as usize;
            let start = src_row * cap.w as usize * 4 + x0 as usize * 4;
            bits.extend_from_slice(&cap.bits[start..start + w as usize * 4]);
        }
        Ok(Capture { w, h, bits })
    }

    fn cmd_shot(args: &[String], dpi: &Dpi) -> std::io::Result<()> {
        let out = opt(args, "--out").ok_or_else(|| std::io::Error::other("shot: --out is required"))?;
        let (cap, what) = if args.iter().any(|a| a == "--screen") {
            (capture_screen()?, "screen".to_string())
        } else {
            let hwnd = find_window(args)?;
            let mut rect = RECT { left: 0, top: 0, right: 0, bottom: 0 };
            // SAFETY: a live window handle.
            unsafe {
                GetWindowRect(hwnd, &mut rect);
            }
            let mut cap = capture_window(hwnd)?;
            if args.iter().any(|a| a == "--visible") {
                cap = crop_to_visible(hwnd, &rect, cap)?;
            }
            (
                cap,
                format!(
                    "hwnd 0x{:x} at {},{} title {:?}",
                    hwnd as usize,
                    rect.left,
                    rect.top,
                    text_of(hwnd, GetWindowTextW)
                ),
            )
        };
        write_bmp(out, &cap)?;
        println!(
            "shot              {} -> {out} ({}x{}, {})",
            what,
            cap.w,
            cap.h,
            awareness_name(dpi.after)
        );
        Ok(())
    }

    // ---------------------------------------------------------------- drag --

    /// Put a window into the modal sizing loop and drive it, from outside.
    ///
    /// # What is being tested, and why it needs a second process
    ///
    /// docs/waycompose-design.md §8.2 chose to wake the UI thread with a `PostMessage` to a
    /// **message-only window** rather than a `PostThreadMessage`, because a
    /// thread message has no `hwnd` and the modal loops `DefWindowProc` runs
    /// during a window drag or resize discard it. §8.11 item 4 is the check that
    /// the choice was right, and it says "dragging a border by hand", which is
    /// why nothing automated had ever run it: the loop is entered by the *user*.
    ///
    /// It is not only enterable by hand. `WM_SYSCOMMAND`/`SC_SIZE` is the same
    /// entry point the system menu's "Size" uses, and once inside, the arrow
    /// keys resize — all of it driven from the window's own message queue, so
    /// `PostMessageW` from another process drives it exactly as a keyboard does.
    /// This is therefore the real modal loop, entered the real way, and not a
    /// simulation of one.
    ///
    /// The measurement is not here. This command holds the window inside the
    /// loop for `--hold-ms` and prints the rectangle either side; what happens to
    /// *frames* during that window is counted by whoever started the compositor
    /// with `--dump-frames`, because a compositor that has stopped repainting is
    /// a compositor that has stopped dumping.
    fn cmd_drag(args: &[String]) -> std::io::Result<()> {
        // `SC_SIZE` is documented as needing its low four bits free for the
        // direction; 8 is `WMSZ_BOTTOMRIGHT`, the corner a user grabs.
        const WMSZ_BOTTOMRIGHT: usize = 8;
        const VK_RIGHT: usize = 0x27;
        const VK_DOWN: usize = 0x28;
        const VK_RETURN: usize = 0x0D;

        let hwnd = find_window(args)?;
        let steps: usize = opt(args, "--steps").unwrap_or("24").parse().unwrap_or(24);
        let hold: u64 = opt(args, "--hold-ms").unwrap_or("6000").parse().unwrap_or(6000);

        let rect = |h| {
            let mut r = RECT { left: 0, top: 0, right: 0, bottom: 0 };
            // SAFETY: a live window handle; the rectangle is written in place.
            unsafe {
                GetWindowRect(h, &mut r);
            }
            r
        };
        let before = rect(hwnd);
        println!(
            "drag.before       {}x{} at {},{}",
            before.right - before.left,
            before.bottom - before.top,
            before.left,
            before.top
        );

        // **There is no way to ask whether the loop was entered while it is
        // running**, and two attempts at one were wrong before this comment was
        // written. `GetWindowRect` does not move during keyboard sizing —
        // Windows tracks a rectangle and applies it on Return, so the window is
        // still its old size all the way through, which is why `drag.during`
        // below always reads unchanged and is printed anyway. And the first
        // arrow key only chooses an edge; it does not resize. So a probe of the
        // form "post `SC_SIZE`, tap a key, did the frame move" answers *no* for
        // a loop that is working perfectly, and a retry built on it makes things
        // worse by posting a second `SC_SIZE` into the queue of a loop that is
        // already running.
        //
        // The only honest test is the whole sequence: enter, resize, commit,
        // and see whether the window ended up bigger. `drag.entered` is
        // therefore the same fact as `drag.grew`, reported separately because
        // the caller has to tell "the loop did not start" (skip, and say so)
        // from "the loop started and the compositor misbehaved" (fail).
        //
        // SAFETY: a live window handle. `PostMessageW` never blocks, which is
        // the point: `SendMessageW` here would block this process for as long as
        // the modal loop runs, which is what is being held open.
        unsafe {
            PostMessageW(hwnd, WM_SYSCOMMAND, SC_SIZE as usize + WMSZ_BOTTOMRIGHT, 0);
        }
        std::thread::sleep(std::time::Duration::from_millis(500));

        // Inside the loop now. Every one of these is a resize step, and each one
        // makes the compositor send an `xdg_toplevel.configure` — which is the
        // half of the negotiation §8.3 describes that a stuck loop would stop.
        for _ in 0..steps {
            // SAFETY: a live window handle.
            unsafe {
                PostMessageW(hwnd, WM_KEYDOWN, VK_RIGHT, 0);
                PostMessageW(hwnd, WM_KEYUP, VK_RIGHT, 0);
                PostMessageW(hwnd, WM_KEYDOWN, VK_DOWN, 0);
                PostMessageW(hwnd, WM_KEYUP, VK_DOWN, 0);
            }
            std::thread::sleep(std::time::Duration::from_millis(30));
        }
        let during = rect(hwnd);
        println!(
            "drag.during       {}x{}  (still inside the modal loop)",
            during.right - during.left,
            during.bottom - during.top
        );

        // Is the thread still answering while it is in there? A modal loop that
        // pumps is one that can still dispatch our wake message; one that does
        // not would show up here as a timeout, and the two are worth separating.
        let mut result: usize = 0;
        // SAFETY: a live window handle; `WM_NULL` has no side effects, and the
        // result pointer is written only on success.
        let answered = unsafe {
            SendMessageTimeoutW(hwnd, WM_NULL, 0, 0, SMTO_ABORTIFHUNG, 1000, &mut result)
        };
        println!(
            "drag.responsive   {}",
            if answered != 0 { "yes" } else { "NO — the loop is not pumping" }
        );

        std::thread::sleep(std::time::Duration::from_millis(hold));

        // SAFETY: a live window handle; Return commits the size and leaves the
        // loop, which is what a user releasing the mouse does.
        unsafe {
            PostMessageW(hwnd, WM_KEYDOWN, VK_RETURN, 0);
            PostMessageW(hwnd, WM_KEYUP, VK_RETURN, 0);
        }
        std::thread::sleep(std::time::Duration::from_millis(600));
        let after = rect(hwnd);
        println!(
            "drag.after        {}x{}",
            after.right - after.left,
            after.bottom - after.top
        );
        // Area, not both dimensions. Windows' *keyboard* sizing picks an edge
        // from the first arrow key and then only tracks the perpendicular axis,
        // so whether the window grows in one direction or two depends on which
        // key the loop saw first — and that is a fact about `user32`, not about
        // the compositor. Growing at all is what proves the loop was entered.
        let area = |r: &RECT| (r.right - r.left) as i64 * (r.bottom - r.top) as i64;
        let grew = area(&after) > area(&before);
        println!("drag.entered      {}", if grew { "yes" } else { "no" });
        println!("drag.grew         {}", if grew { "yes" } else { "no" });
        Ok(())
    }

    // ------------------------------------------------------ §82: the caption --

    /// A window-relative point, `X,Y`. A negative coordinate counts from the
    /// far edge — `-20,15` is twenty pixels in from the right — because the
    /// points worth asking about (the close button, the top edge) are fixed
    /// relative to an edge and the window's size is the client's choice.
    fn at(spec: &str, r: &RECT) -> std::io::Result<(i32, i32)> {
        let (x, y) = spec
            .split_once(',')
            .ok_or_else(|| std::io::Error::other(format!("{spec:?}: want X,Y")))?;
        let n = |s: &str| {
            s.trim().parse::<i32>().map_err(|e| std::io::Error::other(format!("{spec:?}: {e}")))
        };
        let (x, y) = (n(x)?, n(y)?);
        let x = if x < 0 { r.right - r.left + x } else { x };
        let y = if y < 0 { r.bottom - r.top + y } else { y };
        Ok((r.left + x, r.top + y))
    }

    fn ht_name(v: isize) -> &'static str {
        match v {
            -2 => "HTTRANSPARENT",
            0 => "HTNOWHERE",
            1 => "HTCLIENT",
            2 => "HTCAPTION",
            3 => "HTSYSMENU",
            8 => "HTMINBUTTON",
            9 => "HTMAXBUTTON",
            10 => "HTLEFT",
            11 => "HTRIGHT",
            12 => "HTTOP",
            13 => "HTTOPLEFT",
            14 => "HTTOPRIGHT",
            15 => "HTBOTTOM",
            16 => "HTBOTTOMLEFT",
            17 => "HTBOTTOMRIGHT",
            20 => "HTCLOSE",
            _ => "?",
        }
    }

    /// Ask the window what is under each point: `WM_NCHITTEST`, exactly as
    /// Windows asks it before every mouse message.
    ///
    /// ADDED FOR §82, and it is the test §31.13 item 4 said nobody had: the
    /// compositor's answer for the top edge, the caption buttons and the empty
    /// tab strip of a folded window, read back from the window procedure
    /// itself rather than inferred from a cursor shape in a photograph. On the
    /// guest, the answers over DWM's buttons come from `DwmDefWindowProc`;
    /// under Wine from the compositor's own fallback. Either way the line says
    /// what Windows will do with a click there.
    fn cmd_hittest(args: &[String]) -> std::io::Result<()> {
        let hwnd = find_window(args)?;
        let mut r = RECT { left: 0, top: 0, right: 0, bottom: 0 };
        // SAFETY: a live window handle; the rectangle is written in place.
        unsafe { GetWindowRect(hwnd, &mut r) };
        println!(
            "hittest.window    {}x{} at {},{} zoomed={}",
            r.right - r.left,
            r.bottom - r.top,
            r.left,
            r.top,
            // SAFETY: a live window handle.
            unsafe { IsZoomed(hwnd) } != 0
        );
        let mut i = 0;
        while i < args.len() {
            if args[i] == "--at"
                && let Some(spec) = args.get(i + 1)
            {
                let (sx, sy) = at(spec, &r)?;
                let lp = ((sy as u32 & 0xffff) << 16 | (sx as u32 & 0xffff)) as LPARAM;
                let mut result: usize = 0;
                // SAFETY: a live window handle; `result` is written on success.
                let ok = unsafe {
                    SendMessageTimeoutW(hwnd, WM_NCHITTEST, 0, lp, SMTO_ABORTIFHUNG, 2000, &mut result)
                };
                let v = result as isize;
                if ok == 0 {
                    println!("hittest {spec:>10} screen {sx},{sy} -> (no answer)");
                } else {
                    println!("hittest {spec:>10} screen {sx},{sy} -> {} ({v})", ht_name(v));
                }
                i += 2;
            } else {
                i += 1;
            }
        }
        Ok(())
    }

    /// One mouse event through `SendInput`, at a screen point.
    fn send_mouse(x: i32, y: i32, flags: u32) {
        // SAFETY: plain metric reads.
        let (w, h) = unsafe { (GetSystemMetrics(SM_CXSCREEN), GetSystemMetrics(SM_CYSCREEN)) };
        let nx = (i64::from(x) * 65535 / i64::from((w - 1).max(1))) as i32;
        let ny = (i64::from(y) * 65535 / i64::from((h - 1).max(1))) as i32;
        let input = INPUT {
            r#type: INPUT_MOUSE,
            Anonymous: INPUT_0 {
                mi: MOUSEINPUT {
                    dx: nx,
                    dy: ny,
                    mouseData: 0,
                    dwFlags: flags | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_MOVE,
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        };
        // SAFETY: one well-formed `INPUT`, its size passed with it.
        unsafe { SendInput(1, &input, std::mem::size_of::<INPUT>() as i32) };
    }

    /// §96: relative motion, which `SendInput` without `MOUSEEVENTF_ABSOLUTE`
    /// produces exactly as a mouse would, raw input included.
    fn send_rel(dx: i32, dy: i32) {
        let input = INPUT {
            r#type: INPUT_MOUSE,
            Anonymous: INPUT_0 {
                mi: MOUSEINPUT { dx, dy, mouseData: 0, dwFlags: MOUSEEVENTF_MOVE, time: 0, dwExtraInfo: 0 },
            },
        };
        // SAFETY: one well-formed `INPUT`, its size passed with it.
        unsafe { SendInput(1, &input, std::mem::size_of::<INPUT>() as i32) };
    }

    /// §96: one key transition, by scan code.
    fn send_key(scan: u16, up: bool) {
        let input = INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: 0,
                    wScan: scan,
                    dwFlags: KEYEVENTF_SCANCODE | if up { KEYEVENTF_KEYUP } else { 0 },
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        };
        // SAFETY: one well-formed `INPUT`, its size passed with it.
        unsafe { SendInput(1, &input, std::mem::size_of::<INPUT>() as i32) };
    }

    /// §96: the keys a game test needs, by set-1 scan code. Deliberately a
    /// short list, and no Windows key: nothing here may reach the shell.
    fn scan_code(name: &str) -> std::io::Result<u16> {
        Ok(match name.to_ascii_uppercase().as_str() {
            "ESC" | "ESCAPE" => 0x01,
            "W" => 0x11,
            "A" => 0x1e,
            "S" => 0x1f,
            "D" => 0x20,
            "E" => 0x12,
            "SPACE" => 0x39,
            "F1" => 0x3b,
            "F2" => 0x3c,
            "F3" => 0x3d,
            "F11" => 0x57,
            other => return Err(std::io::Error::other(format!("--key: no key {other:?}"))),
        })
    }

    /// Drive the mouse over a window: `--drag A B`, `--click A`, `--dblclick A`,
    /// each point window-relative as for `hittest`, in the order given.
    ///
    /// ADDED FOR §82. §22.11 item 1 — *nothing has been dragged* — has been
    /// open since stage 17, because a drag of a client-decorated window is a
    /// round trip nothing on this side can fake: the press reaches the client,
    /// the client decides it was on its title bar and asks for
    /// `xdg_toplevel.move`, the compositor hands that to Windows' modal loop,
    /// and only then do the remaining moves move the window. So this is real
    /// input, through `SendInput`, with the button held across enough small
    /// steps for that round trip to happen, and the window rectangle printed
    /// before and after each gesture. A window that did not follow says so.
    fn cmd_mouse(args: &[String]) -> std::io::Result<()> {
        let hwnd = find_window(args)?;
        let step_ms: u64 = opt(args, "--step-ms").unwrap_or("40").parse().unwrap_or(40);
        let rect = || {
            let mut r = RECT { left: 0, top: 0, right: 0, bottom: 0 };
            // SAFETY: a live window handle; the rectangle is written in place.
            unsafe { GetWindowRect(hwnd, &mut r) };
            r
        };
        let say = |what: &str, r: &RECT| {
            println!(
                "mouse.{what:<10} {}x{} at {},{} zoomed={}",
                r.right - r.left,
                r.bottom - r.top,
                r.left,
                r.top,
                // SAFETY: a live window handle.
                unsafe { IsZoomed(hwnd) } != 0
            );
        };
        let pause = |ms: u64| std::thread::sleep(std::time::Duration::from_millis(ms));
        // SAFETY: a live window handle; refused while another window is in the
        // foreground on some desktops, which the printed rectangles will show.
        unsafe { SetForegroundWindow(hwnd) };
        pause(300);
        let mut i = 0;
        while i < args.len() {
            let r = rect();
            match args[i].as_str() {
                "--drag" if i + 2 < args.len() => {
                    let (x0, y0) = at(&args[i + 1], &r)?;
                    let (x1, y1) = at(&args[i + 2], &r)?;
                    say("before", &r);
                    send_mouse(x0, y0, 0);
                    pause(step_ms * 4);
                    send_mouse(x0, y0, MOUSEEVENTF_LEFTDOWN);
                    pause(step_ms * 4);
                    let n = 24;
                    for s in 1..=n {
                        let x = x0 + (x1 - x0) * s / n;
                        let y = y0 + (y1 - y0) * s / n;
                        send_mouse(x, y, 0);
                        pause(step_ms);
                    }
                    pause(step_ms * 4);
                    send_mouse(x1, y1, MOUSEEVENTF_LEFTUP);
                    pause(600);
                    say("after-drag", &rect());
                    i += 3;
                }
                "--click" | "--dblclick" if i + 1 < args.len() => {
                    let (x, y) = at(&args[i + 1], &r)?;
                    say("before", &r);
                    let times = if args[i] == "--dblclick" { 2 } else { 1 };
                    send_mouse(x, y, 0);
                    pause(step_ms * 4);
                    for _ in 0..times {
                        send_mouse(x, y, MOUSEEVENTF_LEFTDOWN);
                        pause(30);
                        send_mouse(x, y, MOUSEEVENTF_LEFTUP);
                        pause(60);
                    }
                    pause(1200);
                    say(if times == 2 { "after-dbl" } else { "after-clk" }, &rect());
                    i += 2;
                }
                // Park the pointer and wait, with no button down: what raises
                // Windows 11's snap-layouts flyout over a maximise button. The
                // flyout is a window of the shell's, so `shot --screen` taken
                // during the wait is what photographs it.
                // §96: a key, by its scan code, which is what the compositor
                // translates (`crate::input` in wc-compositor) and what a game
                // reads positionally.
                "--key" if i + 1 < args.len() => {
                    let sc = scan_code(&args[i + 1])?;
                    send_key(sc, false);
                    pause(60);
                    send_key(sc, true);
                    println!("mouse.key        {} (scan {sc:#04x})", args[i + 1]);
                    pause(step_ms * 4);
                    i += 2;
                }
                "--hold" if i + 2 < args.len() => {
                    let sc = scan_code(&args[i + 1])?;
                    let ms: u64 = args[i + 2].parse().unwrap_or(1000);
                    send_key(sc, false);
                    pause(ms);
                    send_key(sc, true);
                    println!("mouse.hold       {} for {ms} ms", args[i + 1]);
                    pause(step_ms * 4);
                    i += 3;
                }
                // §96: N relative moves of (DX, DY): what a mouse does, and the
                // only motion a locked pointer reports (raw input).
                "--rel" if i + 3 < args.len() => {
                    let dx: i32 = args[i + 1].parse().unwrap_or(0);
                    let dy: i32 = args[i + 2].parse().unwrap_or(0);
                    let n: u32 = args[i + 3].parse().unwrap_or(1);
                    for _ in 0..n {
                        send_rel(dx, dy);
                        pause(step_ms);
                    }
                    println!("mouse.rel        {n} x ({dx},{dy})");
                    i += 4;
                }
                "--wait" if i + 1 < args.len() => {
                    pause(args[i + 1].parse().unwrap_or(1000));
                    i += 2;
                }
                "--hover" if i + 2 < args.len() => {
                    let (x, y) = at(&args[i + 1], &r)?;
                    let ms: u64 = args[i + 2].parse().unwrap_or(1500);
                    send_mouse(x, y, 0);
                    println!("mouse.hover      {},{} for {ms} ms", x, y);
                    pause(ms);
                    i += 3;
                }
                _ => i += 1,
            }
        }
        Ok(())
    }

    // ---------------------------------------------------------------- tick --

    fn now(freq: i64) -> f64 {
        let mut t: i64 = 0;
        // SAFETY: `t` is written by the call, which cannot fail on any machine
        // this runs on.
        unsafe {
            QueryPerformanceCounter(&mut t);
        }
        t as f64 * 1000.0 / freq as f64
    }

    fn stats(v: &mut [f64]) -> (f64, f64, f64) {
        v.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let mean = v.iter().sum::<f64>() / v.len() as f64;
        (mean, v[v.len() / 2], v[v.len() - 1])
    }

    /// What a short wait actually costs, and what the alternatives cost.
    ///
    /// # Why this measures three things and not one
    ///
    /// §10.8 measured `Sleep(1)` at 15.2 ms and stopped there. That number on
    /// its own does not decide anything, because the loop it matters to sleeps
    /// **5** ms and the question is what to replace that with. So:
    ///
    /// * `Sleep(1)` and `Sleep(5)` — the current behaviour, and the pair that
    ///   shows the cost is a *quantum* rather than a per-millisecond overhead:
    ///   if both land on the same number, everything below one tick is free and
    ///   everything above it is rounded up.
    /// * a **high-resolution waitable timer**
    ///   (`CREATE_WAITABLE_TIMER_HIGH_RESOLUTION`, Windows 10 1803) — the
    ///   documented way to wait for less than a tick. It is the alternative the
    ///   user's ruling leaves open, because unlike `timeBeginPeriod` it does not
    ///   raise the *global* timer resolution: the effect is scoped to the wait,
    ///   so it costs this process latency rather than costing every process on
    ///   the machine its idle power.
    /// * `Sleep(0)` — the control. It yields without waiting, so it isolates the
    ///   scheduler quantum from the loop's own overhead, and if it is also 15 ms
    ///   then the measurement is wrong rather than the machine slow.
    fn cmd_tick(args: &[String]) -> std::io::Result<()> {
        let n: usize = opt(args, "--samples")
            .unwrap_or("20")
            .parse()
            .map_err(|e| std::io::Error::other(format!("--samples: {e}")))?;
        let mut freq: i64 = 0;
        // SAFETY: `freq` is written by the call.
        unsafe {
            QueryPerformanceFrequency(&mut freq);
        }
        println!("qpc.frequency     {freq} Hz");

        for ms in [0u32, 1, 5, 16] {
            let mut v = Vec::with_capacity(n);
            for _ in 0..n {
                let t0 = now(freq);
                // SAFETY: `Sleep` has no failure mode.
                unsafe {
                    Sleep(ms);
                }
                v.push(now(freq) - t0);
            }
            let (mean, med, max) = stats(&mut v);
            println!("{:<18}mean {mean:7.3} ms  median {med:7.3} ms  max {max:7.3} ms", format!("sleep({ms})"));
        }

        // The high-resolution waitable timer. A negative due time is relative
        // and in 100 ns units, which is the same unit `QueryPerformanceCounter`
        // resolves to on this machine (§10.8) — so if this does not work, it is
        // not because the clock cannot express it.
        // SAFETY: a null name and null security attributes; the handle is
        // closed by the process exiting, and a failure comes back as null.
        let timer = unsafe {
            CreateWaitableTimerExW(
                std::ptr::null(),
                std::ptr::null(),
                CREATE_WAITABLE_TIMER_HIGH_RESOLUTION,
                TIMER_ALL_ACCESS,
            )
        };
        if timer.is_null() {
            println!(
                "hrtimer           unavailable: {}",
                std::io::Error::last_os_error()
            );
            return Ok(());
        }
        for ms in [1i64, 5] {
            let mut v = Vec::with_capacity(n);
            for _ in 0..n {
                let due = -(ms * 10_000);
                let t0 = now(freq);
                // SAFETY: a live timer handle, a live due time, and no
                // completion routine.
                unsafe {
                    SetWaitableTimer(timer, &due, 0, None, std::ptr::null(), 0);
                    WaitForSingleObject(timer, 0xFFFF_FFFF);
                }
                v.push(now(freq) - t0);
            }
            let (mean, med, max) = stats(&mut v);
            println!("{:<18}mean {mean:7.3} ms  median {med:7.3} ms  max {max:7.3} ms", format!("hrtimer({ms})"));
        }
        Ok(())
    }
}
