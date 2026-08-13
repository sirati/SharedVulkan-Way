//! ```text
//! sv-gpu-server serve   --socket <path> [--device N] [--arena BYTES] [--clients N]
//!                       --clients N = accept up to N *sessions*, each served as
//!                       it arrives, each with its own arena (design.md §0.12)
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
pub fn run_main(argv: Vec<String>) -> std::process::ExitCode {
    match win_run(&argv) {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("sv-gpu-server: {e}");
            std::process::ExitCode::FAILURE
        }
    }
}

#[cfg(windows)]
fn win_run(argv: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    use std::sync::Arc;
    use sv_gpu_server::driver::Driver;
    use sv_gpu_server::{arena, server::Shared, session, table::Objects};
    use sv_transport::cold::{Attachment, ColdMessage};
    use sv_transport::control::Control;
    use sv_transport::region::SharedRegion;
    use sv_transport::stream::{read_message, write_message};
    use sv_transport::{winpipe, winshm};

    let args: Vec<String> = argv.to_vec();
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
        "[gpu] {} ({key}), queue family {}, provider AppNamedShm (named section)",
        driver.name, driver.queue_family
    );

    let geometry = geometry_for(arena_size);

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
                    // Both sides derive these from the session string
                    // ([`winshm::section_name`]), so they are stated here for
                    // the record rather than for the peer's benefit — the
                    // Windows axis has obeyed design.md §1a since it was written
                    // and is the pattern the Linux axis was generalised from.
                    object: winshm::section_name(&format!("{session_name}.ctl")),
                    arena_object: winshm::section_name(&format!("{session_name}.arena")),
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

    let fns = driver.load_fns();
    report_missing(&fns);

    let mut objects = Objects::new();
    objects.set_arena(arena.memory, arena.len);
    let shared = Arc::new(Shared {
        driver: Arc::new(driver),
        arena,
        objects: std::sync::Mutex::new(objects),
        fns: Arc::new(fns),
        queue_lock: Arc::new(std::sync::Mutex::new(())),
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
pub fn run_main(_argv: Vec<String>) -> std::process::ExitCode {
    eprintln!("sv-gpu-server: only Linux and Windows have a cold path in this build");
    std::process::exit(1);
}

#[cfg(target_os = "linux")]
pub fn run_main(argv: Vec<String>) -> std::process::ExitCode {
    match run(&argv) {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("sv-gpu-server: {e}");
            std::process::ExitCode::FAILURE
        }
    }
}

#[cfg(target_os = "linux")]
fn run(argv: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    use sv_gpu_server::arena;
    use sv_gpu_server::driver::Driver;

    let args: Vec<String> = argv.to_vec();
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
    use sv_transport::cold::{Attachment, ColdMessage, recv, send};
    use sv_transport::control::Control;
    use sv_transport::localfd::send_with_fds;
    use sv_transport::region::{SharedRegion, shm_create, shm_path, shm_unlink};

    let geometry = geometry_for(arena_size);
    let arena_size = geometry.arena_size;
    let driver = Arc::new(driver);
    let fns = Arc::new(driver.load_fns());
    report_missing(&fns);
    let queue_lock = Arc::new(std::sync::Mutex::new(()));

    // The arena comes from the driver. The control region does not: §4.1 splits
    // the two by purpose and the control region is "the one only our code
    // touches", so it is a **named** object like every other region in this
    // project. That also keeps the ring atomics on ordinary write-back RAM
    // rather than on whatever a GPU's host-visible heap turns out to be, which
    // is a question nobody has measured and which this design does not need to
    // ask.
    //
    // Naming it is what reduces this path's violation of design.md §1a to
    // exactly one object: the exported dma-buf, which has no name to give.
    let _ = std::fs::remove_file(socket);
    let listener = UnixListener::bind(socket)?;
    eprintln!("[gpu] listening on {socket} for up to {clients} session(s)");

    // **One session per client, each served as it arrives.**
    //
    // This used to gather `clients` handshakes into *one* control region and one
    // ring set, serve none of them until all had arrived, and then run a single
    // `session::serve` until 30 s of idle. So the process served exactly one
    // session in its lifetime, and a client that connected afterwards found a
    // socket that existed and was never answered.
    //
    // That was fine for every harness in this tree and for `vulkaninfo`,
    // `eglinfo`, `glxgears` and `video-probe`, all of which are one process, and
    // it was the whole of what stopped a real application: Firefox forks
    // `glxtest` before the browser exists, that child took the session, the
    // browser was never served, and Firefox exited (design.md §0.11,
    // `docs/firefox-accel.md`). Two ordinary GL programs run one after another
    // gave one attach, not two.
    //
    // `--clients N` now means "accept up to N sessions", which for `N = 1` — what
    // every existing harness passes — is the same observable behaviour as before.
    //
    // # Why each session gets its own arena
    //
    // The obvious reading of "one arena per device" is that the sessions share
    // one, and that is not available without more than a lock. **The arena's
    // allocator lives on the app side**, one per client (`sv-arena`), so two
    // clients over one arena would both allocate from offset zero. Handing each a
    // disjoint sub-range would fix that and needs no lock at all — the
    // `Attachment` already carries `arena_offset` — but it would still hand every
    // client a descriptor for the *whole* object, so isolation between clients
    // would be a convention rather than a fact.
    //
    // A separate allocation per session makes it a fact: a client's descriptor
    // names its own memory and no other's. That is `security-model.md`'s R3
    // argument applied one level up, and it is the reason to prefer it over the
    // partitioned arena, which is otherwise the tidier answer. **The cost is
    // real and is stated rather than hidden**: `N` × `--arena` bytes of device
    // memory, so `--clients 8 --arena 256M` is 2 GiB. `--arena` is the knob.
    let mut sessions: Vec<std::thread::JoinHandle<()>> = Vec::new();
    for i in 0..clients {
        let (sock, _) = listener.accept()?;

        // Everything below is per session, and it is built *after* the accept so
        // that a client that never comes costs nothing.
        let control_name = shm_path(&format!("control.{}.{i}", std::process::id()));
        let control_file = shm_create(&control_name, geometry.control_size)?;
        let control_region = Arc::new(SharedRegion::map_fd(
            control_file.as_fd(),
            0,
            geometry.control_size as usize,
        )?);
        let arena = arena::allocate_and_export(&driver, arena_size)?;
        eprintln!(
            "[gpu] session {i}: arena {} MiB from the driver, memory type {}, exported as a \
             descriptor",
            arena.len >> 20,
            arena.memory_type
        );
        arena::check_external_buffers_keep_the_arena(&driver, &arena)?;
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
            // The control region has a name; the arena does not, and that is
            // what an empty `object` means on this path. It is the one sender in
            // the project that leaves it empty, and design.md §0.7 says why.
            object: control_name.clone(),
            arena_object: String::new(),
            control_offset: 0,
            control_len: geometry.control_size,
            // The arena is its own object here, so it begins at offset 0 of the
            // descriptor that accompanies the message. On the named path both
            // regions are one object and the arena begins after the control
            // region.
            arena_offset: 0,
            arena_len: arena_size,
            generation,
            run_count: 0,
        };

        match recv(&sock) {
            Ok(ColdMessage::AttachRequest) => {
                let mut buf = Vec::new();
                ColdMessage::Attach(attachment).encode(&mut buf)?;
                // One descriptor, and it is the dma-buf. The control region
                // crosses as a name like everything else.
                send_with_fds(&sock, &buf, &[arena_fd])?;
                // Wait for the client to say it has opened the control region
                // *by name* before moving on, for the same reason `sv-hostd`
                // does: the name is unlinked once the client has it, and a
                // client still between "received the message" and "opened the
                // object" would find nothing.
                //
                // This was learned the expensive way. Unlinking without the
                // wait produced `open(named object) failed: errno 2` in the ICD
                // and `MESA: error: ZINK: vkCreateInstance failed
                // (VK_ERROR_INCOMPATIBLE_DRIVER)` two layers up — a message that
                // says nothing about names at all.
                sock.set_read_timeout(Some(std::time::Duration::from_secs(30)))?;
                match recv(&sock) {
                    Ok(ColdMessage::Accept { .. }) => {}
                    Ok(other) => eprintln!("[gpu] session {i} said {other:?} rather than Accept"),
                    Err(e) => eprintln!("[gpu] session {i} never acknowledged the name: {e}"),
                }
                eprintln!(
                    "[gpu] session {i} attached (control region by name, arena as a dma-buf \
                     descriptor — design.md §1a's one exception)"
                );
            }
            Ok(other) => {
                send(
                    &sock,
                    &ColdMessage::Refuse {
                        code: 1,
                        reason: format!("expected AttachRequest, got {other:?}"),
                    },
                )?;
                shm_unlink(&control_name);
                continue;
            }
            Err(e) => {
                eprintln!("[gpu] session {i}: no attach request: {e}");
                shm_unlink(&control_name);
                continue;
            }
        }

        // **Unlink as soon as this client has it open, not at exit.**
        //
        // Unlinking releases the *name*; this process's descriptor and the
        // client's keep the object alive, so nothing above notices. Doing it
        // here is what stops a killed GPU side from leaving a 16 MiB object in
        // `/dev/shm` until the next reboot — and every harness kills the GPU
        // side, because it serves until the app side goes quiet.
        shm_unlink(&control_name);

        let mut objects = Objects::new();
        objects.set_arena(arena.memory, arena.len);
        let shared = Arc::new(Shared {
            driver: Arc::clone(&driver),
            arena,
            objects: std::sync::Mutex::new(objects),
            fns: Arc::clone(&fns),
            queue_lock: Arc::clone(&queue_lock),
        });
        let control = Arc::new(control);
        let slot_count = geometry.ring_slot_count;
        // The socket is held for the session's lifetime rather than dropped
        // here: closing it is how the client learns the GPU side is gone, and a
        // client whose cold path closed under it while its rings were live would
        // read that as a crash.
        sessions.push(std::thread::spawn(move || {
            let _sock = sock;
            let _region = control_region;
            let _file = control_file;
            match session::serve(control, shared, slot_count, std::time::Duration::from_secs(30)) {
                Ok(n) => eprintln!("[gpu] session {i} finished, {n} commands dispatched"),
                Err(e) => eprintln!("[gpu] session {i} ended: {e}"),
            }
        }));
    }

    for s in sessions {
        let _ = s.join();
    }
    let _ = std::fs::remove_file(socket);
    Ok(())
}
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
    use sv_transport::region::{SharedRegion, open_named};

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
    let at = match recv(&sock)? {
        ColdMessage::Attach(a) => a,
        ColdMessage::Refuse { code, reason } => return Err(format!("refused {code}: {reason}").into()),
        other => return Err(format!("unexpected {other:?}").into()),
    };
    if at.object.is_empty() {
        return Err("the daemon sent no object name; this build cannot take a descriptor \
                    across the boundary (design.md §1a)"
            .into());
    }
    // **The GPU side opens the object itself, by name.** This is the whole of
    // design.md §1a on this path: what arrived over the socket was text, and it
    // would have arrived identically down a virtio-serial port from a daemon on
    // the other side of a hypervisor.
    let fd = open_named(&at.object)?;
    eprintln!("[gpu] opened {} by name (no descriptor was passed)", at.object);

    let control_region =
        Arc::new(SharedRegion::map_fd(fd.as_fd(), at.control_offset, at.control_len as usize)?);
    let arena_region =
        Arc::new(SharedRegion::map_fd(fd.as_fd(), at.arena_offset, at.arena_len as usize)?);
    // Tell the daemon the name has been resolved, so it may release it.
    send(&sock, &ColdMessage::Accept { generation: at.generation })?;

    let geometry = geometry_for(at.arena_len);
    let control = Control::create(
        Arc::clone(&control_region),
        sv_transport::control::Geometry { control_size: at.control_len, ..geometry },
        sv_gpu_server::protocol_ids(),
        at.generation,
    )?;
    control.stamp_epoch()?;

    // design.md §5.2: "Slabs are **imported** lazily." Nothing is asked of the
    // driver here; the app side sends `svImportArenaSlab` for each slab it
    // allocates in, and each import costs about 4.4 ms of the ~70 ms per GiB
    // this would otherwise all pay at startup (`arena::import_slab`).
    //
    // This is the *import* provider, and it is the only one where the question
    // arises: on the GPU-allocated path the arena **is** a `vkAllocateMemory`
    // this process made before the app side existed, so there is nothing to
    // defer. §5.2 does not say that and reads as if lazy import were a property
    // of the arena; it is a property of the provider.
    let arena = arena::map_lazy(arena_region);
    eprintln!(
        "[gpu] arena {} MiB mapped, {} slabs of {} MiB to import on demand",
        arena.len >> 20,
        arena.len.div_ceil(geometry.arena_slab_size),
        geometry.arena_slab_size >> 20
    );

    let mut objects = Objects::new();
    objects.set_lazy_arena(arena.len, geometry.arena_slab_size);
    let fns = driver.load_fns();
    report_missing(&fns);
    let shared = Arc::new(Shared {
        driver: Arc::new(driver),
        arena,
        objects: std::sync::Mutex::new(objects),
        fns: Arc::new(fns),
        queue_lock: Arc::new(std::sync::Mutex::new(())),
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

    let fns = driver.load_fns();
    report_missing(&fns);

    let mut objects = Objects::new();
    objects.set_arena(gpu_arena.memory, gpu_arena.len);
    let shared = Arc::new(Shared {
        driver: Arc::new(driver),
        arena: gpu_arena,
        objects: std::sync::Mutex::new(objects),
        fns: Arc::new(fns),
        queue_lock: Arc::new(std::sync::Mutex::new(())),
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

/// Say once, at startup, which of the profile's entry points this driver lacks.
///
/// Reported here rather than discovered per command: "this driver has no
/// `vkCmdBeginRendering`" is a fact about the configuration, and an application
/// should be told before it has drawn nothing for ten seconds. An empty list is
/// the normal case on a modern driver and prints nothing.
// Both platforms: a driver missing part of the profile is worth saying out
// loud wherever the GPU side runs.
#[cfg(any(target_os = "linux", windows))]
fn report_missing(fns: &sv_gpu_entry::DriverFns) {
    let missing = fns.missing();
    if missing.is_empty() {
        return;
    }
    eprintln!(
        "[gpu] the driver is missing {} of the profile's entry points, starting with {:?}",
        missing.len(),
        &missing[..missing.len().min(8)]
    );
}

/// The geometry both sides use — [`sv_transport::control::Geometry::for_arena`],
/// which is where it lives so that `sv-hostd` and this binary cannot disagree.
#[cfg(any(target_os = "linux", windows))]
fn geometry_for(arena_size: u64) -> sv_transport::control::Geometry {
    sv_transport::control::Geometry::for_arena(arena_size)
}
