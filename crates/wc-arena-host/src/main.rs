//! Find the guest compositor's arena in QEMU's memory backend, prove it is the
//! right memory, and write the plan `wc-proxy` maps.
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
//! itself. It is the only crate in this repository that does, it is deliberately
//! not a workspace member (see `Cargo.toml`), and the split keeps the sibling
//! repository out of the Windows cross gate entirely.
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
//! `--watch` is the live-coherency measurement, and it is the one docs/design.md
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

        // The arena has no control region: `wc-proxy`'s allocator owns the whole
        // thing and there is no slab directory to publish. `Granularity::Small4K`
        // because the guest stamps every 4096-byte page whatever the allocator did,
        // and `Locator::Scan` because content is the only locator available — see
        // `wc_handle::arena::attach_guest_ram`.
        let desc = ExtentDesc {
            run_id: args.run_id,
            granularity: Granularity::Small4K,
            control_pages: 0,
            arena_pages: args.pages,
            locator: Locator::Scan,
        };
        let len = args.pages * PAGE;

        let t0 = Instant::now();
        let (hostd, regions, report) = sv_hostd::Hostd::qemu(&args.qmp, &args.vm, &desc, 1)?;
        let located = t0.elapsed();

        println!(
            "guest RAM in the flat view: {} MiB in {} alias(es)",
            regions.total_ram() >> 20,
            regions.aliases().len()
        );
        println!(
            "located {}/{} pages in {:.2?} (swept {} MiB; {} other-run stamps, {} outside guest RAM)",
            report.pages_found,
            report.pages_expected,
            located,
            report.bytes_swept >> 20,
            report.other_runs,
            report.outside_ram
        );
        println!(
            "coalesced into {} run(s) covering {} MiB — {:.1} pages per run",
            hostd.runs.len(),
            hostd.runs.bytes() >> 20,
            report.pages_found as f64 / hostd.runs.len().max(1) as f64
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
            let mut wrong = 0usize;
            let mut header = [0u8; 20];
            for i in 0..args.pages {
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
                args.pages as usize - wrong,
                args.pages
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
        write_plan(&args.plan, &hostd.object, len, args.run_id, args.outbound, &hostd.runs)?;
        println!("plan: {} ({} run(s), object {})", args.plan.display(), hostd.runs.len(), hostd.object);
        if args.outbound > 0 {
            println!(
                "outbound: the last {} bytes (offset {}) are the guest compositor's",
                args.outbound,
                len - args.outbound
            );
        }

        // ------------------------------------------------------------------ live
        // The measurement docs/design.md §10.3 could not make about a file. Both
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
    fn write_plan(
        path: &Path,
        object: &str,
        len: u64,
        run_id: u64,
        outbound: u64,
        runs: &RunList,
    ) -> std::io::Result<()> {
        let mut f = std::fs::File::create(path)?;
        writeln!(f, "# wc-arena-host plan — {} run(s), {len} bytes", runs.len())?;
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
fn main() -> std::process::ExitCode {
    host::main()
}

#[cfg(not(unix))]
fn main() -> std::process::ExitCode {
    eprintln!("wc-arena-host runs on the VM host, which is Linux here.");
    std::process::ExitCode::from(2)
}
