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
         \n\
         built with: {}\n\
         \n\
         `gpu` takes sv-gpu-server's arguments; `wayland` takes\n\
         wc-compositor's. Both halves share one arena — see docs/one-binary.md.",
        halves.join("|"),
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
        #[cfg(feature = "gpu")]
        "gpu" => sv_gpu_server::entry::run_main(half_argv("sv-guest gpu", args)),
        #[cfg(feature = "wayland")]
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
