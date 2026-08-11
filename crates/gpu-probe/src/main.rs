//! Step 7's first question, isolated: **has a GPU ever executed a workload
//! against arena memory?**
//!
//! Everything measured so far proves the driver imports our pages and that CPUs
//! on both sides of the hypervisor see the same bytes. An import is not a
//! submission. `security-model.md` R15 and `design.md` §14 item 1 both say so,
//! and until this program ran it was the biggest unmeasured thing in the
//! project.
//!
//! What it does, in order:
//!
//!  1. Allocates an arena-shaped window with one of the four allocators
//!     `awe-probe` measured — the *same* code, from `awe_probe::alloc`, because
//!     a re-typed allocator would be measuring different memory.
//!  2. Lets the Linux host fill the first half of it, through QEMU's memfd.
//!  3. Imports the whole window as one `VkDeviceMemory`, binds two `VkBuffer`s
//!     to it (at offset 0 and half-way in), and dispatches a compute shader
//!     that reads every input word and writes a value depending on the word and
//!     its index.
//!  4. Checks the answer from the guest CPU, and leaves it for the Linux host
//!     to check independently through the memfd.
//!  5. Times the same dispatch against driver-allocated memory, so "is imported
//!     host memory slower?" gets a number instead of a shrug.
//!
//! Console only, no window. The workload is bounded, the fence has a timeout,
//! and a device loss stops the run rather than being retried: this GPU drives
//! the physical laptop panel and somebody is using the machine (R1, R18).
//!
//! ```text
//! cargo build -p gpu-probe --release --target x86_64-pc-windows-gnu
//! gpu-probe.exe --workdir "Z:\probe" --pages 16384 --mode large
//! gpu-probe.exe --pages 16384 --mode large --no-host   # guest alone, no memfd
//! ```
//!
//! On Linux the same binary runs the whole Vulkan path against ordinary
//! `mmap`ed memory. That is not a measurement of the arena — it shares no pages
//! with anything — but it is how the shader and the submission path get
//! exercised before they are pointed at somebody's laptop.

mod vkwork;

use arena_record as rec;
use arena_record::gpu as agpu;
use awe_probe::log;
use awe_probe::rendezvous::{parse_hex_field, wait_for, write_file};
use std::ffi::c_void;
use std::path::PathBuf;
use std::time::{Duration, Instant};
use vkwork::{Fail, Gpu, Work};

const PAGE: usize = rec::PAGE;

struct Opts {
    workdir: PathBuf,
    pages: usize,
    mode: String,
    no_host: bool,
    device_index: usize,
    perf_iters: usize,
    concurrent: bool,
    /// Skip the perf comparison. Useful when the only question is correctness
    /// and the machine should be disturbed as little as possible.
    no_perf: bool,
    /// Non-Windows only: back the window with a shared mapping of this file
    /// instead of anonymous memory, so `arena-host` can play the host half
    /// against it. That makes the entire three-party protocol — fill, dispatch,
    /// verify from the other side, churn — runnable on one machine before any
    /// of it is pointed at somebody's laptop. It shares nothing across a
    /// hypervisor and proves nothing about the arena; it proves the harness.
    file: Option<PathBuf>,
}

fn parse_args() -> Opts {
    let mut o = Opts {
        workdir: PathBuf::from(if cfg!(windows) { "Z:\\probe" } else { "/tmp/probe" }),
        pages: 16384,
        mode: String::from("large,locked"),
        no_host: !cfg!(windows),
        device_index: 0,
        perf_iters: 7,
        concurrent: false,
        no_perf: false,
        file: None,
    };
    let args: Vec<String> = std::env::args().collect();
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--workdir" => {
                o.workdir = PathBuf::from(&args[i + 1]);
                i += 2;
            }
            "--pages" => {
                o.pages = args[i + 1].parse().expect("--pages takes a number");
                i += 2;
            }
            "--mode" => {
                o.mode = args[i + 1].clone();
                i += 2;
            }
            "--device" => {
                o.device_index = args[i + 1].parse().expect("--device takes a number");
                i += 2;
            }
            "--iters" => {
                o.perf_iters = args[i + 1].parse().expect("--iters takes a number");
                i += 2;
            }
            "--file" => {
                o.file = Some(PathBuf::from(&args[i + 1]));
                o.no_host = false;
                i += 2;
            }
            "--no-host" => {
                o.no_host = true;
                i += 1;
            }
            "--no-perf" => {
                o.no_perf = true;
                i += 1;
            }
            "--concurrent" => {
                o.concurrent = true;
                i += 1;
            }
            other => panic!("unknown argument {other}"),
        }
    }
    o
}

fn main() {
    let opts = parse_args();
    match run(&opts) {
        Ok(()) => log!("done"),
        Err(Fail::DeviceLost(what)) => {
            log!("");
            log!("*** VK_ERROR_DEVICE_LOST at {what} ***");
            log!("Stopping. A device loss is visible to whoever is at the machine and");
            log!("retrying it is the wrong response — this is the result, report it.");
            std::process::exit(3);
        }
        Err(e) => {
            log!("FAILED: {e}");
            std::process::exit(1);
        }
    }
}

// ---------------------------------------------------------------------------
// the window
// ---------------------------------------------------------------------------

/// A window of arena-shaped memory, however this platform got one.
struct Region {
    base: *mut c_void,
    pages: usize,
    /// What allocator really produced it, asserted rather than assumed.
    label: String,
    pfns: Option<Vec<usize>>,
    wsex: String,
    /// Kept alive for as long as the region is used; dropping it releases.
    #[allow(dead_code)]
    keep: Keep,
}

/// Held only for its `Drop`. Releasing an AWE window or unlocking a
/// `VirtualLock`ed one is the whole job, and on a machine somebody is using,
/// giving the pages back on every exit path matters more than it would in a
/// test harness.
#[cfg(windows)]
#[allow(dead_code)]
enum Keep {
    Win(awe_probe::alloc::Window),
}

#[cfg(not(windows))]
enum Keep {
    Posix {
        base: *mut c_void,
        size: usize,
        /// Held open for as long as the mapping lives when `--file` is used.
        #[allow(dead_code)]
        file: Option<std::fs::File>,
    },
}

#[cfg(not(windows))]
impl Drop for Keep {
    fn drop(&mut self) {
        let Keep::Posix { base, size, .. } = self;
        unsafe { libc_munmap(*base, *size) };
    }
}

#[cfg(windows)]
fn acquire(opts: &Opts) -> Result<Region, Fail> {
    use awe_probe::alloc::{self, Mode, Window};
    match awe_probe::win::enable_privilege("SeLockMemoryPrivilege") {
        Ok(true) => log!("SeLockMemoryPrivilege: enabled"),
        Ok(false) => log!("SeLockMemoryPrivilege: not-held (large pages and AWE will fail)"),
        Err(e) => log!("SeLockMemoryPrivilege: error: {e}"),
    }
    let order: Vec<Mode> = opts
        .mode
        .split(',')
        .map(|m| Mode::parse(m).unwrap_or_else(|| panic!("unknown mode {m}")))
        .collect();
    let pages = alloc::round_pages(&order, opts.pages);
    let w = Window::acquire(&order, pages).map_err(Fail::Other)?;
    let base = w.base;
    let pages = w.pages;
    let mode = w.mode;
    let pfns = w.pfns().map(|p| p.to_vec());
    let wsex = alloc::wsex_report(base, pages * PAGE);
    // Assert what we got rather than what we asked for: a slab that silently
    // fell back to 4 KiB pages has different properties (R26), and a
    // measurement that names the wrong allocator is worse than no measurement.
    let large = alloc::is_large_page_backed(base);
    let label = format!(
        "{} (LargePage bit: {})",
        mode.as_str(),
        match large {
            Some(true) => "set",
            Some(false) => "clear",
            None => "unknown",
        }
    );
    Ok(Region {
        base,
        pages,
        label,
        pfns,
        wsex,
        keep: Keep::Win(w),
    })
}

#[cfg(not(windows))]
unsafe extern "C" {
    fn mmap(
        addr: *mut c_void,
        len: usize,
        prot: i32,
        flags: i32,
        fd: i32,
        off: i64,
    ) -> *mut c_void;
    fn munmap(addr: *mut c_void, len: usize) -> i32;
}

#[cfg(not(windows))]
unsafe fn libc_munmap(p: *mut c_void, n: usize) -> i32 {
    unsafe { munmap(p, n) }
}

/// The Linux path exists to exercise the shader, the submission and the
/// three-party protocol — not the arena. It says so in the label so that no
/// report can quote it as an arena result.
#[cfg(not(windows))]
fn acquire(opts: &Opts) -> Result<Region, Fail> {
    use std::os::unix::io::AsRawFd;
    const PROT_READ: i32 = 1;
    const PROT_WRITE: i32 = 2;
    const MAP_SHARED: i32 = 1;
    const MAP_PRIVATE: i32 = 2;
    const MAP_ANONYMOUS: i32 = 0x20;
    let size = opts.pages * PAGE;

    let (base, file, label) = match &opts.file {
        Some(path) => {
            let f = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .open(path)
                .map_err(|e| Fail::Other(format!("open {path:?}: {e}")))?;
            f.set_len(size as u64)
                .map_err(|e| Fail::Other(format!("size {path:?}: {e}")))?;
            let b = unsafe {
                mmap(
                    std::ptr::null_mut(),
                    size,
                    PROT_READ | PROT_WRITE,
                    MAP_SHARED,
                    f.as_raw_fd(),
                    0,
                )
            };
            (
                b,
                Some(f),
                format!(
                    "linux MAP_SHARED of {path:?} (HARNESS TEST — one machine, no hypervisor, \
                     proves nothing about the arena)"
                ),
            )
        }
        None => {
            let b = unsafe {
                mmap(
                    std::ptr::null_mut(),
                    size,
                    PROT_READ | PROT_WRITE,
                    MAP_PRIVATE | MAP_ANONYMOUS,
                    -1,
                    0,
                )
            };
            (
                b,
                None,
                "linux-anonymous (DRY RUN — shares nothing, proves nothing about the arena)"
                    .to_string(),
            )
        }
    };
    if base as isize == -1 {
        return Err(Fail::Other("mmap failed".into()));
    }
    unsafe { std::ptr::write_bytes(base as *mut u8, rec::FILL, size) };
    Ok(Region {
        base,
        pages: opts.pages,
        label,
        pfns: None,
        wsex: String::new(),
        keep: Keep::Posix { base, size, file },
    })
}

// ---------------------------------------------------------------------------
// the run
// ---------------------------------------------------------------------------

fn run(opts: &Opts) -> Result<(), Fail> {
    let run_id = awe_probe::new_run_id();
    log!(
        "gpu-probe: run_id=0x{run_id:016x} pages={} ({} MiB) workdir={:?}",
        opts.pages,
        opts.pages * PAGE / (1 << 20),
        opts.workdir
    );

    let region = acquire(opts)?;
    let pages = region.pages;
    let base = region.base;
    let size = pages * PAGE;
    log!(
        "window: {} pages ({} MiB) at 0x{:x} — {}",
        pages,
        size / (1 << 20),
        base as usize,
        region.label
    );

    let split = agpu::Split::for_window(pages);
    if split.pages_per_half == 0 {
        return Err(Fail::Other("the window is too small to split in two".into()));
    }
    let half = split.input_bytes();
    let words = split.words();
    log!(
        "split: input = pages 0..{} ({} MiB), output = pages {}..{}, {} words per half \
         ({} of them payload)",
        split.pages_per_half,
        half / (1 << 20),
        split.pages_per_half,
        split.pages_per_half * 2,
        words,
        words - words / agpu::WORDS_PER_PAGE * agpu::HEADER_WORDS
    );

    // Stamp every page so the host can find them by content, and so the record
    // survives the workload: the shader skips the first 256 bytes of each page.
    stamp(base, pages, run_id, &region);

    // ---- get the input written -------------------------------------------
    let stamp_value = if opts.no_host {
        let s = run_id ^ 0x1111_2222_3333_4444;
        let input = unsafe { std::slice::from_raw_parts_mut(base as *mut u8, half) };
        vkwork::fill_input(input, s, words);
        log!("no host in the loop: filled the input half locally, stamp=0x{s:016x}");
        s
    } else {
        publish_status(opts, run_id, &region, split);
        let Some(text) = wait_for(&opts.workdir.join("host-gpu-fill.txt"), 600) else {
            return Err(Fail::Other("timed out waiting for host-gpu-fill.txt".into()));
        };
        let Some(s) = parse_hex_field(&text, "stamp") else {
            return Err(Fail::Other(
                "host-gpu-fill.txt has no parsable stamp= field — refusing to check against zero"
                    .into(),
            ));
        };
        log!("host says it filled the input half with stamp=0x{s:016x}");
        s
    };

    // What the guest CPU sees of the host's write, before any GPU is involved.
    // Already-established ground (step 0), but it separates "the host's bytes
    // never arrived" from "the GPU did not see them", which is the whole point
    // of measuring it here too.
    {
        let input = unsafe { std::slice::from_raw_parts(base as *const u8, half) };
        let mut ok = 0u64;
        let mut n = 0u64;
        for i in 0..words {
            if !agpu::is_payload(i) {
                continue;
            }
            n += 1;
            let o = i as usize * 4;
            if u32::from_le_bytes(input[o..o + 4].try_into().unwrap())
                == agpu::host_fill(stamp_value, i)
            {
                ok += 1;
            }
        }
        log!("guest CPU sees the writer's input: {ok}/{n} words");
        if ok == 0 {
            return Err(Fail::Other(
                "the input half holds none of the writer's words — stopping before the GPU, \
                 because a dispatch against memory the writer never reached measures nothing"
                    .into(),
            ));
        }
    }

    // ---- open the device and import ---------------------------------------
    let gpu = Gpu::open(opts.device_index)?;
    log!(
        "device: {} — queue family {}, VK_EXT_external_memory_host: {}",
        gpu.name,
        gpu.queue_family,
        if gpu.has_host_import { "yes" } else { "NO" }
    );
    log!(
        "limits: maxStorageBufferRange={} maxComputeWorkGroupCount.x={}",
        gpu.max_storage_buffer_range,
        gpu.max_workgroup_count_x
    );

    let mem = gpu.import(base, size)?;
    log!(
        "imported {} MiB as VkDeviceMemory: memoryTypeBits=0x{:x}, using type {}",
        size / (1 << 20),
        mem.permitted_type_bits,
        mem.type_index
    );

    // ---- THE GATE: can a VkBuffer live in imported memory at all? ----------
    let work = Work::build(&gpu, &mem, half)?;
    log!(
        "bind: buffer memoryTypeBits=0x{:x} (import permitted 0x{:x}), alignment {}, \
         requires {} bytes for a {} byte buffer",
        work.bind.buffer_type_bits,
        work.bind.memory_type_bits,
        work.bind.alignment,
        work.bind.required_size,
        work.bind.bound_size
    );
    log!(
        "bind: vkBindBufferMemory OK at offset 0 and at offset {} — a buffer can live \
         part-way into imported memory",
        work.bind.second_buffer_offset
    );

    // A copy of the output half as it was, so "the GPU never wrote this word"
    // is distinguishable from "the GPU wrote the wrong thing".
    let before: Vec<u8> = unsafe {
        std::slice::from_raw_parts((base as *const u8).add(half), half).to_vec()
    };

    // ---- the submission ----------------------------------------------------
    log!("dispatching the transform kernel over {words} words...");
    let t = work.dispatch(&gpu, words, agpu::KERNEL_TRANSFORM)?;
    log!("submit -> fence signalled in {t:?}");

    let input = unsafe { std::slice::from_raw_parts(base as *const u8, half) };
    let output = unsafe { std::slice::from_raw_parts((base as *const u8).add(half), half) };
    let v = vkwork::verify_transform(input, output, stamp_value, words, Some(&before));
    log!("");
    log!("=== what the GPU computed, judged by the guest CPU ===");
    log!("  payload words checked : {}", v.checked);
    log!("  correct               : {}", v.correct);
    log!("  untouched by the GPU  : {}", v.untouched);
    log!("  wrong                 : {}", v.wrong);
    log!("  input words missing   : {}", v.input_bad);
    for e in &v.examples {
        log!("  ! {e}");
    }
    let passed = v.wrong == 0 && v.untouched == 0 && v.correct == v.checked && v.checked > 0;
    log!(
        "  verdict               : {}",
        if passed { "PASS" } else { "FAIL" }
    );

    // Did the page headers survive? R26's canary, tested rather than asserted:
    // if the GPU had run past its region or the pages had moved, this is where
    // it would show.
    let mut headers_ok = 0usize;
    for p in 0..pages {
        let page = unsafe { std::slice::from_raw_parts((base as *const u8).add(p * PAGE), PAGE) };
        if rec::is_ours(page, run_id) && rec::get_u32(page, rec::OFF_PAGE_INDEX) == p as u32 {
            headers_ok += 1;
        }
    }
    log!("  page headers intact   : {headers_ok}/{pages} (the shader must not touch them)");

    // ---- tell the host, so it can check the same bytes independently -------
    if !opts.no_host {
        let mut out = String::new();
        out.push_str(&format!("run_id={run_id:016x}\n"));
        out.push_str(&format!("stamp={stamp_value:016x}\n"));
        out.push_str(&format!("pages={pages}\n"));
        out.push_str(&format!("words={words}\n"));
        out.push_str(&format!("dispatch_ns={}\n", t.as_nanos()));
        out.push_str(&format!("guest_checked={}\n", v.checked));
        out.push_str(&format!("guest_correct={}\n", v.correct));
        out.push_str(&format!("guest_wrong={}\n", v.wrong));
        out.push_str(&format!("guest_untouched={}\n", v.untouched));
        out.push_str(&format!("headers_intact={headers_ok}\n"));
        out.push_str(&format!("verdict={}\n", if passed { "pass" } else { "fail" }));
        write_file(&opts.workdir.join("guest-gpu.txt"), &out);
        log!("wrote guest-gpu.txt — the host can now verify the same bytes through the memfd");
    }

    // ---- performance -------------------------------------------------------
    if !opts.no_perf {
        perf(&gpu, &work, &mem, half, words, opts.perf_iters)?;
    }

    // ---- concurrency, if asked ---------------------------------------------
    if opts.concurrent && !opts.no_host {
        concurrency(&gpu, &work, opts, base, half, words, pages)?;
    }

    work.destroy(&gpu);
    gpu.free(mem);

    if !opts.no_host {
        if let Some(s) = wait_for(&opts.workdir.join("host-done.txt"), 600) {
            log!("host says: {}", s.trim().replace('\n', " | "));
        } else {
            log!("no host-done.txt — the host half did not report back");
        }
    }
    // Explicit, because the order matters and is not obvious: the Vulkan
    // objects go first, then the window. Releasing guest RAM while the driver
    // still holds an import of it is the one sequencing mistake available here.
    drop(region);
    Ok(())
}

fn stamp(base: *mut c_void, pages: usize, run_id: u64, region: &Region) {
    let w = unsafe { std::slice::from_raw_parts_mut(base as *mut u8, pages * PAGE) };
    // The mode code is cosmetic here; the host matches on the magic and run id.
    for p in 0..pages {
        let gpa = region
            .pfns
            .as_ref()
            .map(|f| (f[p] as u64) << 12)
            .unwrap_or(0);
        let page = rec::make_guest_page(run_id, p as u32, rec::MODE_LARGE_PAGES, gpa);
        w[p * PAGE..(p + 1) * PAGE].copy_from_slice(&page);
    }
    log!("stamped {pages} pages with SVARENA1/{run_id:016x}");
}

fn publish_status(opts: &Opts, run_id: u64, region: &Region, split: agpu::Split) {
    let mut status = String::new();
    status.push_str(&format!("run_id={run_id:016x}\n"));
    status.push_str(&format!("mode={}\n", region.label));
    status.push_str(&format!("pages={}\n", region.pages));
    status.push_str(&format!("pages_per_half={}\n", split.pages_per_half));
    status.push_str(&format!("words={}\n", split.words()));
    status.push_str(&format!("page_size={PAGE}\n"));
    status.push_str(&format!("base_va={:016x}\n", region.base as usize));
    status.push_str(&region.wsex);
    if let Some(pfns) = &region.pfns {
        for (i, pfn) in pfns.iter().enumerate() {
            status.push_str(&format!("pfn {i} {:x} gpa {:x}\n", pfn, (*pfn as u64) << 12));
        }
    }
    write_file(&opts.workdir.join("guest-status.txt"), &status);
    log!("wrote guest-status.txt — the host can scan for run 0x{run_id:016x} now");
}

// ---------------------------------------------------------------------------
// performance
// ---------------------------------------------------------------------------

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

/// Is reading imported host memory dramatically slower than device-local memory
/// on this iGPU?
///
/// It is unified memory, so it may not be — but "may not be" is not a
/// measurement, and the answer decides whether the arena is viable for real
/// workloads or only for staging. Three configurations, same shader, same size,
/// same queue:
///
///  * the imported window,
///  * a driver allocation of *the same memory type*, which isolates "imported"
///    from "host-visible",
///  * a driver allocation of the most device-local type there is, which is what
///    an application that never maps its memory would get.
///
/// The best of `iters` is reported rather than the mean. On a machine somebody
/// is using, the mean measures their workload as much as ours; the minimum is
/// the closest thing to "what this costs when the GPU is free", and the spread
/// is printed so nobody has to take that on faith.
fn perf(
    gpu: &Gpu,
    work: &Work,
    mem: &vkwork::Memory,
    half: usize,
    words: u32,
    iters: usize,
) -> Result<(), Fail> {
    log!("");
    log!("=== performance ===");

    // Submission overhead: the same submit/fence round trip with nothing to do.
    let mut sub = Vec::new();
    for _ in 0..iters.max(10) {
        sub.push(work.dispatch(gpu, 0, agpu::KERNEL_TRANSFORM)?);
    }
    sub.sort();
    log!(
        "  submit+fence with an empty dispatch: min {:.3} ms, median {:.3} ms, max {:.3} ms \
         (n={})",
        ms(sub[0]),
        ms(sub[sub.len() / 2]),
        ms(sub[sub.len() - 1]),
        sub.len()
    );
    let overhead = sub[0];

    let bytes = 2.0 * (words as f64 - (words / agpu::WORDS_PER_PAGE * agpu::HEADER_WORDS) as f64)
        * 4.0;
    // Report the raw round-trip and the throughput derived from it, and state
    // the overhead separately rather than subtracting it. Subtracting was the
    // first version and it was wrong: when the dispatch is as fast as an empty
    // submit — which is exactly what a fast device-local run looks like — the
    // subtraction saturates to zero and prints an infinite bandwidth. A number
    // that goes to infinity when the thing being measured gets good is not a
    // measurement.
    let report = |label: &str, times: &mut Vec<Duration>| {
        times.sort();
        let best = times[0];
        log!(
            "  {label:<34} min {:.3} ms  median {:.3} ms  max {:.3} ms",
            ms(best),
            ms(times[times.len() / 2]),
            ms(times[times.len() - 1])
        );
        if best <= overhead {
            log!(
                "  {:<34} at or below the {:.3} ms empty-submit floor — the dispatch is too \
                 fast for this method to resolve, so no throughput is quoted",
                "",
                ms(overhead)
            );
        } else {
            log!(
                "  {:<34} {:.2} GB/s read+write including submission, {:.2} GB/s with the \
                 {:.3} ms floor removed",
                "",
                bytes / best.as_secs_f64() / 1e9,
                bytes / (best - overhead).as_secs_f64() / 1e9,
                ms(overhead)
            );
        }
        best
    };

    let mut t_imported = Vec::new();
    for _ in 0..iters {
        t_imported.push(work.dispatch(gpu, words, agpu::KERNEL_TRANSFORM)?);
    }
    let imported = report("imported host pointer", &mut t_imported);

    // Baseline 1: same memory type, allocated by the driver. The only variable
    // is where the pages came from.
    let mut same_type = None;
    match gpu.allocate(half * 2, mem.type_index) {
        Ok(m2) => match Work::build(gpu, &m2, half) {
            Ok(w2) => {
                let mut t = Vec::new();
                for _ in 0..iters {
                    t.push(w2.dispatch(gpu, words, agpu::KERNEL_TRANSFORM)?);
                }
                same_type = Some(report(
                    &format!("driver alloc, same type ({})", mem.type_index),
                    &mut t,
                ));
                w2.destroy(gpu);
                gpu.free(m2);
            }
            Err(e) => {
                log!("  driver alloc, same type: buffers refused it ({e})");
                gpu.free(m2);
            }
        },
        Err(e) => log!("  driver alloc, same type: allocation failed ({e})"),
    }

    // Baseline 2: the most device-local type. On a unified iGPU this is memory
    // no application can map, which is the "what you would have used instead"
    // case. Values are not checked — this is a timing measurement only, and
    // saying so matters because a reader could otherwise assume it was verified.
    if let Some(dl) = gpu.device_local_type()
        && dl != mem.type_index
    {
        match gpu.allocate(half * 2, dl) {
            Ok(m3) => match Work::build(gpu, &m3, half) {
                Ok(w3) => {
                    let mut t = Vec::new();
                    for _ in 0..iters {
                        t.push(w3.dispatch(gpu, words, agpu::KERNEL_TRANSFORM)?);
                    }
                    report(
                        &format!("driver alloc, device-local ({dl}, timing only)"),
                        &mut t,
                    );
                    w3.destroy(gpu);
                    gpu.free(m3);
                }
                Err(e) => {
                    log!("  driver alloc, device-local: buffers refused it ({e})");
                    gpu.free(m3);
                }
            },
            Err(e) => log!("  driver alloc, device-local: allocation failed ({e})"),
        }
    }

    if let Some(st) = same_type {
        let ratio = imported.as_secs_f64() / st.as_secs_f64();
        log!(
            "  imported / driver-allocated of the same type: {ratio:.3}× \
             (1.0 would mean the import costs nothing)"
        );
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// concurrency
// ---------------------------------------------------------------------------

/// The one thing R10 has never had: a writer and the GPU touching the same
/// pages at the same time.
///
/// The host churns the input half between two stamps while the GPU repeatedly
/// copies it. Every output word must then be one of the two values the host
/// ever wrote. A word that is neither is a torn or incoherent read, and is the
/// only positive evidence available. A count of zero is one bounded
/// observation, at 4-byte granularity, over whatever interleaving happened —
/// not a proof.
fn concurrency(
    gpu: &Gpu,
    work: &Work,
    opts: &Opts,
    base: *mut c_void,
    half: usize,
    words: u32,
    _pages: usize,
) -> Result<(), Fail> {
    log!("");
    log!("=== concurrent access: the host writes while the GPU reads ===");
    write_file(
        &opts.workdir.join("guest-churn-ready.txt"),
        &format!("words={words}\nready=1\n"),
    );
    let Some(text) = wait_for(&opts.workdir.join("host-churn.txt"), 600) else {
        log!("  no host-churn.txt — skipping, and claiming nothing");
        return Ok(());
    };
    let (Some(a), Some(b)) = (
        parse_hex_field(&text, "stampa"),
        parse_hex_field(&text, "stampb"),
    ) else {
        log!("  host-churn.txt has no parsable stamps — skipping, and claiming nothing");
        return Ok(());
    };
    let secs: u64 = text
        .lines()
        .find_map(|l| l.trim().strip_prefix("seconds="))
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(10);
    log!("  host churning between A=0x{a:016x} and B=0x{b:016x} for {secs}s");

    let output = unsafe { std::slice::from_raw_parts((base as *const u8).add(half), half) };
    let start = Instant::now();
    let mut passes = 0u64;
    let mut total = vkwork::ConcurrentVerdict::default();
    // Stop a little before the host does, so the last pass is still overlapped
    // with a live writer rather than measuring a quiet window.
    while start.elapsed() < Duration::from_secs(secs.saturating_sub(1).max(1)) {
        work.dispatch(gpu, words, agpu::KERNEL_COPY)?;
        let v = vkwork::verify_concurrent_copy(output, (a, b), words);
        total.checked += v.checked;
        total.saw_a += v.saw_a;
        total.saw_b += v.saw_b;
        total.neither += v.neither;
        for e in v.examples {
            if total.examples.len() < 6 {
                total.examples.push(e);
            }
        }
        passes += 1;
    }
    log!("  {passes} dispatch+check passes in {:?}", start.elapsed());
    log!("  words checked      : {}", total.checked);
    log!("  held the old value : {}", total.saw_a);
    log!("  held the new value : {}", total.saw_b);
    log!("  NEITHER            : {}", total.neither);
    for e in &total.examples {
        log!("  ! {e}");
    }
    if total.saw_a == 0 || total.saw_b == 0 {
        log!(
            "  NOTE: the GPU only ever saw one of the two values, so the writes did not \
             actually overlap the dispatches. This measures nothing about concurrency."
        );
    }
    write_file(
        &opts.workdir.join("guest-churn-done.txt"),
        &format!(
            "passes={passes}\nchecked={}\nsaw_a={}\nsaw_b={}\nneither={}\n",
            total.checked, total.saw_a, total.saw_b, total.neither
        ),
    );
    Ok(())
}
