//! What a pool costs the **receiving** side, per transport, measured through
//! the production code path.
//!
//! ```text
//!   wc-poolcost --iters N --size BYTES --files 'Z:\wc\pools' \
//!               --arena 'Z:\wc\arena':SIZE --sections
//!   wc-poolcost --prepare <dir> --iters N --size BYTES
//! ```
//!
//! # Why this exists
//!
//! docs/design.md §9.11 item 2 asks what `SharedFile` costs per pool across the
//! hypervisor boundary against `ArenaSlice`'s arithmetic, and calls it "the
//! measurement that turns a preference into a decision". §9.5 had already
//! observed that on one machine the two are indistinguishable — both end in a
//! `MapViewOfFile` of a local page cache — so the number that decides anything
//! can only be taken with a real filesystem daemon in the path.
//!
//! # Why it is a binary in `wc-handle` and not a C probe
//!
//! Because a C imitation of `Handle::map` would measure a C imitation. The
//! whole point is the cost of the *shipping* path: `Descriptor::SharedFile` →
//! `platform::map_file` → `CreateFileW` + `CreateFileMappingW` +
//! `MapViewOfFile`, against `Descriptor::ArenaSlice` → `arena::window` → a
//! registry lookup and checked arithmetic. Those are the two things being
//! compared and they both live here, so the probe lives here too and is linted
//! with everything else.
//!
//! # What is timed, and what is deliberately timed separately
//!
//! Three phases per pool, because they answer different questions and the
//! second one is where a paravirtual filesystem hides:
//!
//!  * **map** — reaching the object. `SharedFile` opens a path; `ArenaSlice`
//!    does arithmetic on a region attached once, before the loop.
//!  * **touch** — one byte read per 4096-byte page. On a local mapping this is
//!    a soft fault; over virtio-fs it is a FUSE read per page, and it is the
//!    term that a "how long does the open take" measurement misses entirely.
//!  * **drop** — unmapping. Free for an arena slice by construction: an arena
//!    window borrows the region and unmaps nothing (`MappingInner::Window`).
//!
//! # Negative capability
//!
//! The comparison can come out either way and the harness asserts a *ratio*
//! rather than a direction: §9.5 measured these transports the same on one
//! machine, and if they are the same here too this has to be able to say so.
//!
//! `--sections` is the control, and what it controls for is attribution rather
//! than speed. A named section touches no filesystem, so its number is the same
//! whichever directory the `--files` run was pointed at; if it is not, the two
//! runs are not comparable and nothing below them means anything. The second
//! control is the caller's: run `--files` once against the shared mount and once
//! against a local one. A slow `CreateFileW` is only evidence about the mount if
//! the same call against NTFS is fast, and without that pair the whole
//! measurement would be equally consistent with "Windows is slow".

fn main() -> std::process::ExitCode {
    imp::main()
}

#[cfg(windows)]
mod imp {
    use std::time::{Duration, Instant};

    use wc_handle::{Descriptor, Handle, arena};

    struct Args {
        iters: usize,
        size: u64,
        files: Option<String>,
        arena: Option<(String, u64)>,
        /// STAGE 7. Provide an arena out of this process's own guest RAM and
        /// measure pools cut from it. No path, because there is nothing to
        /// name — see `wc_handle::arena::attach_guest_ram`.
        guest_arena: Option<u64>,
        /// `offset:seconds` — after the arena exists, hold a live mapping and
        /// watch these 32 bytes for the host's write. The live half of the
        /// coherency measurement docs/design.md §10.3 made against a file.
        watch: Option<(u64, u64)>,
        /// `offset:text` — write this once the watch fires, so the host can
        /// measure the other direction while this process still holds the
        /// mapping.
        reply: Option<(u64, String)>,
        sections: bool,
        prepare: Option<String>,
        /// STAGE 10. Measure the **sending** side instead of the receiving one:
        /// what it costs this process to make an object of `--size` *for a peer*
        /// and fill it, which is what a `wl_keyboard.keymap` is.
        ///
        /// `Handle::create_shared` picks the transport from what the process was
        /// set up with, so this runs it three times over three setups rather
        /// than taking a mode argument — a section, a published directory, and
        /// an outbound arena window. The directory is `--files`, reused, because
        /// where objects go is the same question in both directions.
        mint: bool,
        /// `PATH:BYTES` — a file to provide as a writable arena for `--mint`.
        /// Under Wine there is no hypervisor and therefore no guest RAM; a file
        /// both sides open is the substrate that leg has.
        mint_arena: Option<(String, u64)>,
    }

    fn usage() -> &'static str {
        "\
usage: wc-poolcost [options]

  --iters N              how many pools to measure (default 32)
  --size BYTES           how big each pool is (default 1048576)

  --files DIR            SharedFile: DIR holds pc-000000.bin .. pc-<N>.bin,
                         created by whoever owns the filesystem
  --arena PATH:BYTES     ArenaSlice: attach PATH once, then cut --iters slices
  --arena-guest-ram N    ArenaSlice over a region this process commits and pins
                         inside the VM, stamped so the host can find it. Prints
                         the run id the host's locator needs.
  --watch OFF:SECS       hold the guest arena mapped and wait for the host to
                         change 32 bytes at OFF (live coherency, this direction)
  --reply OFF:TEXT       and answer at OFF once it does (the other direction)
  --sections             also measure a named section, as the control
  --prepare DIR          create the pool files in DIR and exit

  --mint                 measure the *sending* side instead: what it costs to
                         make an object of --size for a peer and fill it, which
                         is what a wl_keyboard.keymap is. Uses --files as the
                         published directory and --mint-arena as the region.
  --mint-arena PATH:BYTES  provide PATH as a writable arena for --mint, and cut
                         the objects out of a window at the end of it
"
    }

    fn parse() -> Result<Args, String> {
        let mut a = Args {
            iters: 32,
            size: 1 << 20,
            files: None,
            arena: None,
            guest_arena: None,
            watch: None,
            reply: None,
            sections: false,
            prepare: None,
            mint: false,
            mint_arena: None,
        };
        let mut it = std::env::args().skip(1);
        while let Some(arg) = it.next() {
            let mut next = || it.next().ok_or_else(|| format!("{arg} needs a value"));
            match arg.as_str() {
                "--iters" => a.iters = next()?.parse().map_err(|e| format!("--iters: {e}"))?,
                "--size" => a.size = next()?.parse().map_err(|e| format!("--size: {e}"))?,
                "--files" => a.files = Some(next()?),
                "--sections" => a.sections = true,
                "--prepare" => a.prepare = Some(next()?),
                "--mint" => a.mint = true,
                "--mint-arena" => {
                    let v = next()?;
                    let (path, size) = v
                        .rsplit_once(':')
                        .ok_or_else(|| "--mint-arena wants PATH:BYTES".to_string())?;
                    a.mint_arena = Some((
                        path.to_owned(),
                        size.parse().map_err(|e| format!("--mint-arena size: {e}"))?,
                    ));
                }
                "--arena" => {
                    let v = next()?;
                    // Split at the *last* colon: a Windows path starts with one.
                    let (path, size) = v
                        .rsplit_once(':')
                        .ok_or_else(|| "--arena wants PATH:BYTES".to_string())?;
                    a.arena = Some((
                        path.to_owned(),
                        size.parse().map_err(|e| format!("--arena size: {e}"))?,
                    ));
                }
                "--arena-guest-ram" => {
                    a.guest_arena =
                        Some(next()?.parse().map_err(|e| format!("--arena-guest-ram: {e}"))?)
                }
                "--watch" => {
                    let v = next()?;
                    let (off, secs) = v.split_once(':').ok_or("--watch wants OFFSET:SECONDS")?;
                    a.watch = Some((
                        off.parse().map_err(|e| format!("--watch offset: {e}"))?,
                        secs.parse().map_err(|e| format!("--watch seconds: {e}"))?,
                    ));
                }
                "--reply" => {
                    let v = next()?;
                    let (off, text) = v.split_once(':').ok_or("--reply wants OFFSET:TEXT")?;
                    a.reply = Some((
                        off.parse().map_err(|e| format!("--reply offset: {e}"))?,
                        text.to_owned(),
                    ));
                }
                "-h" | "--help" => return Err(String::new()),
                other => return Err(format!("unknown argument {other}")),
            }
        }
        Ok(a)
    }

    fn pool_path(dir: &str, i: usize) -> String {
        let sep = if dir.ends_with('\\') || dir.ends_with('/') {
            ""
        } else {
            "\\"
        };
        format!("{dir}{sep}pc-{i:06}.bin")
    }

    /// Mean and median of a sample, in microseconds.
    fn stats(mut xs: Vec<Duration>) -> (f64, f64) {
        if xs.is_empty() {
            return (0.0, 0.0);
        }
        let mean = xs.iter().map(|d| d.as_secs_f64()).sum::<f64>() / xs.len() as f64 * 1e6;
        xs.sort();
        let median = xs[xs.len() / 2].as_secs_f64() * 1e6;
        (mean, median)
    }

    /// The receiving side's three phases, which is what this program was for
    /// until stage 10.
    fn report(label: &str, map: Vec<Duration>, touch: Vec<Duration>, drop_: Vec<Duration>) {
        report_as(label, ["map", "touch", "drop"], "pool", map, touch, drop_);
    }

    /// The same table with the phases named, because the sending side's three
    /// are not `map`/`touch`/`drop` and a table that said they were would be
    /// wrong in the one way a reader could not check.
    fn report_as(
        label: &str,
        phases: [&str; 3],
        unit: &str,
        a: Vec<Duration>,
        b: Vec<Duration>,
        c: Vec<Duration>,
    ) {
        let (am, ad) = stats(a);
        let (bm, _) = stats(b);
        let (cm, _) = stats(c);
        println!(
            "  {label:<18} {:<8}{am:9.1} us (median {ad:8.1})  {:<6}{bm:9.1} us  \
             {:<8}{cm:7.1} us  total {:9.1} us/{unit}",
            phases[0],
            phases[1],
            phases[2],
            am + bm + cm
        );
    }

    /// One byte per 4096-byte page, summed so nothing can be optimised out.
    fn touch(bytes: &[u8]) -> u64 {
        let mut acc = 0u64;
        let mut i = 0;
        while i < bytes.len() {
            acc += u64::from(std::hint::black_box(bytes[i]));
            i += 4096;
        }
        acc
    }

    pub fn main() -> std::process::ExitCode {
        let args = match parse() {
            Ok(a) => a,
            Err(msg) => {
                if !msg.is_empty() {
                    eprintln!("wc-poolcost: {msg}");
                }
                eprint!("{}", usage());
                return std::process::ExitCode::from(2);
            }
        };

        if let Some(dir) = &args.prepare {
            for i in 0..args.iters {
                let p = pool_path(dir, i);
                let f = match std::fs::File::create(&p) {
                    Ok(f) => f,
                    Err(e) => {
                        eprintln!("wc-poolcost: create {p}: {e}");
                        return std::process::ExitCode::FAILURE;
                    }
                };
                if let Err(e) = f.set_len(args.size) {
                    eprintln!("wc-poolcost: extend {p}: {e}");
                    return std::process::ExitCode::FAILURE;
                }
            }
            println!(
                "prepared {} files of {} bytes in {dir}",
                args.iters, args.size
            );
            return std::process::ExitCode::SUCCESS;
        }

        // ------------------------------------------------------------ 10 ---
        // The sending side. Everything below this block measures what a
        // *receiver* pays to reach a pool a peer made; this measures what a
        // **sender** pays to make an object a peer will open, which is the
        // direction stage 6 opened and stage 10 gave an arena to.
        //
        // Three phases, chosen to mirror the receiver's three so the two tables
        // read the same way:
        //
        //  * **create** — `Handle::create_shared`, whatever it decides that is.
        //  * **fill** — `write_at` of the whole object, once. A keymap is
        //    written exactly once and never touched again.
        //  * **release** — dropping the handle. For a published file that is an
        //    unlink; for a gift it is a free-list insert.
        //
        // Deliberately *not* included: the receiver's cost, which is the rest
        // of this program, and the descriptor's 30-odd bytes on the wire, which
        // are the same for both.
        if args.mint {
            println!(
                "wc-poolcost: minting {} objects of {} bytes each, for a peer",
                args.iters, args.size
            );
            let mut bad = 0;

            let run = |label: &str| -> usize {
                let mut bad = 0usize;
                let (mut c, mut f, mut r) = (Vec::new(), Vec::new(), Vec::new());
                let filler = vec![0x5au8; args.size as usize];
                for _ in 0..args.iters {
                    let t0 = Instant::now();
                    let h = match Handle::create_shared(args.size) {
                        Ok(h) => h,
                        Err(e) => {
                            eprintln!("  {label}: create failed: {e}");
                            bad += 1;
                            break;
                        }
                    };
                    c.push(t0.elapsed());
                    let t1 = Instant::now();
                    if let Err(e) = h.write_at(0, &filler) {
                        eprintln!("  {label}: fill failed: {e}");
                        bad += 1;
                        break;
                    }
                    f.push(t1.elapsed());
                    let t2 = Instant::now();
                    std::mem::drop(h);
                    r.push(t2.elapsed());
                }
                report_as(label, ["create", "fill", "release"], "object", c, f, r);
                bad
            };

            // A section, first, because it is the answer with no filesystem and
            // no arena and is therefore the floor for the other two.
            bad += run("mint section");
            if let Some(dir) = &args.files {
                let sep = if dir.ends_with('\\') || dir.ends_with('/') { "" } else { "\\" };
                if let Err(e) = wc_handle::publish::set(dir, &format!("{dir}{sep}")) {
                    eprintln!("  mint shared-file: --files {dir}: {e}");
                    bad += 1;
                } else {
                    bad += run("mint shared-file");
                }
            }
            // Last, because an outbound window wins over a published directory
            // in `create_shared` and there is no way to give it back.
            if let Some((path, size)) = &args.mint_arena {
                match arena::provide_file(9, path, *size)
                    .and_then(|()| arena::reserve_outbound(9, *size / 2))
                {
                    Ok(w) => {
                        println!("  arena 9 outbound window: {}+{}", w.offset, w.len);
                        bad += run("mint arena-slice");
                    }
                    Err(e) => {
                        eprintln!("  mint arena-slice: {path}: {e}");
                        bad += 1;
                    }
                }
            }
            return if bad == 0 {
                println!("RESULT: minted");
                std::process::ExitCode::SUCCESS
            } else {
                println!("RESULT: {bad} measurement(s) failed");
                std::process::ExitCode::FAILURE
            };
        }

        println!(
            "wc-poolcost: {} pools of {} bytes each",
            args.iters, args.size
        );
        let mut bad = 0;

        if args.sections {
            // The control. A named section this process created and maps by
            // name: no filesystem anywhere, the cheapest kind that exists.
            let mut m = Vec::new();
            let mut t = Vec::new();
            let mut d = Vec::new();
            let mut acc = 0u64;
            for _ in 0..args.iters {
                let owner = match Handle::create_section(args.size) {
                    Ok(h) => h,
                    Err(e) => {
                        eprintln!("  section: create failed: {e}");
                        bad += 1;
                        break;
                    }
                };
                let peer = Handle::from_descriptor(owner.descriptor().clone());
                let t0 = Instant::now();
                let mapping = match peer.map() {
                    Ok(v) => v,
                    Err(e) => {
                        eprintln!("  section: map failed: {e}");
                        bad += 1;
                        break;
                    }
                };
                m.push(t0.elapsed());
                let t1 = Instant::now();
                acc += touch(mapping.as_slice());
                t.push(t1.elapsed());
                let t2 = Instant::now();
                std::mem::drop(mapping);
                d.push(t2.elapsed());
            }
            std::hint::black_box(acc);
            report("section", m, t, d);
        }

        if let Some(dir) = &args.files {
            let mut m = Vec::new();
            let mut t = Vec::new();
            let mut d = Vec::new();
            let mut acc = 0u64;
            for i in 0..args.iters {
                let path = pool_path(dir, i);
                let h = Handle::from_descriptor(Descriptor::SharedFile {
                    path: path.clone(),
                    size: args.size,
                });
                let t0 = Instant::now();
                let mapping = match h.map() {
                    Ok(v) => v,
                    Err(e) => {
                        eprintln!("  shared-file: map {path}: {e}");
                        bad += 1;
                        break;
                    }
                };
                m.push(t0.elapsed());
                let t1 = Instant::now();
                acc += touch(mapping.as_slice());
                t.push(t1.elapsed());
                let t2 = Instant::now();
                std::mem::drop(mapping);
                d.push(t2.elapsed());
            }
            std::hint::black_box(acc);
            report("shared-file", m, t, d);
        }

        if let Some((path, size)) = &args.arena {
            // Attached once, out of band, before any pool exists — which is
            // what an arena is in the real deployment, and why its per-pool
            // cost is arithmetic. The attach is reported separately rather than
            // amortised into the pools, because hiding a one-off cost inside a
            // per-pool number is how a transport wins an argument it should
            // have lost.
            let t0 = Instant::now();
            if let Err(e) = arena::attach(0, path, *size) {
                eprintln!("  arena: attach {path}: {e}");
                return std::process::ExitCode::FAILURE;
            }
            println!(
                "  arena attach   {:9.1} us, once, for all pools",
                t0.elapsed().as_secs_f64() * 1e6
            );

            let mut m = Vec::new();
            let mut t = Vec::new();
            let mut d = Vec::new();
            let mut acc = 0u64;
            for i in 0..args.iters {
                let offset = (i as u64) * args.size;
                if offset + args.size > *size {
                    eprintln!(
                        "  arena: {} pools of {} do not fit in {size}",
                        args.iters, args.size
                    );
                    bad += 1;
                    break;
                }
                let h = Handle::from_descriptor(Descriptor::ArenaSlice {
                    arena: 0,
                    offset,
                    len: args.size,
                });
                let t0 = Instant::now();
                let mapping = match h.map() {
                    Ok(v) => v,
                    Err(e) => {
                        eprintln!("  arena: window at {offset}: {e}");
                        bad += 1;
                        break;
                    }
                };
                m.push(t0.elapsed());
                let t1 = Instant::now();
                acc += touch(mapping.as_slice());
                t.push(t1.elapsed());
                let t2 = Instant::now();
                std::mem::drop(mapping);
                d.push(t2.elapsed());
            }
            std::hint::black_box(acc);
            report("arena-slice", m, t, d);
        }

        // ------------------------------------------------------------ 7 ----
        // The same measurement over the region the shipping deployment
        // actually has. Everything above this point maps *somebody else's*
        // object; this commits guest RAM and pins it, which is the substrate
        // docs/design.md §10.13 item 1 exists to install. The per-pool numbers
        // should be indistinguishable from the file-backed arena's — the work
        // per pool is a registry lookup either way — and the interesting column
        // is `touch`, which no longer crosses a filesystem daemon.
        if let Some(size) = args.guest_arena {
            let t0 = Instant::now();
            let region = match arena::attach_guest_ram(1, size) {
                Ok(r) => r,
                Err(e) => {
                    eprintln!("  guest arena: {e}");
                    return std::process::ExitCode::FAILURE;
                }
            };
            println!(
                "  guest arena    {:9.1} us, once, for all pools \
                 (run 0x{:016x}, {} pages, locked={})",
                t0.elapsed().as_secs_f64() * 1e6,
                region.run_id,
                region.pages,
                region.locked
            );
            // The line the harness greps for, and the only thing the host is
            // told. Printed before anything else happens with the region,
            // because the stamps are already written by the time `attach_guest_ram`
            // returns and the host may start sweeping the instant it sees this.
            println!("GUESTARENA-READY: run=0x{:016x} pages={}", region.run_id, region.pages);
            use std::io::Write as _;
            let _ = std::io::stdout().flush();

            let mut m = Vec::new();
            let mut t = Vec::new();
            let mut d = Vec::new();
            let mut acc = 0u64;
            for i in 0..args.iters {
                let offset = (i as u64) * args.size;
                if offset + args.size > size {
                    eprintln!("  guest arena: {} pools of {} do not fit in {size}", args.iters, args.size);
                    bad += 1;
                    break;
                }
                let h = Handle::from_descriptor(Descriptor::ArenaSlice {
                    arena: 1,
                    offset,
                    len: args.size,
                });
                let t0 = Instant::now();
                let mapping = match h.map() {
                    Ok(v) => v,
                    Err(e) => {
                        eprintln!("  guest arena: window at {offset}: {e}");
                        bad += 1;
                        break;
                    }
                };
                m.push(t0.elapsed());
                let t1 = Instant::now();
                acc += touch(mapping.as_slice());
                t.push(t1.elapsed());
                let t2 = Instant::now();
                std::mem::drop(mapping);
                d.push(t2.elapsed());
            }
            std::hint::black_box(acc);
            report("arena-guest-ram", m, t, d);

            // The live half. One mapping, held, watched — which is exactly the
            // configuration `probe-filemap` measured as MISMATCH on 65280 of
            // 65536 bytes over the export (§10.3), and the configuration a
            // window animating depends on.
            if let Some((off, secs)) = args.watch {
                let win = match Handle::from_descriptor(Descriptor::ArenaSlice {
                    arena: 1,
                    offset: off,
                    len: 32,
                })
                .map()
                {
                    Ok(v) => v,
                    Err(e) => {
                        eprintln!("  guest arena: watch window at {off}: {e}");
                        return std::process::ExitCode::FAILURE;
                    }
                };
                let before: Vec<u8> = win.as_slice().to_vec();
                let started = Instant::now();
                let deadline = Duration::from_secs(secs);
                let mut saw = None;
                while started.elapsed() < deadline {
                    if win.as_slice() != before.as_slice() {
                        saw = Some(started.elapsed());
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(20));
                }
                match saw {
                    Some(dt) => println!(
                        "COH-GUEST-SAW: offset={off} after_ms={} bytes={:?}",
                        dt.as_millis(),
                        String::from_utf8_lossy(win.as_slice())
                            .trim_end_matches(|c: char| c as u32 == 0xAB || c == '\0')
                            .to_string()
                    ),
                    None => {
                        println!("COH-GUEST-SAW: offset={off} after_ms=never nothing changed in {secs}s");
                        bad += 1;
                    }
                }
                // And answer, through the one write path a provided arena has,
                // while this mapping is still live. The host is watching.
                if let Some((roff, text)) = &args.reply {
                    match arena::guest_write(1, *roff, text.as_bytes()) {
                        Ok(()) => println!("COH-GUEST-WROTE: offset={roff} text={text:?}"),
                        Err(e) => {
                            eprintln!("  guest arena: reply at {roff}: {e}");
                            bad += 1;
                        }
                    }
                }
            }
        }

        let (s, f, a) = wc_handle::map_counts();
        println!("  mapped: {s} section, {f} shared-file, {a} arena-slice");
        if bad == 0 {
            println!("RESULT: all measurements completed");
            std::process::ExitCode::SUCCESS
        } else {
            println!("RESULT: {bad} measurement(s) failed");
            std::process::ExitCode::FAILURE
        }
    }
}

#[cfg(not(windows))]
mod imp {
    //! On Unix the question does not arise: an `h` argument is an fd, there is
    //! no `SharedFile` and no arena to compare it with. Mirroring
    //! `crates/wc-proxy`'s inverse of this, the binary still builds so that the
    //! workspace gates cover it on both targets, and says why it did nothing.
    pub fn main() -> std::process::ExitCode {
        eprintln!(
            "wc-poolcost measures what a pool costs a Windows receiver; \
             on Unix an `h` argument is a file descriptor and there is nothing to compare."
        );
        std::process::ExitCode::from(2)
    }
}
