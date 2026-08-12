//! waycompose, stage 2: a **smithay** compositor that runs as a Windows binary.
//!
//! Stage 1 answered "does the `wayland-server` stack run as an `.exe`?" by
//! hand-writing a server against the raw backend. This answers the next
//! question, which is the one that decides whether the project writes a
//! compositor or reuses one: **does smithay itself work there?**
//!
//! The difference between the two is not size, it is what fails if the answer is
//! no. Stage 1's server was ours; if it had broken we would have fixed it.
//! smithay is 96,004 lines of somebody else's code, of which 44,728 already
//! compiled for Windows and are exactly the compositor logic this project would
//! otherwise have to write (docs/design.md §5.2). Standing it up is the
//! difference between porting and reimplementing.
//!
//! What this binary does:
//!
//! * creates a smithay `Display`, `CompositorState`, `ShmState` and
//!   `XdgShellState`, so the globals a real client looks for are the ones
//!   smithay itself advertises rather than hand-rolled stand-ins;
//! * accepts clients on an AF_UNIX socket and inserts them;
//! * runs smithay's own `xdg_shell` implementation, so a client that binds
//!   `xdg_wm_base`, creates a `wl_surface`, wraps it in an `xdg_surface` and
//!   asks for an `xdg_toplevel` gets the real protocol — including the
//!   configure/ack handshake, which this sends;
//! * prints one greppable line per protocol milestone so a test script can
//!   assert on the sequence rather than on an exit code.
//!
//! What it does not do: draw anything. There is no renderer, no HWND and no
//! output. `wl_surface.commit` with a buffer attached is acknowledged and the
//! buffer released. Pixels are the next stage.
//!
//! # The event loop is still a poll loop, and still deliberately
//!
//! Every smithay compositor nests its `Display` inside `calloop` as one source,
//! using `Backend::poll_fd()`. That does not work on Windows: the Windows
//! counterpart returns a wepoll handle, wepoll polls sockets, and a wepoll
//! handle is not a socket (docs/design.md §3.3). The settled answer is the
//! two-thread design — a Win32 message pump on one thread, Wayland dispatch on
//! another — and it is not needed until there are windows to pump messages for.
//! Until then this loop is honest about doing nothing clever, exactly as stage
//! 1's was.

use std::sync::Arc;
use std::time::{Duration, Instant};

use smithay::input::{Seat, SeatHandler, SeatState};
use smithay::reexports::wayland_server::backend::{ClientData, ClientId, DisconnectReason};
use smithay::reexports::wayland_server::protocol::{wl_buffer, wl_seat, wl_surface::WlSurface};
use smithay::reexports::wayland_server::{Client, Display, ListeningSocket};
use smithay::wayland::buffer::BufferHandler;
use smithay::wayland::compositor::{
    CompositorClientState, CompositorHandler, CompositorState,
};
use smithay::wayland::shell::xdg::{
    PopupSurface, PositionerState, ToplevelSurface, XdgShellHandler, XdgShellState,
};
use smithay::wayland::shm::{ShmHandler, ShmState};
use smithay::{delegate_compositor, delegate_seat, delegate_shm, delegate_xdg_shell};

/// Everything the compositor knows.
///
/// Four smithay state structs and two counters. The counters exist so `--once`
/// can distinguish "no client yet" from "the client has gone", and so the test
/// script can assert that a toplevel was actually created rather than merely
/// that nothing crashed.
struct State {
    compositor: CompositorState,
    shm: ShmState,
    xdg_shell: XdgShellState,
    seat_state: SeatState<Self>,

    clients_seen: usize,
    toplevels: usize,
}

/// Per-client data.
///
/// `CompositorClientState` is not optional: smithay's compositor stores
/// per-client protocol state in it and expects the compositor to hang it off
/// its own `ClientData` so it is dropped with the client.
#[derive(Default)]
struct ClientState {
    compositor_state: CompositorClientState,
}

impl ClientData for ClientState {
    fn initialized(&self, client_id: ClientId) {
        println!("wc-compositor: client {client_id:?} initialized");
    }

    fn disconnected(&self, client_id: ClientId, reason: DisconnectReason) {
        println!("wc-compositor: client {client_id:?} disconnected: {reason:?}");
    }
}

impl CompositorHandler for State {
    fn compositor_state(&mut self) -> &mut CompositorState {
        &mut self.compositor
    }

    fn client_compositor_state<'a>(&self, client: &'a Client) -> &'a CompositorClientState {
        &client.get_data::<ClientState>().unwrap().compositor_state
    }

    fn new_surface(&mut self, _surface: &WlSurface) {
        println!("wc-compositor: wl_surface created");
    }

    fn commit(&mut self, _surface: &WlSurface) {
        // With no renderer there is nothing to do on commit but say so. The
        // *interesting* part is that we get here at all: reaching `commit` means
        // the surface, its role and its pending state all round-tripped through
        // smithay's own `compositor` module on Windows.
        println!("wc-compositor: wl_surface commit");
    }
}

impl BufferHandler for State {
    fn buffer_destroyed(&mut self, _buffer: &wl_buffer::WlBuffer) {}
}

impl ShmHandler for State {
    fn shm_state(&self) -> &ShmState {
        &self.shm
    }
}

impl SeatHandler for State {
    type KeyboardFocus = WlSurface;
    type PointerFocus = WlSurface;
    type TouchFocus = WlSurface;

    fn seat_state(&mut self) -> &mut SeatState<Self> {
        &mut self.seat_state
    }

    fn focus_changed(&mut self, _seat: &Seat<Self>, _focused: Option<&WlSurface>) {}
    fn cursor_image(
        &mut self,
        _seat: &Seat<Self>,
        _image: smithay::input::pointer::CursorImageStatus,
    ) {
    }
}

impl XdgShellHandler for State {
    fn xdg_shell_state(&mut self) -> &mut XdgShellState {
        &mut self.xdg_shell
    }

    fn new_toplevel(&mut self, surface: ToplevelSurface) {
        self.toplevels += 1;
        println!("wc-compositor: xdg_toplevel created (#{})", self.toplevels);

        // A client is not allowed to attach a buffer until it has been
        // configured, so a compositor that never configures produces a client
        // that hangs. Sending it here is the minimum that makes the protocol
        // complete: the client gets `xdg_toplevel.configure` followed by
        // `xdg_surface.configure`, and replies with `ack_configure`.
        //
        // No size is set, which tells the client to pick its own — correct for
        // a compositor with no output and no window management.
        surface.send_configure();
        println!("wc-compositor: sent xdg_toplevel configure");
    }

    fn new_popup(&mut self, surface: PopupSurface, _positioner: PositionerState) {
        println!("wc-compositor: xdg_popup created");
        let _ = surface.send_configure();
    }

    fn grab(&mut self, _surface: PopupSurface, _seat: wl_seat::WlSeat, _serial: smithay::utils::Serial) {}

    fn reposition_request(
        &mut self,
        _surface: PopupSurface,
        _positioner: PositionerState,
        _token: u32,
    ) {
    }
}

delegate_compositor!(State);
delegate_shm!(State);
delegate_xdg_shell!(State);
delegate_seat!(State);

fn main() -> std::process::ExitCode {
    let mut once = false;
    let mut socket_name: Option<String> = None;
    let mut idle_timeout = Duration::from_secs(30);

    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--once" => once = true,
            "--socket" => socket_name = args.next(),
            "--timeout" => {
                idle_timeout = args
                    .next()
                    .and_then(|s| s.parse().ok())
                    .map(Duration::from_secs)
                    .unwrap_or(idle_timeout)
            }
            other => {
                eprintln!("wc-compositor: unknown argument {other}");
                return std::process::ExitCode::from(2);
            }
        }
    }

    let display: Display<State> = match Display::new() {
        Ok(d) => d,
        Err(e) => {
            eprintln!("wc-compositor: could not create display: {e}");
            return std::process::ExitCode::from(2);
        }
    };
    let dh = display.handle();

    // Every one of these creates its globals through smithay, not through us.
    // `ShmState::new` also advertises the mandatory `Argb8888`/`Xrgb8888`
    // formats; the extra list is empty because there is no renderer to claim
    // support for anything else.
    let mut state = State {
        compositor: CompositorState::new::<State>(&dh),
        shm: ShmState::new::<State>(&dh, Vec::new()),
        xdg_shell: XdgShellState::new::<State>(&dh),
        seat_state: SeatState::new(),
        clients_seen: 0,
        toplevels: 0,
    };
    // A seat is not needed to reach `xdg_toplevel`, but a client that finds no
    // `wl_seat` may decline to map a window at all, and it is one line.
    let _seat: Seat<State> = state.seat_state.new_wl_seat(&dh, "seat0");

    let listener = match socket_name {
        Some(name) => ListeningSocket::bind(&name),
        None => ListeningSocket::bind_auto("wayland", 1..32),
    };
    let listener = match listener {
        Ok(l) => l,
        Err(e) => {
            eprintln!("wc-compositor: could not bind socket: {e}");
            return std::process::ExitCode::from(2);
        }
    };

    // Fixed, greppable, and explicitly flushed. A Windows binary under Wine with
    // a redirected stdout is fully buffered, so a harness waiting for this line
    // would otherwise wait forever while the compositor worked perfectly.
    println!(
        "wc-compositor: listening on {}",
        listener
            .socket_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default()
    );
    use std::io::Write as _;
    let _ = std::io::stdout().flush();

    let mut display = display;
    let deadline = Instant::now() + idle_timeout;

    loop {
        match listener.accept() {
            Ok(Some(stream)) => {
                match display
                    .handle()
                    .insert_client(stream, Arc::new(ClientState::default()))
                {
                    Ok(_) => state.clients_seen += 1,
                    Err(e) => eprintln!("wc-compositor: could not insert client: {e}"),
                }
            }
            Ok(None) => {}
            Err(e) => {
                eprintln!("wc-compositor: accept failed: {e}");
                break;
            }
        }

        if let Err(e) = display.dispatch_clients(&mut state) {
            eprintln!("wc-compositor: dispatch failed: {e}");
            break;
        }
        if let Err(e) = display.flush_clients() {
            eprintln!("wc-compositor: flush failed: {e}");
            break;
        }

        let mut live = 0;
        display.handle().backend_handle().with_all_clients(|_| live += 1);

        if once && state.clients_seen > 0 && live == 0 {
            println!(
                "wc-compositor: --once and the last client has gone; {} toplevel(s) seen",
                state.toplevels
            );
            let _ = std::io::stdout().flush();
            return if state.toplevels > 0 {
                std::process::ExitCode::SUCCESS
            } else {
                eprintln!("wc-compositor: no xdg_toplevel was ever created");
                std::process::ExitCode::from(4)
            };
        }

        if Instant::now() > deadline {
            eprintln!("wc-compositor: idle timeout reached; exiting");
            return std::process::ExitCode::from(3);
        }

        std::thread::sleep(Duration::from_millis(5));
    }

    std::process::ExitCode::from(1)
}
