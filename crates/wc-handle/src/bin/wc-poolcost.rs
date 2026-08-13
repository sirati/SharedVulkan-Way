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
        sections: bool,
        prepare: Option<String>,
    }

    fn usage() -> &'static str {
        "\
usage: wc-poolcost [options]

  --iters N              how many pools to measure (default 32)
  --size BYTES           how big each pool is (default 1048576)

  --files DIR            SharedFile: DIR holds pc-000000.bin .. pc-<N>.bin,
                         created by whoever owns the filesystem
  --arena PATH:BYTES     ArenaSlice: attach PATH once, then cut --iters slices
  --sections             also measure a named section, as the control
  --prepare DIR          create the pool files in DIR and exit
"
    }

    fn parse() -> Result<Args, String> {
        let mut a = Args {
            iters: 32,
            size: 1 << 20,
            files: None,
            arena: None,
            sections: false,
            prepare: None,
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

    fn report(label: &str, map: Vec<Duration>, touch: Vec<Duration>, drop_: Vec<Duration>) {
        let (mm, md) = stats(map);
        let (tm, _) = stats(touch);
        let (dm, _) = stats(drop_);
        println!(
            "  {label:<14} map {mm:9.1} us (median {md:8.1})  touch {tm:9.1} us  \
             drop {dm:7.1} us  total {:9.1} us/pool",
            mm + tm + dm
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
