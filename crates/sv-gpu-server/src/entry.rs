//! ```text
//! sv-gpu-server serve   --socket <path> [--device N] [--arena BYTES] [--clients N]
//!                       --clients N = accept up to N *sessions*, each served as
//!                       it arrives, each with its own arena (docs/design.md §0.12)
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
    match argv.get(1).map(String::as_str) {
        Some("serve") => win_serve(argv),
        Some("guest") => win_guest(argv),
        _ => {
            eprintln!(
                "usage:\n  \
                 sv-gpu-server serve --session <name> [--device N] [--arena BYTES]\n  \
                 sv-gpu-server guest --port <tcp> [--device N] [--arena BYTES] [--idle SECS]"
            );
            Err("no subcommand".into())
        }
    }
}

#[cfg(windows)]
fn win_serve(argv: &[String]) -> Result<(), Box<dyn std::error::Error>> {
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
                    // Windows axis has obeyed docs/design.md §1a since it was written
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
        pool_lock: Arc::new(std::sync::Mutex::new(())),
        video_stable: Default::default(),
        poisoned: Default::default(),
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

/// The **Windows guest side** of the cross-VM topology.
///
/// ```text
/// sv-gpu-server guest --port <tcp> [--device N] [--arena BYTES] [--idle SECS] [--allow-small-pages]
/// ```
///
/// This is the mode `testhost` needs and the one that did not exist: the
/// Windows path had only `serve`, which puts both halves inside the guest and
/// hands the app side two **named sections** — objects with no meaning outside
/// that kernel. An application on the Linux host cannot open a section name, so
/// `serve` cannot reach it however it is started.
///
/// The Linux [`guest`] subcommand is the same shape and this is deliberately a
/// mirror of it rather than a new design. What differs is only the spelling of
/// the two OS-specific steps:
///
/// | step | Linux `guest` | here |
/// |---|---|---|
/// | commit + pin | `mmap(MAP_POPULATE)` + `mlock` ([`sv_arena::guestram`]) | `VirtualAlloc(MEM_LARGE_PAGES)`, else `VirtualAlloc`+`VirtualLock` ([`awe_probe::alloc`]) |
/// | stamp | one `arena_record` per 4 KiB page | the same, from the same crate |
/// | cold path | `/dev/virtio-ports/<name>` | a **TCP listener on loopback** |
///
/// # Why the cold path is TCP, and why that is not a hole
///
/// The Linux path rides a virtio-serial port. This guest has exactly two —
/// `org.sirati.wifi.0` and `org.sirati.display.0`, both spoken for — and adding
/// a third is a change to the machine's NixOS configuration and a reboot of a
/// VM other people are using. So the channel is a loopback TCP listener the
/// host reaches with `ssh -L <unix-socket>:127.0.0.1:<port>`, which puts a Unix
/// socket on the host at the far end of it and lets `sv-hostd qemu-serve`
/// connect to it with **no change at all** — [`sv_transport::stream`]'s
/// `ColdChannel` is blanket-implemented for anything `Read + Write`, which is
/// the claim that module exists to make.
///
/// This is waycompose's `probe-relay.c` argument and it holds for the same
/// reason: **what crosses is framed text and page counts**, never a descriptor,
/// because on this path there is no descriptor to cross — the guest cannot name
/// its own frames and the host finds the pages by content (§5.1, D21). A
/// transport that *could* carry a descriptor would not make this one carry one.
/// The listener is bound to `127.0.0.1` so the only way to it is through an ssh
/// channel the host already had.
///
/// # One session per process, on purpose
///
/// Unlike Linux `serve`, this takes no `--clients`. A cross-VM session is one
/// pinned window, one extent for the host to locate, and one control region;
/// serving several from one process would mean the host locating one extent and
/// slicing it, which is a wire change for no isolation. Firefox needs several
/// sessions (§0.11 — it forks `glxtest` before the browser exists), and the way
/// to get them here is several of these processes on several ports, each with
/// its own window. That is §0.12's "a separate allocation per session makes it
/// a fact" arriving on the cross-VM path, and it costs the same `--arena` bytes
/// per session that it costs there.
#[cfg(windows)]
fn win_guest(args: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    if args.iter().any(|a| a == "--ivshmem-slot") {
        return win_guest_ivshmem(args);
    }
    crate::fault::install();
    use awe_probe::alloc::{Mode, Window};
    use std::sync::Arc;
    use sv_gpu_server::driver::Driver;
    use sv_gpu_server::{arena, server::Shared, session, table::Objects};
    use sv_transport::cold::{ColdMessage, ExtentDesc, Granularity, Hello, Locator};
    use sv_transport::control::{Control, ENDIAN_MARKER};
    use sv_transport::region::{PAGE, SharedRegion};
    use sv_transport::stream::{read_message, write_message};

    let arg = |k: &str| -> Option<String> {
        args.iter().position(|a| a == k).and_then(|i| args.get(i + 1)).cloned()
    };
    let port: u16 = arg("--port").ok_or("--port is required")?.parse()?;
    let device: usize = arg("--device").map_or(Ok(0), |s| s.parse())?;
    let arena_size: u64 = arg("--arena").map_or(Ok(64 << 20), |s| s.parse())?;
    let idle: u64 = arg("--idle").map_or(Ok(30), |s| s.parse())?;

    let geometry = geometry_for(arena_size);
    let total = geometry.control_size + geometry.arena_size;
    let extent_pages = (total / PAGE as u64) as usize;

    // Large pages, and since waycompose-design.md §86 **only** large pages unless
    // the caller says otherwise. 64 MiB of 4 KiB locked pages measured 1471
    // physical runs and the same window in 2 MiB pages measured 22
    // (`measured-gpu-side.md`), but that is no longer the reason. §84.5
    // measured `VirtualLock`ed 4 KiB pages *moving to another frame* on the
    // deployment guest while locked, in bursts, and §84.7 recorded the guest
    // hard-hanging during the one run that had the host writing into such an
    // arena. The host writes this arena on every command. A page that moved
    // under it would put the host's writes into a frame Windows has given to
    // someone else, so the old "a fragmented arena that runs beats a tidy one
    // that does not" is the wrong trade here: refused, with the reason, and
    // `--allow-small-pages` (or `SV_ALLOW_SMALL_PAGES=1`) puts the fallback
    // back for a guest where that is known to be safe.
    let small_ok = args.iter().any(|a| a == "--allow-small-pages")
        || std::env::var_os("SV_ALLOW_SMALL_PAGES").is_some_and(|v| v == "1");
    let order: &[Mode] = if small_ok { &[Mode::LargePages, Mode::LockedVa] } else { &[Mode::LargePages] };
    let window_pages = awe_probe::alloc::round_pages(order, extent_pages);
    let mut window = Window::acquire(order, window_pages).map_err(|e| {
        if small_ok {
            format!("no guest-RAM window: {e}")
        } else {
            format!(
                "no large-page guest-RAM window: {e}. The VirtualLock fallback is refused \
                 (waycompose-design.md §84.5, §86): this account needs SeLockMemoryPrivilege \
                 and 2 MiB of contiguous free memory per page, or --allow-small-pages on a \
                 guest where locked 4 KiB pages are known to stay put"
            )
        }
    })?;
    if window.pages < extent_pages {
        return Err(format!(
            "the allocator gave {} pages of the {extent_pages} the geometry needs",
            window.pages
        )
        .into());
    }
    eprintln!(
        "[gpu] {} window: {} MiB of guest RAM, {} pages",
        window.mode.as_str(),
        window.size() >> 20,
        window.pages,
    );

    // Stamped here rather than through `Window::stamp`, which writes every page
    // of the *window*. A large-page allocation is rounded up to the 2 MiB
    // granularity, so the window can be larger than the extent, and pages
    // stamped past its end carry indices the host is not expecting — a page
    // that turns up where the extent does not reach is exactly what the
    // stitched-view check refuses. So this stamps the extent and nothing more,
    // which is what `sv_arena::guestram` does on the Linux path.
    let run_id = awe_probe::new_run_id() | 1;
    let mode_code = window.mode.code();
    let gpas: Vec<u64> = (0..extent_pages)
        .map(|p| window.pfns().map(|f| (f[p] as u64) << 12).unwrap_or(0))
        .collect();
    {
        let bytes = window.as_mut_slice();
        for i in 0..extent_pages {
            let page = arena_record::make_guest_page(run_id, i as u32, mode_code, gpas[i]);
            bytes[i * PAGE..(i + 1) * PAGE].copy_from_slice(&page);
        }
    }
    let extent = ExtentDesc {
        run_id,
        // Understated, exactly as the Linux provider understates it, and for
        // the same reason: claiming `Large2M` switches off the R26 canary, and
        // this path has not earned that. `mode` above records what was actually
        // obtained, in every page, where the host can read it.
        granularity: Granularity::Small4K,
        control_pages: geometry.control_size / PAGE as u64,
        arena_pages: geometry.arena_size / PAGE as u64,
        // The guest cannot enumerate its own frames on the two `VirtualAlloc`
        // paths — that is the whole reason the host scans — so `Scan` it is,
        // even when `pfns()` happened to be `Some`. D21.
        locator: Locator::Scan,
    };
    eprintln!(
        "[gpu] extent {} MiB of guest RAM, run 0x{run_id:016x}, stamped {extent_pages} pages",
        total >> 20,
    );

    // The cold path. Bound before the driver is opened so that a host waiting
    // on the port sees it come up promptly; opening a Vulkan device takes long
    // enough that the other order reads as a connection that is refused.
    let listener = std::net::TcpListener::bind(("127.0.0.1", port))?;
    eprintln!("[gpu] publishing the extent on 127.0.0.1:{port}");
    let (mut port_stream, _) = listener.accept()?;
    // Nagle would hold a 200-byte handshake frame waiting for more to send,
    // against a peer that is waiting for exactly that frame. The Linux path
    // never met this because a virtio-serial port has no Nagle.
    port_stream.set_nodelay(true)?;

    write_message(
        &mut port_stream,
        &ColdMessage::Hello(Box::new(Hello {
            ids: sv_gpu_server::protocol_ids(),
            generation: 1,
            extent: extent.clone(),
            geometry,
            endianness: ENDIAN_MARKER,
            pointer_width: 64,
        })),
    )?;
    let mut scratch = Vec::new();
    match read_message(&mut port_stream, &mut scratch)? {
        ColdMessage::Accept { generation: 1 } => {
            eprintln!("[gpu] the host located the extent and accepted generation 1");
        }
        ColdMessage::Refuse { code, reason } => {
            return Err(format!("the host refused {code}: {reason}").into());
        }
        other => return Err(format!("unexpected cold-path reply {other:?}").into()),
    }

    // **After the Accept, never before.** `Control::create` zeroes the control
    // region, which erases exactly the stamps the host sweeps for; publishing
    // the header first cost the Linux path a run that reported "located 8192 of
    // 12288 pages" with nothing to say why. The ordering is the same here and
    // the reason is the same, so it is written down in both places rather than
    // referred to from one.
    let base = window.base as *mut u8;
    let control_region =
        Arc::new(unsafe { SharedRegion::from_raw(base, geometry.control_size as usize) });
    let arena_region = Arc::new(unsafe {
        SharedRegion::from_raw(
            base.add(geometry.control_size as usize),
            geometry.arena_size as usize,
        )
    });

    let driver = Driver::open(device)?;
    eprintln!(
        "[gpu] {} ({}), queue family {}, provider GuestRam (VirtualAlloc, pinned)",
        driver.name,
        driver.driver_key(),
        driver.queue_family
    );
    // Imported before the header is written, so that a driver which refuses the
    // pointer fails while there is still nothing on the far side waiting.
    let gpu_arena = arena::import_whole(&driver, Arc::clone(&arena_region))?;
    eprintln!(
        "[gpu] arena imported as a host pointer, memory type {}",
        gpu_arena.memory_type
    );

    let control = Control::create(
        Arc::clone(&control_region),
        geometry,
        sv_gpu_server::protocol_ids(),
        1,
    )?;
    control.stamp_epoch()?;
    eprintln!("[gpu] control region published, generation 1");

    // THE DOORBELL (waycompose-design.md §36.6), the guest half, on Windows.
    //
    // This is the side the 216.9 ms was measured on. Without it every wait here
    // runs its slice, and `WaitOnAddress` takes whole milliseconds so even the
    // ladder floors at 1 ms; a byte on a virtio-serial port crosses in ~9 µs.
    //
    // After `Control::create`, because the `Doorman` rings bells in this region.
    // Optional and warning-only: a guest without the port is the guest that
    // shipped, and this must never be the reason a session fails to start.
    let control = Arc::new(control);
    let _doorman = match arg("--bell") {
        None => None,
        Some(name) => match sv_transport::vdoorbell::PortHandle::guest_port(&name)
            .and_then(|p| sv_transport::vdoorbell::attach(p, Arc::clone(&control)))
        {
            Ok(d) => {
                eprintln!("[gpu] doorbell attached on \\\\.\\Global\\{name}");
                Some(d)
            }
            Err(e) => {
                eprintln!("[gpu] no doorbell on {name} ({e}); waits will run out their slice");
                None
            }
        },
    };

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
        pool_lock: Arc::new(std::sync::Mutex::new(())),
        video_stable: Default::default(),
        poisoned: Default::default(),
    });
    let handled = session::serve(
        control,
        shared,
        geometry.ring_slot_count,
        std::time::Duration::from_secs(idle),
    )?;
    eprintln!("[gpu] {handled} commands dispatched");
    // The two `SharedRegion`s own these pages now; dropping the window here
    // would unlock and free memory the host still has mapped.
    std::mem::forget(window);
    Ok(())
}

/// The Windows guest side **on an ivshmem device** (waycompose-design.md §87).
///
/// ```text
/// sv-gpu-server guest --port <tcp> --ivshmem-slot <pci slot> [--device N]
///                     [--arena BYTES] [--sessions N] [--idle SECS] [--bell PORT]
/// ```
///
/// [`win_guest`] with the arena moved from large-page guest RAM, which the
/// VM host can only reach as root, onto BAR2 of an ivshmem device, whose
/// backing file it opens as the user. What that changes:
///
/// * **one process, several sessions.** The device's driver gives the BAR to
///   one handle and the doorbell's port takes one opener, so this process maps
///   both once and serves each connection on `--port` as its own session in its
///   own slot of the BAR ([`crate::ivshmem`]): its own control region, its own
///   arena, its own `VkDevice`, exactly the isolation one process per session
///   gave. A slot whose session ended cleanly is given to the next one; one
///   whose session left objects behind is quarantined, its device kept, so
///   that nothing is destroyed under a driver that still holds it.
/// * **no window, no lock, no large pages.** The BAR's pages are the device's,
///   not this process's working set, so §84.5's moving frames and §86's
///   large-pages-or-refuse rule have nothing to apply to.
/// * **no locate.** The `Hello` carries the slot's offset in the region
///   (`Locator::Ivshmem`); the host maps the file there and checks the stamps,
///   which are written exactly as before so that a host reading the wrong
///   slot or the wrong file says so.
///
/// The import is the same `VK_EXT_external_memory_host` import as on guest
/// RAM. Intel's driver taking a pointer into a BAR mapping was the open
/// question, and `sv-ivshmem-probe` measured it: the query and the import
/// succeed and a GPU copy through the BAR is exact (§87).
#[cfg(windows)]
fn win_guest_ivshmem(args: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    crate::fault::install();
    use std::sync::Arc;
    use sv_gpu_server::ivshmem::{SlotPlan, Slots};

    let arg = |k: &str| -> Option<String> {
        args.iter().position(|a| a == k).and_then(|i| args.get(i + 1)).cloned()
    };
    let port: u16 = arg("--port").ok_or("--port is required")?.parse()?;
    let device: usize = arg("--device").map_or(Ok(0), |s| s.parse())?;
    let arena_size: u64 = arg("--arena").map_or(Ok(64 << 20), |s| s.parse())?;
    let idle: u64 = arg("--idle").map_or(Ok(30), |s| s.parse())?;
    let max: u32 = arg("--sessions").map_or(Ok(0), |s| s.parse())?;
    let pci = arg("--ivshmem-slot").ok_or("--ivshmem-slot is required")?;
    let pci: u8 = match pci.strip_prefix("0x") {
        Some(h) => u8::from_str_radix(h, 16)?,
        None => pci.parse()?,
    };

    let geometry = geometry_for(arena_size);
    let started = std::time::Instant::now();
    let (view, size, path) = wc_handle::map_ivshmem_slot(pci)?;
    let plan = SlotPlan::new(size, geometry, max)?;
    eprintln!(
        "[gpu] ivshmem slot {pci:#04x} ({path}) mapped in {:.2?}: {} MiB, {} session slot(s) of \
         {} MiB ({} MiB control + {} MiB arena)",
        started.elapsed(),
        size >> 20,
        plan.count,
        plan.slot_len >> 20,
        plan.control_len >> 20,
        plan.arena_len >> 20,
    );
    // The view owns the device handle, and so the mapping; it lives as long as
    // the process, and every session's regions borrow from it.
    let bar = view.ptr().cast_mut() as usize;
    let slots = Arc::new(Slots::new(plan));

    // THE DOORBELL, for every session at once. One port, one opener, so one
    // reader that rings each byte's lane in every live session
    // (`vdoorbell::ControlSet`), and one writer every session's bells poke.
    let set = sv_transport::vdoorbell::ControlSet::new();
    let _doorman = match arg("--bell") {
        None => None,
        Some(name) => match sv_transport::vdoorbell::PortHandle::guest_port(&name)
            .and_then(|p| sv_transport::vdoorbell::attach_set(p, Arc::clone(&set)))
        {
            Ok(d) => {
                eprintln!("[gpu] doorbell attached on \\\\.\\Global\\{name}, shared by every session");
                Some(d)
            }
            Err(e) => {
                eprintln!("[gpu] no doorbell on {name} ({e}); waits will run out their slice");
                None
            }
        },
    };

    let listener = std::net::TcpListener::bind(("127.0.0.1", port))?;
    // The launcher waits for these words; they are the same as `win_guest`'s.
    eprintln!("[gpu] publishing the extent on 127.0.0.1:{port} (ivshmem, one session per connection)");
    let mut n = 0u64;
    for conn in listener.incoming() {
        let mut stream = match conn {
            Ok(s) => s,
            Err(e) => {
                eprintln!("[gpu] accept: {e}");
                continue;
            }
        };
        let _ = stream.set_nodelay(true);
        n += 1;
        let session = n;
        let slots = Arc::clone(&slots);
        let set = Arc::clone(&set);
        std::thread::spawn(move || {
            let Some(slot) = slots.acquire() else {
                let (free, busy, q) = slots.census();
                let reason = format!(
                    "all {} GPU session slots are taken ({busy} busy, {q} quarantined, {free} free)",
                    slots.plan.count
                );
                eprintln!("[gpu] session {session}: refused: {reason}");
                let _ = sv_transport::stream::write_message(
                    &mut stream,
                    &sv_transport::cold::ColdMessage::Refuse { code: 4, reason },
                );
                return;
            };
            let outcome = ivshmem_session(session, slot, bar, &slots.plan, device, idle, &set, stream);
            match outcome {
                Ok((cmds, clean)) => {
                    if clean {
                        slots.release(slot);
                    } else {
                        slots.quarantine(slot);
                    }
                    let (free, busy, q) = slots.census();
                    eprintln!(
                        "[gpu] session {session} (slot {slot}) finished, {cmds} commands; slot {}; \
                         slots free {free}, busy {busy}, quarantined {q}",
                        if clean { "released" } else { "quarantined" }
                    );
                }
                Err((e, clean)) => {
                    if clean {
                        slots.release(slot);
                    } else {
                        slots.quarantine(slot);
                    }
                    eprintln!(
                        "[gpu] session {session} (slot {slot}) ended: {e}; slot {}",
                        if clean { "released" } else { "quarantined" }
                    );
                }
            }
        });
    }
    drop(view);
    Ok(())
}

/// One session in one slot of the BAR: stamp, hand the host the offset, wait
/// for its `Accept`, open a device, import the slot's arena, serve.
///
/// Returns the command count and whether the slot may be reused; on error,
/// the message and the same verdict. A session whose device was never opened
/// left nothing in the driver, so its slot is always clean.
#[cfg(windows)]
#[allow(clippy::too_many_arguments)]
fn ivshmem_session(
    session: u64,
    slot: u32,
    bar: usize,
    plan: &sv_gpu_server::ivshmem::SlotPlan,
    device: usize,
    idle: u64,
    set: &std::sync::Arc<sv_transport::vdoorbell::ControlSet>,
    mut stream: std::net::TcpStream,
) -> Result<(u64, bool), (String, bool)> {
    use std::sync::Arc;
    use sv_gpu_server::driver::Driver;
    use sv_gpu_server::{arena, server::Shared, session as sess, table::Objects};
    use sv_transport::cold::{ColdMessage, ExtentDesc, Granularity, Hello, Locator};
    use sv_transport::control::{Control, ENDIAN_MARKER};
    use sv_transport::region::{PAGE, SharedRegion};
    use sv_transport::stream::{read_message, write_message};

    let fail = |e: String| (e, true);
    let offset = plan.offset(slot);
    // SAFETY: `offset + slot_len <= region_len` by `SlotPlan`, the BAR mapping
    // lives as long as the process, and this slot is this thread's alone until
    // it is released.
    let base = unsafe { (bar as *mut u8).add(offset as usize) };
    let pages = (plan.slot_len / PAGE as u64) as usize;
    let run_id = awe_probe::new_run_id() | 1;
    for i in 0..pages {
        let page = arena_record::make_guest_page(run_id, i as u32, arena_record::MODE_IVSHMEM, 0);
        // SAFETY: page `i` of this slot, inside the mapping.
        unsafe { std::ptr::copy_nonoverlapping(page.as_ptr(), base.add(i * PAGE), PAGE) };
    }
    std::sync::atomic::fence(std::sync::atomic::Ordering::Release);
    eprintln!(
        "[gpu] session {session}: slot {slot} at +{} MiB, run 0x{run_id:016x}, stamped {pages} pages",
        offset >> 20
    );

    let geometry = sv_transport::control::Geometry::for_arena(plan.arena_len);
    write_message(
        &mut stream,
        &ColdMessage::Hello(Box::new(Hello {
            ids: sv_gpu_server::protocol_ids(),
            generation: 1,
            extent: ExtentDesc {
                run_id,
                // Nothing here is a page frame; the granularity says what the
                // stamps were written at, which is 4 KiB.
                granularity: Granularity::Small4K,
                control_pages: plan.control_pages(),
                arena_pages: plan.arena_pages(),
                locator: Locator::Ivshmem { region_offset: offset },
            },
            geometry,
            endianness: ENDIAN_MARKER,
            pointer_width: 64,
        })),
    )
    .map_err(|e| fail(format!("Hello: {e}")))?;
    let mut scratch = Vec::new();
    match read_message(&mut stream, &mut scratch).map_err(|e| fail(format!("no reply to Hello: {e}")))? {
        ColdMessage::Accept { generation: 1 } => {}
        ColdMessage::Refuse { code, reason } => return Err(fail(format!("the host refused {code}: {reason}"))),
        other => return Err(fail(format!("unexpected cold-path reply {other:?}"))),
    }

    // After the Accept, never before: `Control::create` zeroes the stamps the
    // host has just checked.
    let control_region = Arc::new(unsafe { SharedRegion::from_raw(base, plan.control_len as usize) });
    let arena_region = Arc::new(unsafe {
        SharedRegion::from_raw(base.add(plan.control_len as usize), plan.arena_len as usize)
    });

    let t = std::time::Instant::now();
    let setup = (|| -> Result<_, String> {
        let driver = Driver::open(device).map_err(|e| format!("Driver::open: {e}"))?;
        let gpu_arena = arena::import_whole(&driver, Arc::clone(&arena_region))
            .map_err(|e| format!("importing the slot's arena: {e}"))?;
        let control =
            Control::create(Arc::clone(&control_region), geometry, sv_gpu_server::protocol_ids(), 1)
                .map_err(|e| format!("Control::create: {e}"))?;
        control.stamp_epoch().map_err(|e| format!("stamp_epoch: {e}"))?;
        Ok((driver, gpu_arena, control))
    })();
    let (driver, gpu_arena, control) = match setup {
        Ok(v) => v,
        Err(e) => {
            let _ = write_message(&mut stream, &ColdMessage::Refuse { code: 5, reason: e.clone() });
            return Err(fail(e));
        }
    };
    eprintln!(
        "[gpu] session {session}: {} ({}), arena {} MiB of the BAR imported as a host pointer, \
         memory type {}, device, import and control region in {:.2?}",
        driver.name,
        driver.driver_key(),
        gpu_arena.len >> 20,
        gpu_arena.memory_type,
        t.elapsed()
    );
    let arena_memory = gpu_arena.memory;
    let control = Arc::new(control);
    set.add(&control);
    // PUBLISHED. On guest RAM the host located every session before any ICD
    // existed, so the control region was always published by the time one
    // attached. Here the ICD is what asks for the session, and the host must
    // not hand it the slot while the stamps are still where the control
    // header goes — the ICD reads `SVARENA1` as a foreign magic and refuses
    // (measured, the first run of §87). So the host waits for this word.
    // A host that has gone sends no application; the session then times out
    // waiting for its rings to be claimed, which is the ordinary end.
    if let Err(e) = write_message(&mut stream, &ColdMessage::Ping) {
        eprintln!("[gpu] session {session}: could not say it is published: {e}");
    }

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
        pool_lock: Arc::new(std::sync::Mutex::new(())),
        video_stable: Default::default(),
        poisoned: Default::default(),
    });
    let served = sess::serve(
        Arc::clone(&control),
        Arc::clone(&shared),
        geometry.ring_slot_count,
        std::time::Duration::from_secs(idle),
    );
    // The stream is the host's view of this session; closing it now says the
    // session is over. Nothing reads it any more.
    drop(stream);

    // Is the slot reusable? If the application destroyed what it made, at
    // once. If it left objects behind of types this side knows how to
    // destroy, after destroying them on the idle device (`Objects::
    // destroy_leftovers`), which Firefox's `glxtest` needs every time. Only if
    // something else is left are the device and its import kept — leaked for
    // the life of the process, deliberately — and the slot with them.
    let leftovers = shared.objects.lock().map(|o| o.leftovers()).unwrap_or_default();
    let mut clean = Arc::strong_count(&shared) == 1;
    if clean && !leftovers.is_empty() {
        let destroyed = shared.objects.lock().map_err(|_| vec!["poisoned".to_string()]).and_then(|mut o| unsafe {
            let _ = shared.driver.device.device_wait_idle();
            o.destroy_leftovers(&shared.driver.device, arena_memory)
        });
        match destroyed {
            Ok(n) => eprintln!("[gpu] session {session}: destroyed {n} object(s) it left behind ({leftovers:?})"),
            Err(kinds) => {
                eprintln!(
                    "[gpu] session {session}: left behind {leftovers:?}, including {kinds:?}, which \
                     this side does not destroy; keeping its device and slot {slot}"
                );
                clean = false;
            }
        }
    }
    if clean {
        drop(control);
        match Arc::try_unwrap(shared) {
            Ok(s) => {
                unsafe {
                    let _ = s.driver.device.device_wait_idle();
                    s.driver.device.free_memory(arena_memory, None);
                }
                // `Driver::drop` destroys the device and the instance.
                drop(s);
            }
            Err(s) => {
                std::mem::forget(s);
                return Err(("the session's state is still shared; slot kept".into(), false));
            }
        }
    } else {
        std::mem::forget(shared);
        std::mem::forget(control);
    }
    drop(control_region);
    drop(arena_region);
    match served {
        Ok(n) => Ok((n, clean)),
        Err(e) => Err((e, clean)),
    }
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
            let bell = arg("--bell");
            guest(driver, &socket, arena_size, huge, bell.as_deref())
        }
        _ => {
            eprintln!(
                "usage: sv-gpu-server <serve|attach|guest> --socket <path> \
                 [--device N] [--arena BYTES] [--clients N] [--huge] [--bell PORT]"
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
    let pool_lock = Arc::new(std::sync::Mutex::new(()));

    // The arena comes from the driver. The control region does not: §4.1 splits
    // the two by purpose and the control region is "the one only our code
    // touches", so it is a **named** object like every other region in this
    // project. That also keeps the ring atomics on ordinary write-back RAM
    // rather than on whatever a GPU's host-visible heap turns out to be, which
    // is a question nobody has measured and which this design does not need to
    // ask.
    //
    // Naming it is what reduces this path's violation of docs/design.md §1a to
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
    // browser was never served, and Firefox exited (docs/design.md §0.11,
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
            // the project that leaves it empty, and docs/design.md §0.7 says why.
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
                     descriptor — docs/design.md §1a's one exception)"
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
            pool_lock: Arc::clone(&pool_lock),
            video_stable: Default::default(),
            poisoned: Default::default(),
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
            match session::serve(control, Arc::clone(&shared), slot_count, std::time::Duration::from_secs(30)) {
                Ok(n) => eprintln!("[gpu] session {i} finished, {n} commands dispatched"),
                Err(e) => eprintln!("[gpu] session {i} ended: {e}"),
            }
            // What the application left in the table, for the record
            // (waycompose-design.md §87): the ivshmem path destroys these to
            // reuse a slot, and an entry here that the application *did*
            // destroy would be destroyed twice. Printed, never acted on, on
            // this path.
            if let Ok(mut o) = shared.objects.lock() {
                let left = o.leftover_handles();
                if !left.is_empty() {
                    eprintln!("[gpu] session {i} left {} object(s): {left:x?}", left.len());
                    // The ivshmem path's cleanup, run here on request so it can
                    // be proven on a local driver, under the validation layer,
                    // before the guest's driver sees it (§87).
                    if std::env::var_os("SV_DESTROY_LEFTOVERS").is_some_and(|v| v == "1") {
                        let arena = shared.arena.memory;
                        let d = &shared.driver.device;
                        let r = unsafe {
                            let _ = d.device_wait_idle();
                            o.destroy_leftovers(d, arena)
                        };
                        eprintln!("[gpu] session {i}: destroy_leftovers -> {r:?}");
                    }
                }
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
                    across the boundary (docs/design.md §1a)"
            .into());
    }
    // **The GPU side opens the object itself, by name.** This is the whole of
    // docs/design.md §1a on this path: what arrived over the socket was text, and it
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

    // docs/design.md §5.2: "Slabs are **imported** lazily." Nothing is asked of the
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
        pool_lock: Arc::new(std::sync::Mutex::new(())),
        video_stable: Default::default(),
        poisoned: Default::default(),
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
    bell: Option<&str>,
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
    // docs/design.md §4.3 has the owner "allocate both regions, zero them, write
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

    // THE DOORBELL (waycompose-design.md §36.6). Wired here and not earlier:
    // `Doorman` rings bells in this control region, so the region has to exist
    // and be stamped before anything can answer the door.
    //
    // Without it, every wait on this side runs out its slice, because a
    // `FUTEX_WAKE` from the host reaches nobody across the hypervisor —
    // 216.9 ms a present against 1.7-2.1 ms local. With it, a wake crosses in
    // ~9 µs (measured, §36.6.3).
    //
    // Optional, and a failure to open the port is a warning rather than an
    // error: a session without a doorbell is exactly the session that shipped,
    // and refusing to start because the *optimisation* is unavailable would
    // make this strictly worse than not having built it.
    let control = Arc::new(control);
    let _doorman = match bell {
        None => None,
        Some(name) => match sv_transport::vdoorbell::PortHandle::guest_port(name)
            .and_then(|p| sv_transport::vdoorbell::attach(p, Arc::clone(&control)))
        {
            Ok(d) => {
                eprintln!("[gpu] doorbell attached on /dev/virtio-ports/{name}");
                Some(d)
            }
            Err(e) => {
                eprintln!("[gpu] no doorbell on {name} ({e}); waits will run out their slice");
                None
            }
        },
    };

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
        pool_lock: Arc::new(std::sync::Mutex::new(())),
        video_stable: Default::default(),
        poisoned: Default::default(),
    });
    let handled = session::serve(
        control,
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
