//! The client's connection and its loop. Windows only — see the crate docs.
//!
//! Everything about *what bytes* go on the wire lives in `proto`, where the
//! tests can reach it. This file is only about when to send them and what to do
//! when something comes back, which is the part that needs a socket and a
//! compositor and therefore cannot be a unit test.

use std::io::{Read, Write};
use std::process::ExitCode;
use std::time::{Duration, Instant};

use crate::proto::{self, Event, Rect};
use crate::Config;

pub fn run(config: Config) -> ExitCode {
    let Config {
        width,
        height,
        frames,
        partial,
        title,
        app_id,
        path_arg,
    } = config;

    let path = match path_arg {
        Some(p) => std::path::PathBuf::from(p),
        None => {
            let name = std::env::var("WAYLAND_DISPLAY").unwrap_or_else(|_| "wayland-0".into());
            match wc_transport::socket_dir() {
                Ok(dir) => dir.join(name),
                Err(e) => {
                    eprintln!("wc-shmclient: no socket path given and {e}");
                    return ExitCode::from(2);
                }
            }
        }
    };

    println!("wc-shmclient: connecting to {}", path.display());
    let stream = match wc_transport::connect(&path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("wc-shmclient: connect failed: {e}");
            return ExitCode::from(2);
        }
    };
    let _ = stream.set_read_timeout(Some(Duration::from_millis(20)));

    let mut c = Client {
        stream,
        buf: Vec::new(),
        globals: Vec::new(),
        sync_done: false,
        configured: false,
        toplevel_size: None,
        closed: false,
        released: [true, true],
        frame_done: true,
        failure: None,
    };

    if c.send(proto::hello()).is_err() {
        return ExitCode::from(2);
    }
    if !c.pump_until(Duration::from_secs(5), |c| c.sync_done) {
        return c.fail("no wl_callback.done for the initial sync");
    }
    println!("wc-shmclient: {} globals advertised", c.globals.len());

    let (Some(compositor), Some(shm), Some(wm_base)) = (
        c.global("wl_compositor"),
        c.global("wl_shm"),
        c.global("xdg_wm_base"),
    ) else {
        return c.fail("the server did not advertise all of wl_compositor, wl_shm and xdg_wm_base");
    };

    if c.send(proto::create_window(
        compositor, shm, wm_base, &title, &app_id,
    ))
    .is_err()
    {
        return ExitCode::from(2);
    }
    println!("wc-shmclient: created wl_surface, xdg_surface and xdg_toplevel; title {title:?}");

    if !c.pump_until(Duration::from_secs(5), |c| c.configured) {
        return c.fail("no xdg_surface.configure");
    }
    if let Some((w, h)) = c.toplevel_size {
        println!(
            "wc-shmclient: configured {w}x{h}{}",
            if w == 0 || h == 0 {
                " (the compositor left the size to us)"
            } else {
                ""
            }
        );
    }

    // --- the pool ---------------------------------------------------------
    //
    // One section holding two buffers back to back. On Windows this is a named
    // kernel object, and what crosses the socket is its *name*, in band
    // (docs/design.md §2.3).
    //
    // The handle is held for the whole run, and that is not tidiness. A Windows
    // section with no open handles is destroyed and its name freed, so dropping
    // it after `create_pool` would leave the compositor holding a name for an
    // object that no longer exists. That is §2.5, and this is the first program
    // that has to obey it for longer than a round trip.
    let buffer_len = (width * 4) as u64 * height as u64;
    let pool_len = buffer_len * 2;

    let handle = match wc_handle::Handle::create_section(pool_len) {
        Ok(h) => h,
        Err(e) => {
            eprintln!("wc-shmclient: FAIL — could not create a {pool_len}-byte section: {e}");
            return ExitCode::from(1);
        }
    };
    let mut view = match handle.map_mut() {
        Ok(v) => v,
        Err(e) => {
            eprintln!("wc-shmclient: FAIL — could not map the section writable: {e}");
            return ExitCode::from(1);
        }
    };
    let descriptor = handle.descriptor().encode();
    println!(
        "wc-shmclient: created a {pool_len}-byte section for 2 x {width}x{height}; \
         descriptor is {} bytes on the wire",
        descriptor.len()
    );

    if c.send(proto::create_pool(&descriptor, pool_len, width, height))
        .is_err()
    {
        return ExitCode::from(2);
    }
    println!("wc-shmclient: sent wl_shm.create_pool and two wl_shm_pool.create_buffer");

    // --- the loop ---------------------------------------------------------
    let mut committed = 0u32;
    let mut slot = 0usize;
    let deadline = Instant::now() + Duration::from_secs(60);

    while committed < frames && !c.closed && Instant::now() < deadline {
        // Both conditions are the compositor's to satisfy: a buffer may not be
        // drawn into until it is released, and the next frame is not due until
        // the callback fires. A client that skipped either would be writing
        // over pixels the compositor had not finished with — and would make a
        // compositor with a use-after-release bug look perfectly fine.
        let s = slot;
        if !c.pump_until(Duration::from_secs(5), |c| c.released[s] && c.frame_done) {
            return c.fail(&format!(
                "after {committed} frame(s): buffer {s} not released or no frame callback \
                 (released={:?} frame_done={})",
                c.released, c.frame_done
            ));
        }
        if c.closed {
            break;
        }

        let base = slot * buffer_len as usize;
        let damage = proto::draw(
            &mut view.as_mut_slice()[base..base + buffer_len as usize],
            width,
            height,
            width * 4,
            committed,
        );
        let damage = if partial {
            damage
        } else {
            Rect {
                x: 0,
                y: 0,
                w: width,
                h: height,
            }
        };

        c.released[slot] = false;
        c.frame_done = false;
        if c.send(proto::present(slot, damage)).is_err() {
            return ExitCode::from(2);
        }

        if committed < 3 || committed.is_multiple_of(20) {
            println!(
                "wc-shmclient: frame {committed} slot {slot} damage {},{} {}x{}",
                damage.x, damage.y, damage.w, damage.h
            );
        }
        committed += 1;
        slot ^= 1;
    }

    if let Some(e) = c.failure.take() {
        eprintln!("wc-shmclient: FAIL — {e}");
        return ExitCode::from(1);
    }
    println!(
        "wc-shmclient: PASS — {committed} frame(s) committed{}",
        if c.closed {
            ", stopped by xdg_toplevel.close"
        } else {
            ""
        }
    );
    // Only now may the section go. See §2.5.
    drop(view);
    drop(handle);
    ExitCode::SUCCESS
}

struct Client {
    stream: wc_transport::Stream,
    buf: Vec<u8>,
    globals: Vec<(u32, String, u32)>,
    sync_done: bool,
    configured: bool,
    toplevel_size: Option<(i32, i32)>,
    closed: bool,
    released: [bool; 2],
    frame_done: bool,
    failure: Option<String>,
}

impl Client {
    fn send(&mut self, bytes: Vec<u8>) -> std::io::Result<()> {
        self.stream.write_all(&bytes).inspect_err(|e| {
            eprintln!("wc-shmclient: write failed: {e}");
        })
    }

    fn global(&self, interface: &str) -> Option<(u32, u32)> {
        self.globals
            .iter()
            .find(|(_, i, _)| i == interface)
            .map(|(n, _, v)| (*n, *v))
    }

    fn fail(&mut self, why: &str) -> ExitCode {
        eprintln!("wc-shmclient: FAIL — {why}");
        if let Some(e) = self.failure.take() {
            eprintln!("wc-shmclient: the server also raised: {e}");
        }
        ExitCode::from(1)
    }

    /// Read and dispatch until `done` is satisfied or the deadline passes.
    fn pump_until<F: Fn(&Self) -> bool>(&mut self, timeout: Duration, done: F) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            if done(self) {
                return true;
            }
            // Not a failure: the compositor asked us to go away, and every
            // caller checks `closed` immediately after.
            if self.closed {
                return true;
            }
            if self.failure.is_some() || Instant::now() >= deadline {
                return false;
            }
            let mut chunk = [0u8; 8192];
            match self.stream.read(&mut chunk) {
                Ok(0) => {
                    self.failure = Some("the server closed the connection".into());
                    return false;
                }
                Ok(n) => self.buf.extend_from_slice(&chunk[..n]),
                Err(e)
                    if e.kind() == std::io::ErrorKind::WouldBlock
                        || e.kind() == std::io::ErrorKind::TimedOut => {}
                Err(e) => {
                    self.failure = Some(format!("read failed: {e}"));
                    return false;
                }
            }
            self.dispatch();
        }
    }

    fn dispatch(&mut self) {
        let (events, consumed, desync) = proto::parse(&self.buf);
        self.buf.drain(..consumed);
        for event in events {
            self.handle(event);
        }
        if let Some(proto::Desync(why)) = desync {
            self.failure = Some(format!("the event stream desynchronised: {why}"));
        }
    }

    fn handle(&mut self, event: Event) {
        match event {
            Event::Error {
                object,
                code,
                message,
            } => {
                self.failure = Some(format!("protocol error on object {object}, code {code}: {message}"));
            }
            Event::Global {
                name,
                interface,
                version,
            } => {
                println!("wc-shmclient:   global {name}: {interface} v{version}");
                self.globals.push((name, interface, version));
            }
            Event::SyncDone => self.sync_done = true,
            Event::FrameDone => self.frame_done = true,
            Event::BufferReleased(i) => self.released[i] = true,
            Event::Ping(serial) => {
                // A client that does not pong is killed as unresponsive — and
                // it would be killed *after* the screenshot, which is the worst
                // possible way to discover it.
                let _ = self.send(proto::pong(serial));
            }
            Event::XdgConfigure(serial) => {
                // Acked here rather than by the caller, so that **every**
                // configure is acked and not only the first. A compositor whose
                // resize configures were never acked would keep them pending
                // forever, and a client that acked only the initial one would
                // hide that.
                let _ = self.send(proto::ack_configure(serial));
                self.configured = true;
            }
            Event::ToplevelConfigure(w, h) => self.toplevel_size = Some((w, h)),
            Event::ToplevelClose => {
                println!("wc-shmclient: received xdg_toplevel.close");
                self.closed = true;
            }
            Event::Ignored => {}
        }
    }
}
