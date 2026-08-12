//! `sv-hostd` — create the arena's backing and hand it out.
//!
//! ```text
//! sv-hostd serve --socket /run/sv/hostd.sock [--generation N] [--arena BYTES]
//! ```
//!
//! One extent, any number of clients. Each client that connects gets an
//! [`Attachment`] describing the two views and the descriptor to map them from,
//! passed by `SCM_RIGHTS` — the same handoff the real deployment uses, which is
//! why the container harness exercises it rather than sharing a path.

#[cfg(target_os = "linux")]
fn main() -> std::process::ExitCode {
    match run() {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("sv-hostd: {e}");
            std::process::ExitCode::FAILURE
        }
    }
}

/// The daemon opens a memfd and passes descriptors over a Unix socket, and
/// neither exists on Windows. design.md requires either side to be either OS,
/// and the *app* side is the one that runs `sv-hostd`; a Windows app side would
/// need a named-pipe and `DuplicateHandle` implementation of exactly this file.
#[cfg(not(target_os = "linux"))]
fn main() -> std::process::ExitCode {
    eprintln!("sv-hostd: the daemon is Linux-only; a Windows app side needs a named-pipe handoff");
    std::process::ExitCode::FAILURE
}

#[cfg(target_os = "linux")]
fn run() -> Result<(), Box<dyn std::error::Error>> {
    use std::os::fd::AsFd;
    use std::os::unix::net::UnixListener;
    use sv_hostd::{Hostd, demo_geometry};
    use sv_transport::cold::{ColdMessage, recv, send_with_fd};

    let args: Vec<String> = std::env::args().collect();
    let arg = |k: &str| -> Option<String> {
        args.iter().position(|a| a == k).and_then(|i| args.get(i + 1)).cloned()
    };
    if args.len() < 2 || args[1] != "serve" {
        eprintln!(
            "usage: sv-hostd serve --socket <path> [--generation <n>] [--arena <bytes>] [--clients <n>] [--live]"
        );
        return Err("no subcommand".into());
    }
    let path = arg("--socket").ok_or("--socket is required")?;
    let generation: u32 = arg("--generation").map_or(Ok(1), |s| s.parse())?;
    // `--live` serves the geometry a real session uses; the default stays the
    // demo one, because `sv-loopback` and the container harness are sized for
    // it and a 16 MiB control region would make them slower for no gain.
    //
    // The two must not be chosen independently. On the memfd path `sv-hostd`
    // sizes the file and `sv-gpu-server` publishes the header, so a daemon
    // serving 1 MiB to an owner that wants 16 MiB produces "the layout does not
    // fit in control_size" from the *other* process — which is how this flag
    // came to exist.
    let mut geometry = if args.iter().any(|a| a == "--live") {
        sv_transport::control::Geometry::for_arena(64 << 20)
    } else {
        demo_geometry()
    };
    if let Some(n) = arg("--arena") {
        geometry.arena_size = n.parse()?;
    }
    // Serve a fixed number of clients and exit, so the harness is a command that
    // finishes rather than a daemon a script has to remember to kill.
    let clients: usize = arg("--clients").map_or(Ok(2), |s| s.parse())?;

    let run_id = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .subsec_nanos() as u64
        | 1;
    let hostd = Hostd::local(geometry, run_id, generation)?;
    eprintln!(
        "sv-hostd: {} MiB control + {} MiB arena, {} run(s), generation {}",
        geometry.control_size >> 20,
        geometry.arena_size >> 20,
        hostd.runs.len(),
        generation
    );

    let _ = std::fs::remove_file(&path);
    let listener = UnixListener::bind(&path)?;
    // The socket is the rendezvous, so it has to be reachable by the other
    // container. A bind mount of its directory is enough and needs no shared
    // network namespace.
    eprintln!("sv-hostd: listening on {path}");

    for i in 0..clients {
        let (sock, _) = listener.accept()?;
        let (msg, _) = recv(&sock)?;
        match msg {
            ColdMessage::AttachRequest => {
                let mut buf = Vec::new();
                ColdMessage::Attach(hostd.attachment()).encode(&mut buf)?;
                send_with_fd(&sock, &buf, Some(hostd.fd.as_fd()))?;
                eprintln!("sv-hostd: client {i} attached");
            }
            other => {
                let mut buf = Vec::new();
                ColdMessage::Refuse { code: 1, reason: format!("expected AttachRequest, got {other:?}") }
                    .encode(&mut buf)?;
                send_with_fd(&sock, &buf, None)?;
            }
        }
        // The client keeps the descriptor; dropping our end of the socket is
        // fine, because the memfd is refcounted by the kernel and the client's
        // copy holds it alive independently.
    }
    eprintln!("sv-hostd: served {clients} client(s), exiting");
    let _ = std::fs::remove_file(&path);
    Ok(())
}
