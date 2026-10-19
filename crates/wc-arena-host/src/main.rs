//! Find the guest compositor's arena in QEMU's memory backend, prove it is the
//! right memory, and write the plan `wc-proxy` maps.
//!
//! # DO NOT DELETE THIS CRATE. IT HAS BEEN DELETED TWICE.
//!
//! **Nothing in this tree depends on it at compile time.** No crate imports it,
//! no test links it, and until stage 14 made it a member no `--workspace`
//! command even resolved it. Every automated signal a cleanup consults says
//! *dead*. It is not. Its callers are four shell scripts, which invoke it by
//! path, on a different machine, as root, under `systemd-run`:
//!
//! ```text
//!   scripts/wc-titlebar.sh   scripts/wc-desktop.sh
//!   scripts/wc-guest.sh      scripts/sv-firefox-guest.sh
//! ```
//!
//! Those are **every run this project has that puts a real client's pixels in
//! the guest's own RAM**, which is the standing rule docs/waycompose-design.md
//! §12 set. Without this binary all four stop at the locate step and no
//! photograph of a real window over a real arena can be taken again.
//!
//! `crates/arena-host` is **not** a replacement and is the reason this looks
//! redundant. It is a different tool with a different argument set — `scan`,
//! `translate`, `write1`, `gpufill` — and handed this one's arguments it panics
//! with `--fd is required`. That panic is what a desktop shortcut got instead of
//! a window (§28) and what a `wc-titlebar.sh` leg got instead of a capture
//! (§31.9).
//!
//! It was deleted once as "a crate that exists only to hold an absolute path
//! dependency" — true of the *manifest*, and the manifest was a workaround; the
//! **binary** was the product — and restored independently by two stages within
//! days of each other, neither knowing about the other. There is no test that
//! can fail when this file is missing, because the thing that breaks is a run on
//! a Windows VM on another machine. This comment is the whole mitigation.
//!
//! ```text
//!   wc-arena-host --qmp /run/windows-vm/qmp.sock --vm windows-vm \
//!                 --run 0x… --pages 16384 --plan /tmp/wc-arena.plan \
//!                 [--poke OFFSET:TEXT] [--watch OFFSET:SECONDS]
//! ```
//!
//! # Why this program exists rather than a module in `wc-proxy`
//!
//! Two reasons, and the first is the one that matters.
//!
//! **There must be exactly one implementation of guest-physical-address
//! translation across the two projects.** `shared_vulkan`'s `sv-hostd` has it,
//! and its module header is unusually firm about why it is a table lookup of
//! `info mtree -f` and never a formula:
//!
//! > reading the memfd offset as if it were the GPA lands on valid, zeroed,
//! > unrelated memory and reports no error whatsoever — a silent wrong answer,
//! > which is the worst thing this codebase can produce.
//!
//! A copy of that in `waycompose` would be a second thing to keep correct
//! against a machine neither of us controls, and being wrong would look exactly
//! like working. So this binary **depends on** `sv-hostd` and does none of it
//! itself. It is the only crate in this repository that does; stage 14 made it an ordinary
//! workspace member and the `cfg(unix)` gate below is what keeps the Windows
//! cross gate green — see `Cargo.toml`.
//!
//! **Second: privilege.** Reading `/proc/<qemu-pid>/fd` and connecting to
//! QEMU's QMP socket both need root on the VM host. `wc-proxy` should not, and
//! the arena is `MAP_SHARED` memory rather than anything a descriptor has to
//! carry, so the split falls naturally on the same line `sv-hostd` already
//! draws: this side discovers and hands over **a name and a run list**, and the
//! other side opens the name itself. That is `sv-hostd`'s own boundary with its
//! ICD clients, and `wc-proxy` is one more such client.
//!
//! # What crosses the VM boundary because of this program
//!
//! Nothing. The guest said a 64-bit run id and a page count. This program reads
//! the guest's memory from outside, which is what a hypervisor is. No descriptor
//! is passed in either direction and the guest never learns the name of
//! anything.
//!
//! # The two checks that are not optional
//!
//! `--verify` (on by default) re-reads every page of the stitched view and
//! checks that the page at extent offset *n* is the page the guest stamped *n*.
//! It is the assertion that catches a stitcher that mapped the right pages in
//! the wrong order — which reads as "the arena works" on every test that does
//! not look at the index — and it is `sv-hostd qemu --verify`'s check applied to
//! our arena.
//!
//! `--watch` is the live-coherency measurement, and it is the one docs/waycompose-design.md
//! §10.3 could not make about a file: this process writes at an offset while the
//! guest holds a live mapping of the same region, and then waits for the guest
//! to answer at another offset. Both halves have to work for a window to
//! animate, and on virtio-fs neither did.

//! # Why the whole program is `cfg(unix)`
//!
//! It reaches guest RAM through `/proc/<qemu-pid>/fd/<n>` and QEMU's QMP socket,
//! which are not things on the other side of the boundary — `sv_hostd::locate`
//! is Unix-only for exactly that reason. `crates/arena-host` states the same
//! thing in the same shape. It matters here because this crate is a **workspace
//! member** since stage 14 restored it, so `cargo clippy --workspace --target
//! x86_64-pc-windows-gnu` resolves it: without this gate the Windows gate fails
//! on `unresolved import sv_hostd::locate`, which is the *mechanical* form of
//! the objection the original exclusion was written for.

#[cfg(unix)]
mod host {
    use std::io::Write;
    use std::path::{Path, PathBuf};
    use std::time::{Duration, Instant};

    use sv_arena::extent::RunList;
    use sv_hostd::locate;
    use sv_transport::cold::{ExtentDesc, Granularity, Locator};

    const PAGE: u64 = 4096;

    pub fn main() -> std::process::ExitCode {
        match run() {
            Ok(()) => std::process::ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("wc-arena-host: {e}");
                std::process::ExitCode::FAILURE
            }
        }
    }

    struct Args {
        qmp: PathBuf,
        vm: String,
        run_id: u64,
        pages: u64,
        plan: PathBuf,
        verify: bool,
        poke: Option<(u64, String)>,
        watch: Option<(u64, u64)>,
        /// DYNAMIC ARENA. Which region of the arena family this run describes —
        /// the number `wc_proxy::guestram::Plan::id` reads and every
        /// `Descriptor::ArenaSlice { arena, .. }` names.
        ///
        /// It is on the command line rather than derived from the plan's
        /// current contents because the id is **decided by the proxy's
        /// allocator**, before the guest is asked to commit anything: `next_id`
        /// is what `Arena::grow` passes to `RegionSource::provide`, and the
        /// guest registers the region under that same number. A locator that
        /// invented its own id here would write a plan naming a region the
        /// compositor has never heard of, and the symptom is a slice resolved
        /// against a mapping that does not exist.
        region: u32,
        /// Append to the plan instead of truncating it.
        ///
        /// The whole of what makes growth expressible. A plan is a **log of
        /// regions**, and the reason it has to be one is §12.10: the first pool
        /// allocated into a region writes pixels over the stamps the sweep
        /// locates by. Rewriting the file from scratch would mean re-locating
        /// regions whose stamps are gone — which the sweep structurally cannot
        /// do — so a region is written once, when it is new, and never
        /// rewritten.
        append: bool,
        /// A frame list from the guest, in `sv-hostd qemu --frames`' format, to
        /// be used **instead of the content sweep**.
        ///
        /// This is the difference between growth being a feature and being a
        /// three-second stall. The sweep costs what the *guest* costs, not what
        /// the region costs — 2832–3101 ms across a 24x range of arena sizes,
        /// because it is a content search across the whole 13 GiB backing
        /// object — and it is paid on every growth. The frame list is
        /// `SuperfetchPfnQuery`'s answer, and translating it is a table lookup
        /// per page against `info mtree -f`: 0.064 s host-side.
        frames: Option<PathBuf>,
        /// STAGE 10. How many bytes at the end of the arena the guest compositor
        /// reserved for the objects **it** creates for clients — a `wl_keyboard`
        /// keymap first. Copied into the plan so that `wc-proxy`'s pool allocator
        /// leaves them alone.
        ///
        /// This program does not decide the number and does not check it against
        /// anything: it was decided by `wc_handle::arena::reserve_outbound` inside
        /// the guest and printed on the compositor's own report line, exactly as the
        /// run id and the page count were. Carrying it here rather than putting it
        /// on `wc-proxy`'s command line as well keeps it a single value with a
        /// single origin — two spellings of it could differ, and the symptom of
        /// their differing is a pool allocated on top of a live keymap.
        outbound: u64,
    }

    fn usage() -> &'static str {
        "\
    usage: wc-arena-host --qmp <sock> --run <hex> --pages <n> --plan <path> [options]

      --qmp PATH        QEMU's QMP monitor socket (root, on the VM host)
      --vm HINT         process-name hint for finding QEMU (default windows-vm)
      --run 0xHEX       the run id the guest compositor printed
      --pages N         how many 4096-byte pages the arena is
      --plan PATH       where to write the plan wc-proxy reads

      --outbound BYTES  the guest compositor reserved the last BYTES of the arena
                        for objects it creates for clients; record it in the plan
                        so wc-proxy's pool allocator leaves them alone

      --region N        which region of the arena family this is (default 0).
                        The number wc-proxy's allocator chose and the guest
                        registered the region under.
      --append          add this region to an existing plan instead of replacing
                        it. A plan is a log of regions: a region's stamps are
                        overwritten by the first pool allocated into it, so a
                        region is located once, when it is new.
      --frames PATH     a guest frame list (`pfn <index> <frame hex> ...`) to
                        translate instead of sweeping for stamps. 0.064 s rather
                        than ~3 s, because the sweep's cost is the guest's RAM
                        and this is a table lookup per page.

      --no-verify       skip the per-page index check (do not)
      --poke OFF:TEXT   write TEXT at arena offset OFF once the arena is mapped
      --watch OFF:SECS  then wait up to SECS for the guest to change 32 bytes at OFF
    "
    }

    fn parse() -> Result<Args, String> {
        let mut a = Args {
            qmp: PathBuf::new(),
            vm: "windows-vm".into(),
            run_id: 0,
            pages: 0,
            plan: PathBuf::new(),
            verify: true,
            poke: None,
            watch: None,
            outbound: 0,
            region: 0,
            append: false,
            frames: None,
        };
        let mut it = std::env::args().skip(1);
        while let Some(arg) = it.next() {
            let mut next = || it.next().ok_or_else(|| format!("{arg} needs a value"));
            match arg.as_str() {
                "--qmp" => a.qmp = next()?.into(),
                "--vm" => a.vm = next()?,
                "--plan" => a.plan = next()?.into(),
                "--no-verify" => a.verify = false,
                "--run" => {
                    let v = next()?;
                    a.run_id = u64::from_str_radix(v.trim_start_matches("0x"), 16)
                        .map_err(|e| format!("--run {v}: {e}"))?;
                }
                "--pages" => a.pages = next()?.parse().map_err(|e| format!("--pages: {e}"))?,
                "--outbound" => {
                    a.outbound = next()?.parse().map_err(|e| format!("--outbound: {e}"))?
                }
                "--region" => a.region = next()?.parse().map_err(|e| format!("--region: {e}"))?,
                "--append" => a.append = true,
                "--frames" => a.frames = Some(next()?.into()),
                "--poke" => {
                    let v = next()?;
                    let (off, text) = v.split_once(':').ok_or("--poke wants OFFSET:TEXT")?;
                    a.poke = Some((off.parse().map_err(|e| format!("--poke offset: {e}"))?, text.into()));
                }
                "--watch" => {
                    let v = next()?;
                    let (off, secs) = v.split_once(':').ok_or("--watch wants OFFSET:SECONDS")?;
                    a.watch = Some((
                        off.parse().map_err(|e| format!("--watch offset: {e}"))?,
                        secs.parse().map_err(|e| format!("--watch seconds: {e}"))?,
                    ));
                }
                "-h" | "--help" => return Err(String::new()),
                other => return Err(format!("unknown argument {other}")),
            }
        }
        if a.qmp.as_os_str().is_empty() || a.run_id == 0 || a.pages == 0 || a.plan.as_os_str().is_empty()
        {
            return Err("--qmp, --run, --pages and --plan are all required".into());
        }
        Ok(a)
    }

    fn run() -> Result<(), Box<dyn std::error::Error>> {
        let args = match parse() {
            Ok(a) => a,
            Err(m) => {
                if !m.is_empty() {
                    eprintln!("wc-arena-host: {m}");
                }
                eprint!("{}", usage());
                return Err("bad arguments".into());
            }
        };

        // Refuse a duplicate id **before** the locate rather than after it.
        // `Plan::read_all` refuses one too, and that refusal is the one that
        // matters for correctness — two regions with one id would be two
        // mappings a descriptor cannot tell apart. This one is about the clock:
        // a growth that spends its locate and then writes a plan the proxy will
        // not read is a stall the user paid for and got nothing from.
        if args.append && args.plan.exists() {
            let text = std::fs::read_to_string(&args.plan)?;
            let named = |id: u32| {
                text.lines()
                    .map(|l| l.split('#').next().unwrap_or("").trim())
                    .any(|l| {
                        let f: Vec<&str> = l.split_whitespace().collect();
                        f.as_slice() == ["region", &id.to_string()]
                    })
            };
            if named(args.region) {
                return Err(format!(
                    "{} already names region {}: a region id names a mapping and two \
                     mappings cannot share one",
                    args.plan.display(),
                    args.region
                )
                .into());
            }
        }

        // The arena has no control region: `wc-proxy`'s allocator owns the whole
        // thing and there is no slab directory to publish. `Granularity::Small4K`
        // because the guest stamps every 4096-byte page whatever the allocator did.
        //
        // The locator is the one thing that is now a choice, and the choice is
        // the guest's rather than this program's: if the guest could answer
        // `SuperfetchPfnQuery` it sent a frame list, and if it could not it sent
        // nothing and the sweep is what is left. There is no flag that makes
        // this program *prefer* one — a fallback that could be selected by
        // configuration is a fallback that gets selected by accident, which is
        // docs/waycompose-design.md §5.1's objection to fallbacks generally.
        let frames = match &args.frames {
            None => None,
            Some(path) => {
                let text = std::fs::read_to_string(path)
                    .map_err(|e| format!("--frames {}: {e}", path.display()))?;
                let frames = sv_hostd::locate::parse_frame_list(&text)
                    .map_err(|e| format!("--frames {}: {e}", path.display()))?;
                if frames.len() as u64 != args.pages {
                    return Err(format!(
                        "--frames {} has {} frames and --pages says {}: a frame list that \
                         does not describe the whole region would locate part of it and \
                         leave the rest reading as somebody else's memory",
                        path.display(),
                        frames.len(),
                        args.pages
                    )
                    .into());
                }
                Some(frames)
            }
        };
        let (locator, choice) = match &frames {
            Some(f) => (Locator::from_frames(f), sv_hostd::LocatorChoice::Pfns),
            None => (Locator::Scan, sv_hostd::LocatorChoice::Scan),
        };
        let desc = ExtentDesc {
            run_id: args.run_id,
            granularity: Granularity::Small4K,
            control_pages: 0,
            arena_pages: args.pages,
            locator,
        };
        let len = args.pages * PAGE;

        let t0 = Instant::now();
        let (hostd, regions, outcome) =
            sv_hostd::Hostd::qemu_located(&args.qmp, &args.vm, &desc, 1, choice)?;
        let located = t0.elapsed();

        println!(
            "guest RAM in the flat view: {} MiB in {} alias(es)",
            regions.total_ram() >> 20,
            regions.aliases().len()
        );
        // One line, in one shape, whichever locator ran. `wc-session`'s `up`
        // greps this and so does every transcript in docs/; a second spelling of
        // "how many pages did we find" would be a second thing to grep for.
        if let Some(report) = &outcome.scan {
            println!(
                "located {} of {} pages in {:.2?} by sweep (swept {} MiB; {} other-run \
                 stamps, {} outside guest RAM)",
                report.pages_found,
                report.pages_expected,
                located,
                report.bytes_swept >> 20,
                report.other_runs,
                report.outside_ram
            );
        }
        if let Some(report) = &outcome.pfns {
            // `bytes_swept` has no counterpart here and that absence *is* the
            // result: nothing was read. The cost is one table lookup per page
            // against `info mtree -f`, which is why this is 0.064 s where the
            // sweep is ~3 s, and why the number does not move with the guest's
            // RAM.
            println!(
                "located {} of {} pages in {:.2?} by frame list (swept 0 MiB; {} frame \
                 run(s) became {} file run(s), first frame 0x{:x})",
                report.pages,
                args.pages,
                located,
                report.frame_runs,
                report.file_runs,
                report.first_frame
            );
        }
        println!(
            "coalesced into {} run(s) covering {} MiB — {:.1} pages per run",
            hostd.runs.len(),
            hostd.runs.bytes() >> 20,
            args.pages as f64 / hostd.runs.len().max(1) as f64
        );
        if let Some(first) = hostd.runs.runs().first() {
            println!(
                "first run: file offset 0x{:012x} (gpa 0x{:012x}), {} KiB",
                first.file_offset,
                regions.offset_to_gpa(first.file_offset).unwrap_or(0),
                first.len >> 10
            );
        }

        // Borrow the daemon's descriptor as a `File` without taking it: `Hostd` owns
        // it and must close it exactly once.
        let file = {
            use std::os::fd::{AsFd, AsRawFd, FromRawFd};
            let raw = hostd.fd.as_fd().as_raw_fd();
            // SAFETY: a `File` that is never dropped, over a descriptor `hostd`
            // owns for longer than this borrow.
            std::mem::ManuallyDrop::new(unsafe { std::fs::File::from_raw_fd(raw) })
        };
        let t1 = Instant::now();
        let view = locate::attach(&file, &hostd.runs)?;
        println!(
            "stitched {} MiB into one view with {} mmap(MAP_FIXED) call(s) in {:.2?}",
            view.len() >> 20,
            view.runs(),
            t1.elapsed()
        );

        if args.verify {
            // **The outbound window is exempt, and by construction rather than
            // by tolerance.** STAGE 71.
            //
            // Its stamps are the compositor's to overwrite: the window is where
            // gifts are written, and since §53 its first `CONTROL_BYTES` are the
            // growth channel, reserved and initialised *before any gift* while
            // the window is still untouched. So on any run with both a guest-RAM
            // arena and an outbound window, some leading pages of that window
            // have lost their stamps before the host ever looks — which is the
            // design working, not a fault.
            //
            // Measured rather than assumed. A Linux-guest run reported
            // `verify: 16316/16384 pages` and refused to write a plan; the wrong
            // pages began at index 16128, which was exactly the offset the
            // compositor had printed for its outbound window, and there were
            // **68** of them. `grow::CONTROL_BYTES` is 272 KiB, which is 68
            // pages of 4096. The first two carried the channel's header and the
            // rest read as zeroes, which is an initialised ring and not, as
            // §70.4 guessed, a page that had been dropped and re-faulted. That
            // guess is withdrawn: nothing was re-faulted and no frame changed.
            //
            // The data region is still verified in full, which is the half that
            // matters — a mislocated *data* page is somebody's pixels landing in
            // the wrong memory, and that check is unchanged.
            let outbound_pages = args.outbound / PAGE;
            let checked = args.pages.saturating_sub(outbound_pages);
            if outbound_pages > 0 {
                println!(
                    "verify: skipping the last {outbound_pages} page(s) -- the outbound \
                     window is where the compositor writes, and its stamps are its own to spend"
                );
            }
            let mut wrong = 0usize;
            let mut header = [0u8; 20];
            for i in 0..checked {
                view.read(i * PAGE, &mut header).ok_or("short read of the stitched view")?;
                let magic_ok = header[0..8] == wc_handle::arena::STAMP_MAGIC;
                let run_ok = u64::from_le_bytes(header[8..16].try_into().unwrap()) == args.run_id;
                let index_ok = u32::from_le_bytes(header[16..20].try_into().unwrap()) == i as u32;
                if !(magic_ok && run_ok && index_ok) {
                    wrong += 1;
                }
            }
            println!(
                "verify: {}/{} pages of the stitched view carry their own arena index",
                checked as usize - wrong,
                checked
            );
            if wrong > 0 {
                return Err(format!("{wrong} page(s) are not where the arena says they are").into());
            }
        }

        if args.outbound >= len {
            return Err(format!(
                "--outbound {} leaves nothing of a {len}-byte arena for the proxy's pools",
                args.outbound
            )
            .into());
        }
        write_plan(
            &args.plan,
            &hostd.object,
            len,
            args.run_id,
            args.outbound,
            &hostd.runs,
            args.region,
            args.append,
        )?;
        println!(
            "plan: {} region {} ({} run(s), object {}){}",
            args.plan.display(),
            args.region,
            hostd.runs.len(),
            hostd.object,
            if args.append { " [appended]" } else { "" }
        );
        if args.outbound > 0 {
            println!(
                "outbound: the last {} bytes (offset {}) are the guest compositor's",
                args.outbound,
                len - args.outbound
            );
        }

        // ------------------------------------------------------------------ live
        // The measurement docs/waycompose-design.md §10.3 could not make about a file. Both
        // sides hold a live mapping of the same region; this one writes, the other
        // answers. The numbers printed here are what block 10 asserts on.
        if let Some((off, text)) = &args.poke {
            view.write(*off, text.as_bytes()).ok_or("--poke offset is outside the arena")?;
            println!("COH-HOST-WROTE: offset={off} bytes={} text={text:?}", text.len());
        }
        if let Some((off, secs)) = &args.watch {
            let mut before = [0u8; 32];
            view.read(*off, &mut before).ok_or("--watch offset is outside the arena")?;
            let deadline = Instant::now() + Duration::from_secs(*secs);
            let mut now = [0u8; 32];
            let started = Instant::now();
            loop {
                view.read(*off, &mut now).ok_or("--watch offset is outside the arena")?;
                if now != before {
                    println!(
                        "COH-HOST-SAW: offset={off} after_ms={} bytes={:?}",
                        started.elapsed().as_millis(),
                        String::from_utf8_lossy(&now).trim_end_matches('\0').trim_end_matches(|c: char| c as u8 == 0xAB)
                    );
                    break;
                }
                if Instant::now() >= deadline {
                    println!("COH-HOST-SAW: offset={off} after_ms=never nothing changed in {secs}s");
                    break;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
        }
        Ok(())
    }

    /// The plan, in the format `wc_proxy::guestram::Plan` reads.
    ///
    /// # Why appending is the only way to add a region, and not a mode of
    /// convenience
    ///
    /// The stamps a region is located by are **destroyed by the first pool
    /// allocated into it** (§12.10): the host copies a client's pixels over
    /// them within milliseconds of the region joining the arena. A locator that
    /// rewrote this file from scratch would therefore have to re-locate every
    /// region already in it, and for the regions in use there is nothing left
    /// to locate — the sweep would report `located 0 of 4096 pages` for memory
    /// that is working perfectly.
    ///
    /// So the file is a log. Each region is written exactly once, at the one
    /// moment it is findable, and `Plan::read_all` reads back everything ever
    /// appended. That the `--frames` path *can* re-locate an overwritten region
    /// does not change this: a log is still the cheaper and more honest
    /// artefact, and it is the one a human reads after a frozen window.
    ///
    /// A `region <id>` line is written on **every** path, including the first,
    /// once anything might append. The one exception is the pre-growth
    /// spelling — no `--region`, no `--append` — which still writes exactly the
    /// bytes it always did, because `Plan::read_all` reads a file with no
    /// `region` line as one region numbered zero and every existing harness
    /// compares that text.
    #[allow(clippy::too_many_arguments)]
    fn write_plan(
        path: &Path,
        object: &str,
        len: u64,
        run_id: u64,
        outbound: u64,
        runs: &RunList,
        region: u32,
        append: bool,
    ) -> std::io::Result<()> {
        let mut f = if append {
            std::fs::OpenOptions::new().create(true).append(true).open(path)?
        } else {
            std::fs::File::create(path)?
        };
        writeln!(f, "# wc-arena-host plan — {} run(s), {len} bytes", runs.len())?;
        if append || region != 0 {
            // The id names the mapping a `Descriptor::ArenaSlice { arena, .. }`
            // resolves against, so it has to be stated rather than counted:
            // this program does not know how many regions the proxy has, only
            // which one it was asked to describe.
            writeln!(f, "region {region}")?;
        }
        writeln!(f, "# object is QEMU's memory backend; wc-proxy opens it itself.")?;
        writeln!(f, "object {object}")?;
        writeln!(f, "len {len}")?;
        writeln!(f, "run 0x{run_id:016x}")?;
        if outbound > 0 {
            // The guest compositor's own window, as `offset len`. Always the tail,
            // which is what `wc_handle::arena::reserve_outbound` produces and what
            // `wc_proxy::guestram::Plan::read` refuses anything else than.
            writeln!(f, "outbound {} {outbound}", len - outbound)?;
        }
        for r in runs.runs() {
            writeln!(f, "r {} 0x{:x} {}", r.extent_offset, r.file_offset, r.len)?;
        }
        f.flush()
    }

    #[cfg(test)]
    mod tests {
        use super::write_plan;
        use sv_arena::extent::{Run, RunList};

        /// DYNAMIC ARENA. The append is the whole of growth's host half, and
        /// what it has to produce is a file whose *second* record does not
        /// disturb the first — because the first describes memory that is full
        /// of a client's pixels and can never be located again.
        ///
        /// The assertions are on the text rather than on a parse, deliberately:
        /// `wc_proxy::guestram::Plan` is the reader and this crate cannot depend
        /// on it (that dependency is what makes the Windows gate resolve
        /// `sv_hostd`, see the crate header). What is checked here is the
        /// property the reader relies on — one `region` line per record, the
        /// records in the order they were appended, and the first record's run
        /// list still byte-for-byte where it was.
        #[test]
        fn appending_a_region_leaves_the_first_one_alone() {
            let dir = std::env::temp_dir().join(format!("wc-arena-host-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            let path = dir.join("arena.plan");

            let first = RunList::new(vec![Run { extent_offset: 0, file_offset: 0x1000, len: 8192 }]);
            write_plan(&path, "/proc/9/fd/3", 8192, 0xaaa, 4096, &first, 0, false).unwrap();
            let after_first = std::fs::read_to_string(&path).unwrap();
            // The pre-growth spelling is unchanged: no `--region`, no
            // `--append`, and therefore no `region` line at all. Every plan ever
            // written still reads as region 0.
            assert!(!after_first.contains("region 0"), "{after_first}");
            assert!(after_first.contains("outbound 4096 4096"), "{after_first}");

            let second =
                RunList::new(vec![Run { extent_offset: 0, file_offset: 0x40000, len: 4096 }]);
            write_plan(&path, "/proc/9/fd/3", 4096, 0xbbb, 0, &second, 1, true).unwrap();
            let text = std::fs::read_to_string(&path).unwrap();

            // The first record survived intact, which is the point.
            assert!(text.starts_with(&after_first), "the append rewrote the first record:\n{text}");
            // And the second is a record of its own, named.
            assert!(text.contains("\nregion 1\n"), "{text}");
            assert!(text.contains("run 0x0000000000000bbb"), "{text}");
            assert!(text.contains("r 0 0x40000 4096"), "{text}");
            // A grown region has no outbound window: the compositor declares
            // one only in the region it committed at startup, and a second
            // `outbound` line would take bytes out of the new region's free
            // list that nothing had claimed.
            assert_eq!(text.matches("outbound ").count(), 1, "{text}");

            let _ = std::fs::remove_dir_all(&dir);
        }

        /// THE TEST THIS CRATE EXISTS FOR, apart from its binary.
        ///
        /// `wc_handle::arena` spells the stamp layout out for itself rather than
        /// depending on `arena_record`, because depending on it would make the
        /// *Windows* build of waycompose need a sibling repository checked out. Its
        /// doc comment argues that the duplication is safe because a disagreement
        /// is loud rather than silent — and then says the real reason, which is
        /// this: the two definitions are compared, here, in the one binary that
        /// links both projects.
        ///
        /// If this fails, the guest is stamping pages the host's sweep will not
        /// recognise, and the symptom is "located 0 of 16384 pages".
        #[test]
        fn the_two_projects_agree_on_the_stamp() {
            assert_eq!(wc_handle::arena::STAMP_MAGIC, *arena_record::MAGIC);
            assert_eq!(wc_handle::arena::STAMP_OFF_MAGIC, arena_record::OFF_MAGIC);
            assert_eq!(wc_handle::arena::STAMP_OFF_RUN_ID, arena_record::OFF_RUN_ID);
            assert_eq!(wc_handle::arena::STAMP_OFF_PAGE_INDEX, arena_record::OFF_PAGE_INDEX);
            assert_eq!(wc_handle::arena::STAMP_FILL, arena_record::FILL);
            assert_eq!(wc_handle::arena::REGION_PAGE, arena_record::PAGE);
        }
    }
}

#[cfg(unix)]
mod agent;
#[cfg(unix)]
mod ram_peer;
#[cfg(unix)]
mod sessiond;
#[cfg(unix)]
mod sessiond_ivshmem;

/// `wc-arena-host broker-read PATH [BYTES]`: the first BYTES (4096) of a
/// file, or of region `broker:ID` from the broker (§111.10). What
/// `wc-session up` reads the ivshmem header with when its user may not open
/// the BAR file itself.
#[cfg(unix)]
fn broker_read(argv: &[String]) -> std::process::ExitCode {
    use std::io::{Read, Write};
    let Some(path) = argv.first() else {
        eprintln!("usage: wc-arena-host broker-read PATH|broker:ID [BYTES]");
        return std::process::ExitCode::from(2);
    };
    let n: u64 = argv.get(1).and_then(|s| s.parse().ok()).unwrap_or(4096).min(1 << 20);
    let r = wc_broker::client::open_path(std::path::Path::new(path)).and_then(|f| {
        let mut buf = Vec::new();
        f.take(n).read_to_end(&mut buf)?;
        std::io::stdout().write_all(&buf)
    });
    match r {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("wc-arena-host broker-read {path}: {e}");
            std::process::ExitCode::from(1)
        }
    }
}

#[cfg(unix)]
fn main() -> std::process::ExitCode {
    // §103.15: the macOS guest's session service.
    let argv: Vec<String> = std::env::args().collect();
    // §111.10: the mapped user's agent, and the header read through the broker.
    match argv.get(1).map(String::as_str) {
        Some("agent") => return agent::main(&argv[2..]),
        Some("broker-read") => return broker_read(&argv[2..]),
        _ => {}
    }
    // §108: the host's half of a region table (the test VM's instrument).
    if argv.get(1).map(String::as_str) == Some("ram-peer") {
        return ram_peer::main(&argv[2..]);
    }
    // §105: one service, two substrates. `--ivshmem` is a guest with a
    // shared-memory device (Windows); without it, guest RAM swept (macOS).
    if matches!(argv.get(1).map(String::as_str), Some("mac-sessiond" | "sessiond")) {
        if argv.iter().any(|a| a == "--ivshmem" || a == "--arena-file") {
            return sessiond_ivshmem::main(&argv[2..]);
        }
        return sessiond::main(&argv[2..]);
    }
    host::main()
}

#[cfg(not(unix))]
fn main() -> std::process::ExitCode {
    eprintln!("wc-arena-host runs on the VM host, which is Linux here.");
    std::process::ExitCode::from(2)
}
