//! `sv-guest` — the one binary that runs **inside** the VM.
//!
//! It is the Vulkan GPU server and the Wayland compositor, in one process, over
//! one arena. `docs/one-binary.md` is the design; this file is only the
//! dispatch, and it is deliberately tiny.
//!
//! # Why one process, in one number
//!
//! A Firefox frame, before this existed, was rendered by the guest's GPU into
//! the guest's own RAM, copied out to a host with no GPU, and copied back into
//! the guest to be displayed: **four copies and two full-frame crossings of the
//! hypervisor, in opposite directions.** Not because anything needed it, but
//! because shared_vulkan's arena and waycompose's arena were two regions and
//! neither program could name a range of the other's. In one process it is two
//! copies and no crossings. one-binary.md opens with the table.
//!
//! # What this file may never become
//!
//! A place where the two halves hand each other pointers. `design.md` §1a — no
//! descriptor and no handle crosses the boundary — gets *harder* here, not
//! easier: with the renderer and the presenter in one address space, passing a
//! raw pointer between them would simply work, on the development machine, and
//! be unrepresentable on the deployment. `sv_arena::handle::FrameHandle` is the
//! only way memory is named between the halves, and it is a struct of integers
//! whose wire form is `[u32; 12]` precisely so that it *cannot* carry one.

use std::process::ExitCode;

/// Which half to run.
///
/// The two halves are separate processes' worth of work and they take different
/// arguments, so the first argument selects one rather than the binary trying to
/// guess. `sv-guest gpu …` takes exactly `sv-gpu-server`'s arguments and
/// `sv-guest wayland …` takes exactly `wc-compositor`'s — which is what makes
/// every harness script that predates the merge a one-word edit rather than a
/// rewrite.
fn usage() -> String {
    let mut halves: Vec<&str> = Vec::new();
    if cfg!(feature = "gpu") {
        halves.push("gpu");
    }
    if cfg!(feature = "wayland") {
        halves.push("wayland");
    }
    format!(
        "sv-guest — the in-VM half of shared_vulkan + waycompose\n\
         \n\
         usage: sv-guest <{}> [args…]\n\
         {}\
         \n\
         built with: {}\n\
         \n\
         `gpu` takes sv-gpu-server's arguments; `wayland` takes\n\
         wc-compositor's. Both halves share one arena — see docs/one-binary.md.",
        halves.join("|"),
        if cfg!(windows) { "       sv-guest broker [--console] …  the SYSTEM service (§111.8)\n" } else { "" },
        if halves.is_empty() { "no halves — this binary does nothing".to_string() } else { halves.join(", ") },
    )
}

fn main() -> ExitCode {
    let mut args = std::env::args();
    let _argv0 = args.next();
    let Some(half) = args.next() else {
        eprintln!("{}", usage());
        return ExitCode::from(2);
    };

    match half.as_str() {
        // §111.8: the SYSTEM service that lends the devices, on every build:
        // it is neither half, and the guest has one binary.
        #[cfg(windows)]
        "broker" => wc_broker::sys::windows::service::main(&args.collect::<Vec<_>>()),
        #[cfg(feature = "gpu")]
        "gpu" => sv_gpu_server::entry::run_main(half_argv("sv-guest gpu", args)),
        #[cfg(all(feature = "wayland", feature = "gpu"))]
        "wayland" => one_pool::run(args.collect()),
        #[cfg(all(feature = "wayland", not(feature = "gpu")))]
        "wayland" => wc_compositor::run_from(half_argv("sv-guest wayland", args)),

        // Naming the half that was compiled out, rather than saying "unknown",
        // because the failure a person actually hits is a binary built with
        // `--no-default-features --features gpu` being asked for the compositor.
        // "unknown argument" would send them to look for a typo.
        #[cfg(not(feature = "gpu"))]
        "gpu" => {
            eprintln!("sv-guest: this binary was built without the `gpu` feature");
            ExitCode::from(2)
        }
        #[cfg(not(feature = "wayland"))]
        "wayland" => {
            eprintln!("sv-guest: this binary was built without the `wayland` feature");
            ExitCode::from(2)
        }
        "-h" | "--help" => {
            println!("{}", usage());
            ExitCode::SUCCESS
        }
        other => {
            eprintln!("sv-guest: no half called {other:?}\n\n{}", usage());
            ExitCode::from(2)
        }
    }
}

/// Build the argument vector a half sees.
///
/// `argv[0]` is a program name and `argv[1..]` are the arguments, which is what
/// `std::env::args()` yields and what both halves parse. The half-selecting
/// token is consumed *here* rather than skipped by each half, so that
/// `sv-gpu-server --arena 256M` and `sv-guest gpu --arena 256M` reach the same
/// parser with the same input — which is what makes every harness script that
/// predates the merge a one-word edit rather than a rewrite.
fn half_argv(name: &str, rest: impl Iterator<Item = String>) -> Vec<String> {
    std::iter::once(name.to_string()).chain(rest).collect()
}

/// §110: one pool for the Wayland arena and the GPU device.
///
/// `sv-guest wayland <compositor args> --with-gpu <gpu args>` runs both halves
/// in **this** process: the GPU pool (`sv-gpu-server guest --pool …`) on a
/// thread, the compositor on the main thread (its window pump needs it). The
/// ivshmem driver gives one mapping per device, so this is the only way the
/// compositor can read a Wayland buffer that lives on the GPU device: the GPU
/// pool becomes region 1 of the compositor's arena, and a session whose BAR
/// windows are full leases its buffers there, from the same allocator the GPU
/// sessions lease from. Without `--with-gpu` this is `wc_compositor::run_from`.
#[cfg(all(feature = "wayland", feature = "gpu"))]
mod one_pool {
    use std::collections::HashMap;
    use std::process::ExitCode;
    use std::sync::{Arc, Mutex};

    use sv_gpu_server::pool::PoolServer;

    /// The GPU device's pool, as a region of the Wayland arena: a pool account
    /// per Wayland session, `Dedicated` leases (never the reserve kept for a
    /// GPU session's start), region 0 of the GPU pool only (the BAR this
    /// process maps).
    pub struct GpuRegion {
        server: Arc<PoolServer>,
        sessions: Mutex<HashMap<u8, u64>>,
    }

    impl wc_compositor::PoolRegion for GpuRegion {
        fn lease(&self, session: u8, install: &str, len: u64) -> Result<(u64, u64), String> {
            let s = *self
                .sessions
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .entry(session)
                .or_insert_with(|| self.server.open_session(&format!("wayland {install} (session {session})")).session);
            let l = self
                .server
                .lease_raw(s, len, sv_arena::pool::Purpose::Dedicated)
                .map_err(|e| e.to_string())?;
            let (region, offset) = sv_arena::pool::split(l.offset);
            if region != 0 {
                let _ = self.server.release(l.offset);
                return Err(format!("the GPU pool leased in its region {region}, which this process does not map"));
            }
            Ok((offset, l.len))
        }

        fn release(&self, offset: u64) {
            let _ = self.server.release(offset);
        }

        fn describe(&self) -> String {
            format!("the GPU device's pool, {} MiB (§97)", self.server.capacity() >> 20)
        }
    }

    /// Split `wayland` args at `--with-gpu`.
    pub fn split(args: Vec<String>) -> (Vec<String>, Option<Vec<String>>) {
        match args.iter().position(|a| a == "--with-gpu") {
            Some(i) => (args[..i].to_vec(), Some(args[i + 1..].to_vec())),
            None => (args, None),
        }
    }

    pub fn run(args: Vec<String>) -> ExitCode {
        let (wayland, gpu) = split(args);
        if let Some(gpu) = gpu {
            sv_gpu_server::pool_guest::set_pool_hook(Box::new(|server, boot| {
                let src = Arc::new(GpuRegion { server: Arc::clone(server), sessions: Mutex::new(HashMap::new()) });
                // SAFETY: `boot` is the GPU device's mapping, which the GPU
                // pool keeps for the life of the process (`serve_pool` does not
                // return while it serves).
                let r = unsafe { wc_compositor::add_pool_region(1, boot.base_ptr(), boot.len(), src) };
                if let Err(e) = r {
                    eprintln!("sv-guest: the GPU pool could not be the Wayland arena's region 1: {e}");
                }
            }));
            let argv: Vec<String> = std::iter::once("sv-guest gpu".to_string()).chain(gpu).collect();
            let _ = std::thread::Builder::new().name("gpu-pool".into()).spawn(move || {
                let rc = sv_gpu_server::entry::run_main(argv);
                eprintln!("sv-guest: the GPU pool ended ({rc:?}); the compositor goes on without it");
            });
        }
        wc_compositor::run_from(super::half_argv("sv-guest wayland", wayland.into_iter()))
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn the_gpu_args_follow_with_gpu() {
            let a = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
            let (w, g) = split(a(&["--socket", "x", "--with-gpu", "guest", "--pool"]));
            assert_eq!(w, a(&["--socket", "x"]));
            assert_eq!(g, Some(a(&["guest", "--pool"])));
            assert_eq!(split(a(&["--socket", "x"])).1, None);
        }

        /// The one allocator: a Wayland session's leases and a GPU session's
        /// come out of the same pool and never overlap.
        #[test]
        fn wayland_and_gpu_leases_share_one_allocator() {
            let boot = Arc::new(sv_transport::region::SharedRegion::heap(64 << 20).unwrap());
            let cfg = sv_arena::pool::PoolConfig::for_capacity(64 << 20);
            let server = PoolServer::new(cfg, boot, sv_gpu_server::pool::Importer::HostPointer).unwrap();
            let r = GpuRegion { server: Arc::clone(&server), sessions: Mutex::new(HashMap::new()) };
            let gpu = server.open_session("gpu app");
            let mut spans = Vec::new();
            for i in 0..6u64 {
                let l = server.lease_raw(gpu.session, (1 + i) << 20, sv_arena::pool::Purpose::Dedicated).unwrap();
                spans.push((l.offset, l.len));
                let (off, len) = wc_compositor::PoolRegion::lease(&r, (i % 2 + 1) as u8, "wcivs", 2 << 20).unwrap();
                spans.push((off, len));
            }
            spans.sort();
            for w in spans.windows(2) {
                assert!(w[0].0 + w[0].1 <= w[1].0, "overlap: {w:?}");
            }
            let (off, _) = wc_compositor::PoolRegion::lease(&r, 3, "wcmc", 1 << 20).unwrap();
            wc_compositor::PoolRegion::release(&r, off);
            assert!(server.report().contains("wayland wcivs"), "{}", server.report());
        }
    }
}
