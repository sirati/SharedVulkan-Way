//! waycompose, stage 1: a Wayland server that accepts a client and completes a
//! `wl_display` handshake.
//!
//! It composites nothing, draws nothing, and has no windows. That is the point.
//! The question this stage answers is the one the research could not: **does the
//! `wayland-server` stack run as a Windows binary at all?** Everything after it
//! — surfaces, `xdg-shell`, HWNDs, decorations — is work rather than risk, and
//! none of it is worth starting before this is known.
//!
//! What it does:
//!
//! * binds a listening socket (AF_UNIX on both platforms — see
//!   `wc-transport`'s docs for why not a named pipe);
//! * accepts clients and inserts them into the backend;
//! * advertises two globals, `wl_compositor` and `wl_shm`, so that the registry
//!   burst a real client expects is non-empty and has something with a version
//!   and an interface name in it;
//! * answers `wl_display.sync` — which the backend does for us, and which is
//!   what makes the handshake verifiable;
//! * for `wl_shm`, sends the `format` events, because that is the smallest real
//!   piece of protocol that is not just registry bookkeeping.
//!
//! Run it with `--once` to exit after the first client disconnects, which is
//! what the test scripts use.

use std::sync::Arc;
use std::time::{Duration, Instant};

use wayland_server::backend::{ClientData, ClientId, DisconnectReason};
use wayland_server::protocol::{wl_compositor::WlCompositor, wl_shm};
use wayland_server::{Display, ListeningSocket};

/// State passed to every dispatch callback.
///
/// Stage 1 needs almost nothing in it. `clients_seen` exists so that `--once`
/// can tell "no client has connected yet" from "the client has gone".
struct State {
    clients_seen: usize,
    clients_live: usize,
}

/// Per-client data. The backend requires *something* implementing [`ClientData`]
/// for every client; this one exists to print the two lifecycle events, which
/// during bring-up is the difference between "the client connected and did
/// nothing" and "the client never connected".
#[derive(Debug)]
struct ClientState;

impl ClientData for ClientState {
    fn initialized(&self, client_id: ClientId) {
        println!("wc-server: client {client_id:?} initialized");
    }

    fn disconnected(&self, client_id: ClientId, reason: DisconnectReason) {
        println!("wc-server: client {client_id:?} disconnected: {reason:?}");
    }
}

// The two globals. `wl_compositor` is the one every client binds; `wl_shm` is
// the one that will eventually carry an `h` argument through the shim, which is
// why it is here rather than something simpler.
//
// The `Dispatch` impls are empty because nothing may be *done* with these
// objects yet. A client that tries will get no response rather than a wrong one,
// which is the honest behaviour for a stage that implements no surfaces.
mod globals {
    use super::{State, WlCompositor};
    use wayland_server::protocol::{wl_shm, wl_shm_pool, wl_surface};
    use wayland_server::{Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource};

    impl GlobalDispatch<WlCompositor, ()> for State {
        fn bind(
            _state: &mut State,
            _handle: &DisplayHandle,
            _client: &Client,
            resource: New<WlCompositor>,
            _global_data: &(),
            data_init: &mut DataInit<'_, State>,
        ) {
            println!("wc-server:   client bound wl_compositor");
            data_init.init(resource, ());
        }
    }

    impl Dispatch<WlCompositor, ()> for State {
        fn request(
            _state: &mut State,
            _client: &Client,
            _resource: &WlCompositor,
            request: <WlCompositor as Resource>::Request,
            _data: &(),
            _dhandle: &DisplayHandle,
            _data_init: &mut DataInit<'_, State>,
        ) {
            println!("wc-server:   wl_compositor request (unimplemented in stage 1): {request:?}");
        }
    }

    impl Dispatch<wl_surface::WlSurface, ()> for State {
        fn request(
            _state: &mut State,
            _client: &Client,
            _resource: &wl_surface::WlSurface,
            _request: <wl_surface::WlSurface as Resource>::Request,
            _data: &(),
            _dhandle: &DisplayHandle,
            _data_init: &mut DataInit<'_, State>,
        ) {
        }
    }

    impl GlobalDispatch<wl_shm::WlShm, ()> for State {
        fn bind(
            _state: &mut State,
            _handle: &DisplayHandle,
            _client: &Client,
            resource: New<wl_shm::WlShm>,
            _global_data: &(),
            data_init: &mut DataInit<'_, State>,
        ) {
            println!("wc-server:   client bound wl_shm");
            let shm = data_init.init(resource, ());
            // The two formats every compositor must advertise. Worth noting for
            // later: `Argb8888`/`Xrgb8888` are BGRA in memory, which is exactly
            // Win32 DIB order — so the eventual shm blit needs no conversion at
            // all. One of the few places the two worlds already agree.
            shm.format(wl_shm::Format::Argb8888);
            shm.format(wl_shm::Format::Xrgb8888);
        }
    }

    impl Dispatch<wl_shm::WlShm, ()> for State {
        fn request(
            _state: &mut State,
            _client: &Client,
            _resource: &wl_shm::WlShm,
            request: <wl_shm::WlShm as Resource>::Request,
            _data: &(),
            _dhandle: &DisplayHandle,
            data_init: &mut DataInit<'_, State>,
        ) {
            match request {
                // `create_pool` is the one core request that carries an `h`, and
                // therefore the smallest end-to-end test of the handle shim.
                //
                // Stage 1 does not implement `wl_shm` properly — no buffers, no
                // formats beyond the two advertised — but it does the one thing
                // that proves the transport: open what the client named and read
                // it. If the bytes are there, a `h` argument crossed a socket
                // that cannot carry file descriptors.
                wl_shm::Request::CreatePool { id, fd, size } => {
                    println!("wc-server:   wl_shm.create_pool(size={size})");
                    super::report_pool_contents(&fd);
                    // The pool object still has to be created or the client's id
                    // is left dangling and the next request on it is a protocol
                    // error.
                    data_init.init(id, ());
                }
                other => {
                    println!("wc-server:   wl_shm request (unimplemented in stage 1): {other:?}");
                }
            }
        }
    }

    impl Dispatch<wl_shm_pool::WlShmPool, ()> for State {
        fn request(
            _state: &mut State,
            _client: &Client,
            _resource: &wl_shm_pool::WlShmPool,
            _request: <wl_shm_pool::WlShmPool as Resource>::Request,
            _data: &(),
            _dhandle: &DisplayHandle,
            _data_init: &mut DataInit<'_, State>,
        ) {
        }
    }
}

/// Open whatever the client's `h` argument named and print what is in it.
///
/// The printed line is what `scripts/wc-handshake.sh` greps for, and it is the
/// only evidence that matters for the shim: the server was told a *name* and a
/// *size* and nothing else, so bytes it can print are bytes it fetched from the
/// object the client created.
#[cfg(windows)]
fn report_pool_contents(handle: &wc_handle::Handle) {
    println!("wc-server:     descriptor: {:?}", handle.descriptor());
    match handle.map() {
        Ok(map) => {
            let bytes = map.as_slice();
            let head: String = String::from_utf8_lossy(&bytes[..17.min(bytes.len())]).into_owned();
            let tail = if bytes.len() >= 4004 {
                String::from_utf8_lossy(&bytes[4000..4004]).into_owned()
            } else {
                String::new()
            };
            println!("wc-server:     SHM MAPPED len={} head={head:?} tail={tail:?}", map.len());
        }
        Err(e) => println!("wc-server:     SHM MAP FAILED: {e}"),
    }
}

/// See the Windows version. On Unix the `h` is a real file descriptor delivered
/// by `SCM_RIGHTS`, so this reads it the ordinary way — which also means the
/// same test exercises both mechanisms through the same protocol path.
#[cfg(unix)]
fn report_pool_contents(fd: &std::os::fd::OwnedFd) {
    use std::io::{Read, Seek};
    use std::os::fd::AsFd;
    let mut file = std::fs::File::from(fd.as_fd().try_clone_to_owned().unwrap());
    let mut bytes = Vec::new();
    let _ = file.seek(std::io::SeekFrom::Start(0));
    match file.read_to_end(&mut bytes) {
        Ok(_) => {
            let head: String = String::from_utf8_lossy(&bytes[..17.min(bytes.len())]).into_owned();
            let tail = if bytes.len() >= 4004 {
                String::from_utf8_lossy(&bytes[4000..4004]).into_owned()
            } else {
                String::new()
            };
            println!("wc-server:     SHM MAPPED len={} head={head:?} tail={tail:?}", bytes.len());
        }
        Err(e) => println!("wc-server:     SHM MAP FAILED: {e}"),
    }
    // `file` owns a *duplicate* of the client's descriptor, not the client's
    // own, so letting it drop here closes only our copy.
}

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
                eprintln!("wc-server: unknown argument {other}");
                return std::process::ExitCode::from(2);
            }
        }
    }

    let mut display: Display<State> = match Display::new() {
        Ok(d) => d,
        Err(e) => {
            eprintln!("wc-server: could not create display: {e}");
            return std::process::ExitCode::from(2);
        }
    };
    let dh = display.handle();

    dh.create_global::<State, WlCompositor, _>(6, ());
    dh.create_global::<State, wl_shm::WlShm, _>(1, ());

    let listener = match socket_name {
        Some(name) => ListeningSocket::bind(&name),
        None => ListeningSocket::bind_auto("wayland", 1..32),
    };
    let listener = match listener {
        Ok(l) => l,
        Err(e) => {
            eprintln!("wc-server: could not bind socket: {e}");
            return std::process::ExitCode::from(2);
        }
    };

    // Printed in a fixed, greppable format because the test scripts read it to
    // learn where to connect. Flushed explicitly: a Windows binary under Wine
    // with a redirected stdout is fully buffered, and a harness that waits for
    // this line would otherwise wait forever while the server sat there working
    // perfectly.
    println!(
        "wc-server: listening on {}",
        listener.socket_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default()
    );
    use std::io::Write as _;
    let _ = std::io::stdout().flush();

    let mut state = State { clients_seen: 0, clients_live: 0 };
    let deadline = Instant::now() + idle_timeout;

    // The event loop is a poll loop rather than `calloop`, and that is a stage-1
    // decision rather than the final architecture.
    //
    // On Unix the natural shape is `calloop` with the listening socket and
    // `Display::as_fd()` as two sources. On Windows `Display`'s poll handle is a
    // wepoll handle, which cannot itself be polled by wepoll (see
    // `Backend::poll_handle`), so that shape does not transfer. The real answer
    // is the two-thread design the research argues for on independent grounds
    // (§4.6, §9.2) — a Win32 message pump on one thread, Wayland dispatch on
    // another — and it is not needed until there are windows to pump messages
    // for. Until then a sleep-poll loop is honest about doing nothing clever.
    loop {
        match listener.accept() {
            Ok(Some(stream)) => {
                match display.handle().insert_client(stream, Arc::new(ClientState)) {
                    Ok(_) => {
                        state.clients_seen += 1;
                        state.clients_live += 1;
                    }
                    Err(e) => eprintln!("wc-server: could not insert client: {e}"),
                }
            }
            Ok(None) => {}
            Err(e) => {
                eprintln!("wc-server: accept failed: {e}");
                break;
            }
        }

        if let Err(e) = display.dispatch_clients(&mut state) {
            eprintln!("wc-server: dispatch failed: {e}");
            break;
        }
        if let Err(e) = display.flush_clients() {
            eprintln!("wc-server: flush failed: {e}");
            break;
        }

        // `dispatch_clients` does not tell us when a client goes away, so ask
        // the backend how many are left. Cheap at stage-1 scale, and it is only
        // here to make `--once` terminate.
        let mut live = 0;
        display.handle().backend_handle().with_all_clients(|_| live += 1);
        state.clients_live = live;

        if once && state.clients_seen > 0 && state.clients_live == 0 {
            println!("wc-server: --once and the last client has gone; exiting");
            let _ = std::io::stdout().flush();
            return std::process::ExitCode::SUCCESS;
        }

        if Instant::now() > deadline {
            eprintln!("wc-server: idle timeout reached; exiting");
            return std::process::ExitCode::from(3);
        }

        std::thread::sleep(Duration::from_millis(5));
    }

    std::process::ExitCode::from(1)
}
