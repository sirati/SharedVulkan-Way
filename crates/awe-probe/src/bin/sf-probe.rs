//! `sf-probe`: does Windows tell user mode where the pages are after all?
//!
//! `design.md` §5.1 records, as measured, that it does not — and the API it was
//! measured through, `QueryWorkingSetEx`, indeed does not. This program asks the
//! same question of a different interface: the PFN database, through
//! `NtQuerySystemInformation(SystemSuperfetchInformation)` /
//! `SuperfetchPfnQuery`, which is where Sysinternals' RAMMap gets the
//! frame → process → virtual address table it draws.
//!
//! It is deliberately a separate binary from `awe-probe`. That one is the
//! measurement `measured-gpu-side.md` already carries and it should keep
//! running exactly as documented; this one allocates the same windows through
//! the same allocators and asks a new question about them.
//!
//! What it prints, per run:
//!
//! * whether the interface answers at all on this build, and the physical
//!   memory ranges it reports;
//! * how long a whole-machine dump takes and how many frames it examined;
//! * the owners found in the window's address range — more than one is normal
//!   and is why the owner is elected rather than assumed;
//! * the frame list as a permutation check and as a **run count**, which is the
//!   number §5.7 measures attach cost in;
//! * with `--overwrite`, the same locate repeated **after every byte of the
//!   window has been overwritten**, which is the case `ScanLocator`
//!   structurally cannot handle;
//! * with `--recheck`, the cost of re-asking about only the frames already
//!   known, which is what a periodic re-validation would cost.
//!
//! It also writes `guest-status.txt` in the exact format `arena-host translate`
//! already parses, so the host can check the frame list against the memfd
//! independently — and, because the window is stamped, `arena-host scan` can
//! produce the same table the old way and the two can be compared.
//!
//!   cargo build -p awe-probe --release --target x86_64-pc-windows-gnu

#[cfg(not(windows))]
fn main() {
    eprintln!(
        "sf-probe runs inside the Windows guest. Build it with\n  \
         cargo build -p awe-probe --release --target x86_64-pc-windows-gnu"
    );
    std::process::exit(2);
}

#[cfg(windows)]
fn main() {
    body::run();
}

#[cfg(windows)]
mod body {
    use arena_record as rec;
    use awe_probe::alloc::{Mode, Window};
    use awe_probe::log;
    use awe_probe::rendezvous::write_file;
    use awe_probe::superfetch as sf;
    use std::path::PathBuf;

    pub fn run() {
        let mut workdir = PathBuf::from("Z:\\probe");
        let mut pages: usize = 16384;
        let mut order: Vec<Mode> = vec![Mode::LargePages];
        let mut chunk: usize = 1 << 16;
        let mut overwrite = false;
        let mut recheck = false;
        let mut hold_secs = 0u64;
        let mut keep_secs = 0u64;
        let mut await_host = false;
        let args: Vec<String> = std::env::args().collect();
        let mut i = 1;
        while i < args.len() {
            match args[i].as_str() {
                "--workdir" => {
                    workdir = PathBuf::from(&args[i + 1]);
                    i += 2;
                }
                "--pages" => {
                    pages = args[i + 1].parse().expect("--pages takes a number");
                    i += 2;
                }
                "--mode" => {
                    order = args[i + 1]
                        .split(',')
                        .map(|m| Mode::parse(m).unwrap_or_else(|| panic!("unknown mode {m}")))
                        .collect();
                    i += 2;
                }
                // Identities per `SuperfetchPfnQuery` call. 65536 is 1.5 MiB of
                // buffer; the whole machine in one call would be 82 MiB.
                "--chunk" => {
                    chunk = args[i + 1].parse().expect("--chunk takes a number");
                    i += 2;
                }
                // Destroy the stamps and locate again. This is the whole point:
                // a locator that reads memory cannot survive it.
                "--overwrite" => {
                    overwrite = true;
                    i += 1;
                }
                "--recheck" => {
                    recheck = true;
                    i += 1;
                }
                // Wait for the host to write into the frames we just named, and
                // count how many arrived. The only proof that still works once
                // the stamps have been overwritten.
                "--await-host" => {
                    await_host = true;
                    i += 1;
                }
                // Hold the window open before re-locating, to ask whether the
                // frames are still the same ones after the guest has done some
                // living. R26 with a definite answer instead of a scan.
                "--hold" => {
                    hold_secs = args[i + 1].parse().expect("--hold takes seconds");
                    i += 2;
                }
                // Keep the window alive after the frame list is published, so
                // the host can check it against the memfd while it still
                // exists. Zero by default: a run that nobody is waiting on
                // should exit rather than sit on the guest's memory.
                "--keep" => {
                    keep_secs = args[i + 1].parse().expect("--keep takes seconds");
                    i += 2;
                }
                other => panic!("unknown argument {other}"),
            }
        }

        let run_id = awe_probe::new_run_id();
        log!(
            "sf-probe: run_id=0x{run_id:016x} pages={pages} chunk={chunk} pid={} (0x{:x})",
            std::process::id(),
            std::process::id()
        );

        // ---- 1. the privileges ------------------------------------------------
        // `SeProfileSingleProcessPrivilege` is the one the PFN query wants;
        // `SeLockMemoryPrivilege` is for the allocator, not for the locator, and
        // is asked for only because large pages are the allocator we prefer.
        for p in [
            "SeProfileSingleProcessPrivilege",
            "SeDebugPrivilege",
            "SeLockMemoryPrivilege",
        ] {
            let s = match awe_probe::win::enable_privilege(p) {
                Ok(true) => "enabled".to_string(),
                Ok(false) => "NOT HELD".to_string(),
                Err(e) => format!("error: {e}"),
            };
            log!("{p}: {s}");
        }

        // ---- 2. does the interface answer at all? -----------------------------
        let ranges = match sf::available() {
            Ok(r) => r,
            Err(e) => {
                log!("SuperfetchPfnQuery is not available: {e}");
                log!("VERDICT: this route is closed on this build");
                return;
            }
        };
        let total: usize = ranges.iter().map(|(_, c)| *c).sum();
        log!(
            "physical memory: {} ranges, {} frames ({} MiB)",
            ranges.len(),
            total,
            total * rec::PAGE / (1 << 20)
        );
        for (b, c) in &ranges {
            log!("  range: pfn 0x{b:x} .. 0x{:x} ({c} frames)", b + c);
        }

        // ---- 3. the window ----------------------------------------------------
        let pages = awe_probe::alloc::round_pages(&order, pages);
        let mut window = match Window::acquire(&order, pages) {
            Ok(w) => w,
            Err(e) => {
                log!("NO ALLOCATOR WORKED — nothing to measure ({e})");
                return;
            }
        };
        let mode = window.mode;
        let pages = window.pages;
        let base = window.base as usize;
        log!(
            "window: mode={} base=0x{base:x} pages={pages} ({} MiB)",
            mode.as_str(),
            pages * rec::PAGE / (1 << 20)
        );

        // Stamped so that the *other* locator can be run against the same
        // window and the two answers compared. That comparison is the only way
        // to know this one is right rather than merely self-consistent.
        window.stamp(run_id);

        // ---- 4. locate, by asking ---------------------------------------------
        let Some(mut frames) = report("first locate", base, pages, chunk) else {
            return;
        };

        // ---- 5. the thing the scan cannot do ----------------------------------
        if overwrite {
            log!("overwriting every byte of the window — the stamps are now gone");
            {
                let w = window.as_mut_slice();
                for (i, b) in w.iter_mut().enumerate() {
                    *b = (i as u8) ^ 0xa5;
                }
            }
            match report("after overwrite", base, pages, chunk) {
                Some(f2) => {
                    let d = f2.iter().zip(&frames).filter(|(a, b)| a != b).count();
                    if d == 0 {
                        log!(
                            "SAME frame list after the window was overwritten: {} frames",
                            f2.len()
                        );
                    } else {
                        log!(
                            "re-located after overwrite, {d} of {} frames differ",
                            f2.len()
                        );
                    }
                    // Publish the frame list obtained from a window whose
                    // contents are gone: that is the one whose correctness is
                    // in question, and the host is about to test it.
                    frames = f2;
                }
                None => log!("re-locate after overwrite FAILED"),
            }
        }

        if hold_secs > 0 {
            log!("holding {hold_secs}s before re-locating");
            std::thread::sleep(std::time::Duration::from_secs(hold_secs));
            match report("after hold", base, pages, chunk) {
                Some(f2) if f2 == frames => log!("frame list unchanged after {hold_secs}s"),
                Some(f2) => {
                    let d = f2.iter().zip(&frames).filter(|(a, b)| a != b).count();
                    log!("after {hold_secs}s, {d} frames moved");
                }
                None => log!("re-locate after hold FAILED"),
            }
        }

        // ---- 6. what re-validation costs --------------------------------------
        if recheck {
            match sf::recheck(base, &frames) {
                Ok((ok, ms)) => log!(
                    "recheck: {ok}/{} frames still carry this window's addresses, {ms} ms",
                    frames.len()
                ),
                Err(e) => log!("recheck failed: {e}"),
            }
        }

        // ---- 7. hand the frame list to the host -------------------------------
        //
        // The frame list is a *wire* object before it is a text file: it is
        // `Locator::Pfns` in the `ExtentDesc` a guest publishes on the cold path
        // (docs/design.md §0.14, §4.3). Building it here rather than only writing the
        // text means this probe exercises the same encoding the handshake uses,
        // and the run count printed below is the one the host will reconstruct
        // from the descriptor rather than a second count that happens to agree.
        //
        // Nothing in it is a handle. A frame number is an integer, which is what
        // lets this cross a VM boundary at all (§1a).
        let locator = sf::Located::locator(&frames);
        log!(
            "wire locator: {} run(s) of frames for {pages} pages, {} bytes of ExtentDesc",
            locator.pfn_runs().map_or(0, |r| r.len()),
            // 16 bytes per run, which is the figure §4.3 prices the frame path
            // in and the reason large pages matter to the *handshake* and not
            // only to the attacher.
            locator.pfn_runs().map_or(0, |r| r.len()) * 16
        );
        assert_eq!(
            locator.frame_count(),
            pages as u64,
            "the wire form must describe every page of the window"
        );

        let mut status = String::new();
        status.push_str(&format!("run_id={run_id:016x}\n"));
        status.push_str(&format!("mode={}\n", mode.as_str()));
        status.push_str(&format!("pages={pages}\n"));
        status.push_str(&format!("page_size={}\n", rec::PAGE));
        status.push_str(&format!("base_va={base:016x}\n"));
        status.push_str("locator=superfetch\n");
        for (i, f) in frames.iter().enumerate() {
            status.push_str(&format!("pfn {i} {:x} gpa {:x}\n", f, (*f as u64) << 12));
        }
        write_file(&workdir.join("guest-status.txt"), &status);
        log!("wrote guest-status.txt — the host can now run `arena-host translate`");

        // ---- 8. the decisive check, when the host is playing ------------------
        //
        // `arena-host translate` verifies a frame list by finding our *record*
        // at the offset it computes, which is exactly what an overwritten
        // window no longer has. So the proof has to run the other way: the host
        // writes into the offsets this frame list names and we look for its
        // bytes in our own window. If they arrive at the right page indices,
        // the frame list obtained from a window full of garbage was right.
        if await_host {
            let Some(txt) = awe_probe::rendezvous::wait_for(&workdir.join("host-wrote.txt"), 600)
            else {
                log!("TIMEOUT waiting for host-wrote.txt");
                return;
            };
            let Some(stamp) = awe_probe::rendezvous::parse_hex_field(&txt, "stamp") else {
                log!("host-wrote.txt has no parsable stamp= field — refusing to search for zero");
                return;
            };
            let (mut ok, mut wrong_index) = (0usize, 0usize);
            {
                let w = window.as_mut_slice();
                for p in 0..pages {
                    let page = &w[p * rec::PAGE..(p + 1) * rec::PAGE];
                    if &rec::tag_at(page, rec::OFF_HOST1) == rec::HOST1_TAG
                        && rec::get_u64(page, rec::OFF_HOST1_STAMP) == stamp
                    {
                        if rec::get_u32(page, rec::OFF_HOST1_INDEX) == p as u32 {
                            ok += 1;
                        } else {
                            wrong_index += 1;
                        }
                    }
                }
            }
            log!(
                "host wrote through the superfetch frame list: {ok}/{pages} pages carry its \
                 record at the right index ({wrong_index} at a wrong one)"
            );
            write_file(
                &workdir.join("guest-final.txt"),
                &format!(
                    "run_id={run_id:016x}\nhost1_seen={ok}\nhost1_index_bad={wrong_index}\npages={pages}\n"
                ),
            );
        }

        if keep_secs > 0 {
            log!("holding the window open for {keep_secs}s");
            std::thread::sleep(std::time::Duration::from_secs(keep_secs));
        }
        log!("done");
    }

    /// One dump, reported in the terms the design argues in.
    fn report(what: &str, base: usize, pages: usize, chunk: usize) -> Option<Vec<usize>> {
        let (loc, ms) = match sf::locate_timed(base, pages, chunk) {
            Ok(x) => x,
            Err(e) => {
                log!("{what}: dump failed: {e}");
                return None;
            }
        };
        log!(
            "{what}: examined {} frames in {ms} ms ({:.0} MiB of RAM/s)",
            loc.frames_examined,
            (loc.frames_examined as f64 * rec::PAGE as f64 / (1u64 << 20) as f64)
                / (ms.max(1) as f64 / 1000.0)
        );
        for (o, n) in loc.owners.iter().take(4) {
            log!("  owner 0x{o:x}: {n} pages in our address range");
        }
        log!(
            "  our process id is {} (0x{:x})",
            std::process::id(),
            std::process::id()
        );
        let mut uses: Vec<(u64, usize)> = Vec::new();
        for p in &loc.pages {
            match uses.iter_mut().find(|(u, _)| *u == p.use_description) {
                Some((_, n)) => *n += 1,
                None => uses.push((p.use_description, 1)),
            }
        }
        for (u, n) in &uses {
            log!("  use {} (={u}): {n} pages", sf::use_name(*u));
        }
        match loc.frames(pages) {
            Ok(f) => {
                let runs = sf::Located::runs(&f);
                log!(
                    "  {what}: {} of {pages} pages, {} runs, first pfn 0x{:x}",
                    f.len(),
                    runs.len(),
                    f[0]
                );
                Some(f)
            }
            Err(e) => {
                log!("  {what}: {e}");
                None
            }
        }
    }
}
