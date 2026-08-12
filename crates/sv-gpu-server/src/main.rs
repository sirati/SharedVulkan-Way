//! ```text
//! sv-gpu-server serve   --socket <path> [--device N] [--arena BYTES] [--clients N]
//! sv-gpu-server attach  --socket <path> [--device N]
//! ```
//!
//! # Two subcommands, because there are two arena providers
//!
//! `serve` is the GPU-allocated path: this process opens a driver, allocates the
//! arena from it, exports a descriptor, and *is* the cold-path server — app-side
//! processes connect to it and are handed the two descriptors. Measured working
//! on RADV and NVIDIA.
//!
//! `attach` is the app-allocated path: `sv-hostd` owns a memfd, this process
//! connects as a client and imports the mapping it is given with
//! `VK_EXT_external_memory_host`. This is the topology `testhost` needs — the
//! GPU side is inside a VM and the arena is guest RAM the host located — and it
//! is the one lavapipe can serve.
//!
//! In both cases the GPU side is the **owner** of the control region in §4.3's
//! sense: it publishes the header and writes `magic` last. That is unchanged
//! between the two, which is the point — everything above the provider is the
//! same code.

/// The Windows GPU side.
///
/// ```text
/// sv-gpu-server serve --session <name> [--device N] [--arena BYTES]
/// ```
///
/// Everything above the provider is the same code as the Linux path — the same
/// `Control`, the same rings, the same `session::serve`. What differs is only
/// how the two regions come into existence and how the app side is told about
/// them, and both differences are smaller than design.md expected:
///
/// - the arena and the control region are two **named section objects**
///   ([`sv_transport::winshm`]) rather than two memfds;
/// - the cold path is a **named pipe** carrying [`sv_transport::stream`]'s
///   framing, and it passes *no handle at all*, because the peer opens the
///   sections by name.
///
/// §8's "a Windows app side would need a named-pipe and `DuplicateHandle`
/// implementation" is therefore half right: the pipe is needed and
/// `DuplicateHandle` is not. See [`sv_transport::winpipe`] for why that matters
/// rather than being a detail — it removes a privilege relationship between the
/// GPU side and every application that attaches.
#[cfg(windows)]
fn main() -> std::process::ExitCode {
    match win_run() {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("sv-gpu-server: {e}");
            std::process::ExitCode::FAILURE
        }
    }
}

#[cfg(windows)]
fn win_run() -> Result<(), Box<dyn std::error::Error>> {
    use std::sync::Arc;
    use sv_gpu_server::driver::Driver;
    use sv_gpu_server::{arena, server::Shared, session, table::Objects};
    use sv_transport::cold::{Attachment, ColdMessage};
    use sv_transport::control::Control;
    use sv_transport::region::SharedRegion;
    use sv_transport::stream::{read_message, write_message};
    use sv_transport::{winpipe, winshm};

    let args: Vec<String> = std::env::args().collect();
    let arg = |k: &str| -> Option<String> {
        args.iter().position(|a| a == k).and_then(|i| args.get(i + 1)).cloned()
    };
    if args.get(1).map(String::as_str) != Some("serve") {
        eprintln!(
            "usage: sv-gpu-server serve --session <name> [--device N] [--arena BYTES]"
        );
        return Err("no subcommand".into());
    }
    let session_name = arg("--session").ok_or("--session is required")?;
    let device: usize = arg("--device").map_or(Ok(0), |s| s.parse())?;
    let arena_size: u64 = arg("--arena").map_or(Ok(64 << 20), |s| s.parse())?;

    let driver = Driver::open(device)?;
    let key = driver.driver_key();
    eprintln!(
        "[gpu] {} ({key}), queue family {}, provider AppMemfd (named section)",
        driver.name, driver.queue_family
    );

    let geometry = sv_transport::control::Geometry::for_arena(arena_size);

    // Two sections, not one with two views. Windows view offsets must be
    // multiples of the 64 KiB allocation granularity, so "the arena starts after
    // the control region" would drag a number into the geometry that has nothing
    // to do with this design. Two objects is also what §4.1 asks for in spirit —
    // two independent mappings with independent bases — and what the Linux
    // GPU-allocated path already does with two descriptors.
    let control_section =
        winshm::Section::create(&winshm::section_name(&format!("{session_name}.ctl")), geometry.control_size)?;
    let arena_section =
        winshm::Section::create(&winshm::section_name(&format!("{session_name}.arena")), geometry.arena_size)?;
    let control_region = Arc::new(SharedRegion::map_section(control_section)?);
    let arena_region = Arc::new(SharedRegion::map_section(arena_section)?);

    let generation = 1u32;
    let control = Control::create(
        Arc::clone(&control_region),
        geometry,
        sv_gpu_server::protocol_ids(),
        generation,
    )?;
    control.stamp_epoch()?;

    // The import is the same call the Intel driver has already been measured
    // accepting — `memoryTypeBits = 0x6`, `vkMapMemory` returning the same
    // virtual address. What is new here is only where the pointer came from.
    let arena = arena::import_whole(&driver, Arc::clone(&arena_region))?;
    eprintln!(
        "[gpu] arena {} MiB imported as a host pointer, memory type {}",
        arena.len >> 20,
        arena.memory_type
    );

    let pipe_name = winpipe::pipe_name(&session_name);
    eprintln!("[gpu] waiting for the app side on {pipe_name}");
    let mut pipe = winpipe::Pipe::serve(&pipe_name)?;
    let mut scratch = Vec::new();
    match read_message(&mut pipe, &mut scratch)? {
        ColdMessage::AttachRequest => {
            // No descriptor and no handle: the names are derived from the
            // session string on both sides, so what crosses is the geometry.
            write_message(
                &mut pipe,
                &ColdMessage::Attach(Attachment {
                    control_offset: 0,
                    control_len: geometry.control_size,
                    arena_offset: 0,
                    arena_len: geometry.arena_size,
                    generation,
                    run_count: 0,
                }),
            )?;
            eprintln!("[gpu] app side attached (two named sections, no handle passed)");
        }
        other => {
            write_message(
                &mut pipe,
                &ColdMessage::Refuse {
                    code: 1,
                    reason: format!("expected AttachRequest, got {other:?}"),
                },
            )?;
            return Err("the app side did not ask to attach".into());
        }
    }

    let mut objects = Objects::new();
    objects.set_arena(arena.memory, arena.len);
    let shared = Arc::new(Shared {
        driver,
        arena,
        objects: std::sync::Mutex::new(objects),
    });
    let handled = session::serve(
        Arc::new(control),
        shared,
        geometry.ring_slot_count,
        std::time::Duration::from_secs(30),
    )?;
    eprintln!("[gpu] {handled} commands dispatched");
    Ok(())
}

#[cfg(not(any(target_os = "linux", windows)))]
fn main() {
    eprintln!("sv-gpu-server: only Linux and Windows have a cold path in this build");
    std::process::exit(1);
}

#[cfg(target_os = "linux")]
fn main() -> std::process::ExitCode {
    match run() {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("sv-gpu-server: {e}");
            std::process::ExitCode::FAILURE
        }
    }
}

#[cfg(target_os = "linux")]
fn run() -> Result<(), Box<dyn std::error::Error>> {
    use sv_gpu_server::arena;
    use sv_gpu_server::driver::Driver;

    let args: Vec<String> = std::env::args().collect();
    let arg = |k: &str| -> Option<String> {
        args.iter().position(|a| a == k).and_then(|i| args.get(i + 1)).cloned()
    };
    let mode = args.get(1).cloned().unwrap_or_default();
    let socket = arg("--socket").ok_or("--socket is required")?;
    let device: usize = arg("--device").map_or(Ok(0), |s| s.parse())?;

    let driver = Driver::open(device)?;
    let key = driver.driver_key();
    let chosen = arena::Provider::for_driver(key);
    eprintln!(
        "[gpu] {} ({key}), queue family {}, provider {chosen:?}",
        driver.name, driver.queue_family
    );

    match mode.as_str() {
        "serve" => {
            let arena_size: u64 = arg("--arena").map_or(Ok(64 << 20), |s| s.parse())?;
            let clients: usize = arg("--clients").map_or(Ok(1), |s| s.parse())?;
            serve(driver, &socket, arena_size, clients)
        }
        "attach" => attach(driver, &socket),
        "guest" => {
            let arena_size: u64 = arg("--arena").map_or(Ok(64 << 20), |s| s.parse())?;
            let huge = args.iter().any(|a| a == "--huge");
            guest(driver, &socket, arena_size, huge)
        }
        _ => {
            eprintln!(
                "usage: sv-gpu-server <serve|attach|guest> --socket <path> \
                 [--device N] [--arena BYTES] [--clients N] [--huge]"
            );
            Err("no subcommand".into())
        }
    }
}

/// The GPU-allocated path: allocate, export, hand out, serve.
#[cfg(target_os = "linux")]
fn serve(
    driver: sv_gpu_server::driver::Driver,
    socket: &str,
    arena_size: u64,
    clients: usize,
) -> Result<(), Box<dyn std::error::Error>> {
    use std::os::fd::AsFd;
    use std::os::unix::net::UnixListener;
    use std::sync::Arc;
    use sv_gpu_server::{arena, server::Shared, session, table::Objects};
    use sv_transport::cold::{Attachment, ColdMessage, recv, send_with_fds};
    use sv_transport::control::Control;
    use sv_transport::region::{SharedRegion, memfd};

    let geometry = geometry_for(arena_size);
    let arena_size = geometry.arena_size;

    // The arena comes from the driver. The control region does not: §4.1 splits
    // the two by purpose and the control region is "the one only our code
    // touches", so it stays an ordinary memfd. That also keeps the ring atomics
    // on ordinary write-back RAM rather than on whatever a GPU's host-visible
    // heap turns out to be, which is a question nobody has measured and which
    // this design does not need to ask.
    let control_fd = memfd("sv-control", geometry.control_size)?;
    let control_region =
        Arc::new(SharedRegion::map_fd(control_fd.as_fd(), 0, geometry.control_size as usize)?);

    let arena = arena::allocate_and_export(&driver, arena_size)?;
    eprintln!(
        "[gpu] arena {} MiB from the driver, memory type {}, exported as a descriptor",
        arena.len >> 20,
        arena.memory_type
    );
    let arena_fd = arena
        .exported_fd
        .as_ref()
        .ok_or("the export path produced no descriptor")?
        .as_fd();

    let generation = 1u32;
    let control = Control::create(
        Arc::clone(&control_region),
        geometry,
        sv_gpu_server::protocol_ids(),
        generation,
    )?;
    control.stamp_epoch()?;

    let attachment = Attachment {
        control_offset: 0,
        control_len: geometry.control_size,
        // The arena is its own object here, so it begins at offset 0 of the
        // *second* descriptor. On the memfd path both regions are one file and
        // the arena begins after the control region; the client is told which by
        // how many descriptors arrive.
        arena_offset: 0,
        arena_len: arena_size,
        generation,
        run_count: 0,
    };

    let _ = std::fs::remove_file(socket);
    let listener = UnixListener::bind(socket)?;
    eprintln!("[gpu] listening on {socket} for {clients} client(s)");

    for i in 0..clients {
        let (sock, _) = listener.accept()?;
        let (msg, _) = recv(&sock)?;
        match msg {
            ColdMessage::AttachRequest => {
                let mut buf = Vec::new();
                ColdMessage::Attach(attachment.clone()).encode(&mut buf)?;
                send_with_fds(&sock, &buf, &[control_fd.as_fd(), arena_fd])?;
                eprintln!("[gpu] client {i} attached (control + arena descriptors)");
            }
            other => {
                let mut buf = Vec::new();
                ColdMessage::Refuse {
                    code: 1,
                    reason: format!("expected AttachRequest, got {other:?}"),
                }
                .encode(&mut buf)?;
                send_with_fds(&sock, &buf, &[])?;
            }
        }
    }

    let mut objects = Objects::new();
    objects.set_arena(arena.memory, arena.len);
    let shared = Arc::new(Shared {
        driver,
        arena,
        objects: std::sync::Mutex::new(objects),
    });
    let handled = session::serve(
        Arc::new(control),
        shared,
        geometry.ring_slot_count,
        std::time::Duration::from_secs(30),
    )?;
    eprintln!("[gpu] {handled} commands dispatched");
    let _ = std::fs::remove_file(socket);
    Ok(())
}

/// The app-allocated path: attach to `sv-hostd`'s memfd and import it.
#[cfg(target_os = "linux")]
fn attach(
    driver: sv_gpu_server::driver::Driver,
    socket: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    use std::os::fd::AsFd;
    use std::os::unix::net::UnixStream;
    use std::sync::Arc;
    use sv_gpu_server::{arena, server::Shared, session, table::Objects};
    use sv_transport::cold::{ColdMessage, recv, send};
    use sv_transport::control::Control;
    use sv_transport::region::SharedRegion;

    // The daemon may not be listening yet; bounded, because R4 applies to
    // startup too.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let sock = loop {
        match UnixStream::connect(socket) {
            Ok(s) => break s,
            Err(_) if std::time::Instant::now() < deadline => {
                std::thread::sleep(std::time::Duration::from_millis(20))
            }
            Err(e) => return Err(Box::new(e)),
        }
    };
    send(&sock, &ColdMessage::AttachRequest)?;
    let (msg, fd) = recv(&sock)?;
    let at = match msg {
        ColdMessage::Attach(a) => a,
        ColdMessage::Refuse { code, reason } => return Err(format!("refused {code}: {reason}").into()),
        other => return Err(format!("unexpected {other:?}").into()),
    };
    let fd = fd.ok_or("sv-hostd sent no descriptor")?;

    let control_region =
        Arc::new(SharedRegion::map_fd(fd.as_fd(), at.control_offset, at.control_len as usize)?);
    let arena_region =
        Arc::new(SharedRegion::map_fd(fd.as_fd(), at.arena_offset, at.arena_len as usize)?);

    let geometry = geometry_for(at.arena_len);
    let control = Control::create(
        Arc::clone(&control_region),
        sv_transport::control::Geometry { control_size: at.control_len, ..geometry },
        sv_gpu_server::protocol_ids(),
        at.generation,
    )?;
    control.stamp_epoch()?;

    let arena = arena::import_whole(&driver, arena_region)?;
    eprintln!(
        "[gpu] arena {} MiB imported as a host pointer, memory type {}",
        arena.len >> 20,
        arena.memory_type
    );

    let mut objects = Objects::new();
    objects.set_arena(arena.memory, arena.len);
    let shared = Arc::new(Shared {
        driver,
        arena,
        objects: std::sync::Mutex::new(objects),
    });
    let handled = session::serve(
        Arc::new(control),
        shared,
        geometry.ring_slot_count,
        std::time::Duration::from_secs(30),
    )?;
    eprintln!("[gpu] {handled} commands dispatched");
    Ok(())
}

/// The **guest-side** path: allocate the arena out of this machine's own RAM,
/// stamp it, and publish a descriptor over the cold path.
///
/// This is the topology design.md was written for, with the guest running Linux
/// instead of Windows. Nothing about it is a simulation of the Windows case:
///
/// - the arena is guest RAM, pinned, that the *hypervisor* can reach — not a
///   memfd, not driver memory, and not something a descriptor can be made of;
/// - the guest cannot enumerate its own frames, so the descriptor carries
///   `Locator::Scan` and no addresses at all (§4.3);
/// - what crosses the cold path is text and page counts, so it fits down a
///   virtio-serial port, and **no descriptor crosses**, because none can.
///
/// `socket` is the cold path: `/dev/virtio-ports/<name>` in a real guest, or a
/// Unix socket when both halves are on one machine and the point is to test the
/// framing rather than the hypervisor.
#[cfg(target_os = "linux")]
fn guest(
    driver: sv_gpu_server::driver::Driver,
    socket: &str,
    arena_size: u64,
    huge: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    use std::sync::Arc;
    use sv_arena::guestram::{GuestRamProvider, Pages};
    use sv_gpu_server::{arena, server::Shared, session, table::Objects};
    use sv_transport::cold::{ColdMessage, Hello};
    use sv_transport::control::{Control, ENDIAN_MARKER};
    use sv_transport::region::SharedRegion;
    use sv_transport::stream::{open_virtio_port, read_message, write_message};

    let geometry = geometry_for(arena_size);

    // One extent covering both regions (D22), because the host attaches per
    // extent and locating twice would cost two scans for no gain.
    let mut provider =
        GuestRamProvider::new(if huge { Pages::Huge2M } else { Pages::Small });
    let extent = provider.commit_and_stamp(geometry.control_size, geometry.arena_size)?;
    let observed = provider.observed_page_size().unwrap_or(0);
    eprintln!(
        "[gpu] extent {} MiB of guest RAM, run 0x{:016x}, kernel page size {} KiB, stamped {} pages",
        extent.total_len() >> 20,
        extent.desc.run_id,
        observed >> 10,
        extent.desc.control_pages + extent.desc.arena_pages,
    );

    // The window becomes the two regions. They are sub-ranges of one allocation
    // here rather than two objects, because the *host* has to locate one extent
    // and slice it — so the guest must lay them out the same way.
    let window = provider.take_window().ok_or("the provider kept no window")?;
    let base = unsafe { window.base_ptr() };
    let control_region = Arc::new(unsafe {
        SharedRegion::from_raw(base, geometry.control_size as usize)
    });
    let arena_region = Arc::new(unsafe {
        SharedRegion::from_raw(
            base.add(geometry.control_size as usize),
            geometry.arena_size as usize,
        )
    });

    let generation = 1u32;

    // Import before publishing. If the driver refuses the pointer there is
    // nothing to talk to the app side about, and telling it to attach first
    // would leave it waiting on a session that cannot happen.
    let gpu_arena = arena::import_whole(&driver, Arc::clone(&arena_region))?;
    eprintln!(
        "[gpu] arena imported as a host pointer, memory type {}",
        gpu_arena.memory_type
    );

    // **The control region is initialised after the host has located the
    // extent, not before, and this ordering is load-bearing.**
    //
    // design.md §4.3 has the owner "allocate both regions, zero them, write
    // every header field, then store `magic`" as step 1, and offer the extent
    // over the cold path as step 2. §5.1 has the guest stamp every page and the
    // host find them by content. On one machine those two are compatible
    // because the host is handed a descriptor and does not have to search.
    // **Across a hypervisor they contradict each other**: `Control::create`
    // zeroes the control region, which erases exactly the stamps the host is
    // about to sweep for, so the locator finds the arena's pages and none of the
    // control region's.
    //
    // Measured, first run of this path: "located 8192 of 12288 pages" — the
    // 8192 arena pages present and all 4096 control pages missing. Nothing said
    // why; the count was simply short.
    //
    // Publishing first costs nothing, because the design already requires the
    // peer to tolerate an unpublished region: `magic` is written last precisely
    // so an early attacher sees `NotReady` and retries (§4.3 step 4, and
    // sharedgl's missing readiness flag is why). So the order here is: stamp,
    // publish, *let the host locate*, and only then write the header over the
    // stamps it no longer needs.
    eprintln!("[gpu] publishing the extent on {socket}");

    // The cold path. A virtio-serial port when the name looks like one, a Unix
    // socket otherwise — the framing is identical either way, which is the
    // claim `sv_transport::stream` exists to make.
    let mut port: Box<dyn sv_transport::stream::ColdChannel> =
        if socket.starts_with("/dev/virtio-ports/") {
            Box::new(open_virtio_port(socket.rsplit('/').next().unwrap_or(socket))?)
        } else {
            Box::new(std::os::unix::net::UnixStream::connect(socket)?)
        };

    write_message(
        &mut port,
        &ColdMessage::Hello(Box::new(Hello {
            ids: sv_gpu_server::protocol_ids(),
            generation,
            extent: extent.desc.clone(),
            geometry,
            endianness: ENDIAN_MARKER,
            pointer_width: 64,
        })),
    )?;
    let mut scratch = Vec::new();
    match read_message(&mut port, &mut scratch)? {
        ColdMessage::Accept { generation: g } if g == generation => {
            eprintln!("[gpu] the host located the extent and accepted generation {g}");
        }
        ColdMessage::Refuse { code, reason } => {
            return Err(format!("the host refused {code}: {reason}").into());
        }
        other => return Err(format!("unexpected cold-path reply {other:?}").into()),
    }

    // Now the stamps have done their job and the header may overwrite them.
    let control = Control::create(
        Arc::clone(&control_region),
        geometry,
        sv_gpu_server::protocol_ids(),
        generation,
    )?;
    control.stamp_epoch()?;
    eprintln!("[gpu] control region published, generation {generation}");

    let mut objects = Objects::new();
    objects.set_arena(gpu_arena.memory, gpu_arena.len);
    let shared = Arc::new(Shared {
        driver,
        arena: gpu_arena,
        objects: std::sync::Mutex::new(objects),
    });
    let handled = session::serve(
        Arc::new(control),
        shared,
        geometry.ring_slot_count,
        std::time::Duration::from_secs(30),
    )?;
    eprintln!("[gpu] {handled} commands dispatched");
    // The window is owned by the two `SharedRegion`s now; holding it here would
    // be a second owner of the same pages.
    std::mem::forget(window);
    Ok(())
}

/// The geometry both sides use — [`sv_transport::control::Geometry::for_arena`],
/// which is where it lives so that `sv-hostd` and this binary cannot disagree.
#[cfg(any(target_os = "linux", windows))]
fn geometry_for(arena_size: u64) -> sv_transport::control::Geometry {
    sv_transport::control::Geometry::for_arena(arena_size)
}
