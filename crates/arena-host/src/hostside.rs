//! Step 0, host half: find the guest's pages inside QEMU's memfd and prove
//! they are the same memory.
//!
//! The guest half (`awe-probe`) stamps a window of guest RAM with one record
//! per page. This program opens QEMU's `memory-backend-memfd` through
//! `/proc/<qemu-pid>/fd/<n>`, locates those pages, reads the guest's records
//! back, and writes records of its own so the guest can confirm the traffic
//! runs both ways. It is the `sv-hostd` side of the arena reduced to the one
//! claim that has to hold before any of it is worth building.
//!
//! Two ways to locate the pages, and running both is the point:
//!
//! * `scan` sweeps the whole memfd looking for the magic. It needs nothing
//!   from the guest but the run id, and it *finds* the pages rather than
//!   trusting a formula.
//! * `translate` takes the guest-physical addresses the guest reported (AWE
//!   gives them to us) and applies the GPA→offset mapping read off QEMU's
//!   `info mtree -f`.
//!
//! If the two agree, R21's translation is measured rather than assumed. If they
//! disagree, the formula is wrong, which is exactly the silent failure R21
//! warns about — so the tool refuses to guess and prints both answers.
//!
//! Runs as root on the VM host, because the memfd is only reachable through
//! QEMU's `/proc` entry. The host has no Rust toolchain, so the binary is built
//! here and copied over — and since a NixOS machine has no `/lib64/ld-linux`,
//! it is invoked through the store's loader explicitly:
//!
//! ```text
//! G=/nix/store/<hash>-glibc-<ver>; L=/nix/store/<hash>-gcc-<ver>-lib/lib
//! $G/lib/ld-linux-x86-64.so.2 --library-path $G/lib:$L ./arena-host ...
//! ```
//!
//! A static build would avoid that, but the dev shell has no `glibc.static`,
//! and adding one to the flake for a probe is not worth the closure.

use arena_record as rec;
use arena_record::gpu;
use std::ffi::c_void;
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::fs::FileExt;
use std::os::unix::io::AsRawFd;
use std::time::{Duration, Instant};

const PROT_READ: i32 = 1;
const PROT_WRITE: i32 = 2;
const MAP_SHARED: i32 = 1;
const MS_SYNC: i32 = 4;

unsafe extern "C" {
    fn mmap(addr: *mut c_void, len: usize, prot: i32, flags: i32, fd: i32, off: i64)
    -> *mut c_void;
    fn munmap(addr: *mut c_void, len: usize) -> i32;
    fn msync(addr: *mut c_void, len: usize, flags: i32) -> i32;
}

/// One RAM alias from QEMU's flat view: `[gpa, gpa+size)` of guest-physical
/// address space is `[off, off+size)` of the backing object.
#[derive(Clone, Copy, Debug)]
struct Region {
    gpa: u64,
    size: u64,
    off: u64,
}

fn parse_regions(spec: &str) -> Vec<Region> {
    spec.split(',')
        .filter(|s| !s.trim().is_empty())
        .map(|s| {
            let f: Vec<u64> = s
                .split(':')
                .map(|x| {
                    let x = x.trim();
                    u64::from_str_radix(x.trim_start_matches("0x"), 16)
                        .unwrap_or_else(|_| panic!("bad region field {x:?}"))
                })
                .collect();
            assert_eq!(f.len(), 3, "region wants gpa:size:off");
            Region {
                gpa: f[0],
                size: f[1],
                off: f[2],
            }
        })
        .collect()
}

fn gpa_to_off(regions: &[Region], gpa: u64) -> Option<u64> {
    regions
        .iter()
        .find(|r| gpa >= r.gpa && gpa < r.gpa + r.size)
        .map(|r| r.off + (gpa - r.gpa))
}

fn off_to_gpa(regions: &[Region], off: u64) -> Option<u64> {
    regions
        .iter()
        .find(|r| off >= r.off && off < r.off + r.size)
        .map(|r| r.gpa + (off - r.off))
}

/// A page of the memfd, mapped `MAP_SHARED` so writes land in guest RAM.
struct PageMap {
    addr: *mut u8,
    len: usize,
}

impl PageMap {
    fn new(f: &File, off: u64, len: usize) -> std::io::Result<Self> {
        let addr = unsafe {
            mmap(
                std::ptr::null_mut(),
                len,
                PROT_READ | PROT_WRITE,
                MAP_SHARED,
                f.as_raw_fd(),
                off as i64,
            )
        };
        if addr as isize == -1 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(PageMap {
            addr: addr as *mut u8,
            len,
        })
    }
    fn as_slice(&self) -> &[u8] {
        unsafe { std::slice::from_raw_parts(self.addr, self.len) }
    }
    fn as_mut_slice(&mut self) -> &mut [u8] {
        unsafe { std::slice::from_raw_parts_mut(self.addr, self.len) }
    }
    fn flush(&self) {
        unsafe { msync(self.addr as *mut c_void, self.len, MS_SYNC) };
    }
}

impl Drop for PageMap {
    fn drop(&mut self) {
        unsafe { munmap(self.addr as *mut c_void, self.len) };
    }
}

fn arg(args: &[String], key: &str) -> Option<String> {
    args.iter()
        .position(|a| a == key)
        .and_then(|i| args.get(i + 1))
        .cloned()
}

fn hexarg(args: &[String], key: &str) -> Option<u64> {
    arg(args, key).map(|v| {
        u64::from_str_radix(v.trim_start_matches("0x"), 16)
            .unwrap_or_else(|_| panic!("{key} wants hex"))
    })
}

pub fn run() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 {
        eprintln!(
            "usage:\n  \
             arena-host scan --fd <path> --run <hex> [--out <file>]\n  \
             arena-host translate --fd <path> --run <hex> --gpas <guest-status.txt> \
             --regions <gpa:size:off,...> [--out <file>]\n  \
             arena-host locate --fd <path> --run <hex> [--locator scan|pfns|both] \
             [--gpas <guest-status.txt>] [--out <file>]\n  \
             arena-host write1 --fd <path> --map <file> --stamp <hex>\n  \
             arena-host verify --fd <path> --map <file> [--stamp <hex>] [--vkstamp <hex>]\n  \
             arena-host write2 --fd <path> --map <file> --stamp <hex>\n\
             \n\
             the GPU-workload phase (docs/measured-gpu-side.md, \"a GPU has now run\"):\n  \
             arena-host gpufill   --fd <path> --map <file> --stamp <hex>\n  \
             arena-host gpuverify --fd <path> --map <file> --stamp <hex>\n  \
             arena-host gpuchurn  --fd <path> --map <file> --stampa <hex> --stampb <hex> \
             [--seconds <n>]"
        );
        std::process::exit(2);
    }
    let cmd = args[1].clone();
    let fd_path = arg(&args, "--fd").expect("--fd is required");
    let regions = parse_regions(
        &arg(&args, "--regions")
            .unwrap_or_else(|| String::from("0:0xc0000000:0,0x100000000:0x280000000:0xc0000000")),
    );

    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&fd_path)
        .unwrap_or_else(|e| panic!("open {fd_path}: {e}"));
    let size = file.metadata().unwrap().len();
    eprintln!("memfd {fd_path}: {size} bytes ({} MiB)", size / (1 << 20));

    match cmd.as_str() {
        "scan" => {
            let run = hexarg(&args, "--run").expect("--run");
            let found = scan(&file, size, run);
            report_map(&found, &regions, arg(&args, "--out"));
        }
        // design.md §0.14: the same extent, located by either mechanism or by
        // both. `--locator scan` is `scan` under another name and is the default
        // so that nothing acquires the undocumented dependency by accident;
        // `both` is R28's cross-check and is the mode that proves the frame path
        // works, because it locates the same memory twice by two mechanisms with
        // nothing in common and refuses if they differ about any page.
        "locate" => {
            let run = hexarg(&args, "--run").expect("--run");
            let which = arg(&args, "--locator").unwrap_or_else(|| "scan".into());
            assert!(
                matches!(which.as_str(), "scan" | "pfns" | "both"),
                "--locator wants scan|pfns|both, not {which:?}"
            );
            let frames = (which != "scan").then(|| {
                let path = arg(&args, "--gpas")
                    .expect("--locator pfns|both needs --gpas <guest-status.txt>");
                let f = read_frames(&path);
                println!("read {} frame(s) from {path}; first 0x{:x}", f.len(), f[0]);
                f
            });

            let scanned = (which != "pfns").then(|| {
                let t = Instant::now();
                let m = scan(&file, size, run);
                println!("scan: {} pages in {:.3?}", m.len(), t.elapsed());
                m
            });
            let translated = frames.as_ref().map(|f| {
                let t = Instant::now();
                let m = translate_frames(&regions, f);
                println!("frame list: {} pages in {:.3?}", m.len(), t.elapsed());
                m
            });

            // R28, and the reason the scan is not deleted: the frame list is
            // believed *because* this agreed, on the one occasion the stamps
            // still exist to check it against.
            if let (Some(s), Some(p)) = (&scanned, &translated) {
                let d = compare_maps(s, p);
                println!(
                    "R28 cross-check: {} of {} pages agree, {} disagree",
                    d.agreed, d.pages, d.disagreed
                );
                if let Some((i, a, b)) = d.first {
                    println!(
                        "  first disagreement at page {i}: scan {}, frame list {}",
                        a.map_or("not found".into(), |o| format!("0x{o:010x}")),
                        b.map_or("not found".into(), |o| format!("0x{o:010x}"))
                    );
                }
                if d.disagreed > 0 {
                    // Fatal, and loud. A confident frame list that names the
                    // wrong memory is the failure R28 exists for, and carrying
                    // on with either answer is choosing one at random.
                    eprintln!(
                        "FATAL: the two locators disagree — the frame list cannot be trusted"
                    );
                    std::process::exit(1);
                }
                println!("the frame list is corroborated by the scan; it may now be trusted");
            }

            // The frame list's map is what is reported when both ran: they are
            // equal — that is what was just checked — and it is the one the
            // session would re-locate with afterwards.
            let found = translated.or(scanned).expect("one locator always runs");
            report_map(&found, &regions, arg(&args, "--out"));
        }
        "udmabuf" => {
            let map = read_map(&arg(&args, "--map").expect("--map"));
            let limit_mb: u64 = arg(&args, "--limit-mb")
                .map(|s| s.parse().expect("--limit-mb takes a number"))
                .unwrap_or(16);
            udmabuf_probe(&file, &map, limit_mb);
        }
        "translate" => {
            let run = hexarg(&args, "--run").expect("--run");
            let path = arg(&args, "--gpas").expect("--gpas");
            let text = std::fs::read_to_string(&path).expect("read --gpas file");
            let mut found = Vec::new();
            for line in text.lines() {
                // "pfn <index> <pfn hex> gpa <gpa hex>"
                let f: Vec<&str> = line.split_whitespace().collect();
                if f.len() == 5 && f[0] == "pfn" {
                    let idx: u32 = f[1].parse().unwrap();
                    let gpa = u64::from_str_radix(f[4], 16).unwrap();
                    match gpa_to_off(&regions, gpa) {
                        Some(off) => found.push((idx, off, gpa)),
                        None => println!("page {idx}: gpa 0x{gpa:x} is in no RAM region"),
                    }
                }
            }
            println!("translated {} guest frames to memfd offsets", found.len());
            // Prove the translation rather than trusting it: the record at the
            // computed offset must be the page the guest says it is.
            let mut ok = 0;
            for (idx, off, gpa) in &found {
                let mut buf = [0u8; rec::PAGE];
                if file.read_exact_at(&mut buf, *off).is_err() {
                    println!("page {idx}: read at 0x{off:x} failed");
                    continue;
                }
                if rec::is_ours(&buf, run) && rec::get_u32(&buf, rec::OFF_PAGE_INDEX) == *idx {
                    ok += 1;
                } else {
                    println!(
                        "page {idx}: gpa 0x{gpa:x} -> off 0x{off:x} does NOT hold our record \
                         (magic {:?} run 0x{:016x} idx {})",
                        String::from_utf8_lossy(&rec::tag_at(&buf, rec::OFF_MAGIC)),
                        rec::get_u64(&buf, rec::OFF_RUN_ID),
                        rec::get_u32(&buf, rec::OFF_PAGE_INDEX)
                    );
                }
            }
            println!("translation verified for {ok}/{} pages", found.len());
            let simple: Vec<(u32, u64)> = found.iter().map(|(i, o, _)| (*i, *o)).collect();
            report_map(&simple, &regions, arg(&args, "--out"));
        }
        "write1" | "write2" => {
            let stamp = hexarg(&args, "--stamp").expect("--stamp");
            let map = read_map(&arg(&args, "--map").expect("--map"));
            let (tag, tag_off, stamp_off, idx_off) = if cmd == "write1" {
                (
                    rec::HOST1_TAG,
                    rec::OFF_HOST1,
                    rec::OFF_HOST1_STAMP,
                    rec::OFF_HOST1_INDEX,
                )
            } else {
                (
                    rec::HOST2_TAG,
                    rec::OFF_HOST2,
                    rec::OFF_HOST2_STAMP,
                    rec::OFF_HOST2_INDEX,
                )
            };
            let mut n = 0;
            for (idx, off) in &map {
                let mut pm = PageMap::new(&file, *off, rec::PAGE).expect("mmap page");
                {
                    let s = pm.as_mut_slice();
                    s[tag_off..tag_off + 8].copy_from_slice(tag);
                    rec::put_u64(s, stamp_off, stamp);
                    rec::put_u32(s, idx_off, *idx);
                }
                pm.flush();
                n += 1;
            }
            println!("{cmd}: wrote stamp 0x{stamp:016x} into {n} pages through MAP_SHARED");
        }
        "verify" => {
            let map = read_map(&arg(&args, "--map").expect("--map"));
            let stamp = hexarg(&args, "--stamp");
            let vkstamp = hexarg(&args, "--vkstamp");
            let (mut guest_ok, mut host_ok, mut vk_ok, mut vk_idx_bad) = (0, 0, 0, 0);
            let run = hexarg(&args, "--run");
            for (idx, off) in &map {
                let pm = PageMap::new(&file, *off, rec::PAGE).expect("mmap page");
                let s = pm.as_slice();
                if &rec::tag_at(s, rec::OFF_MAGIC) == rec::MAGIC
                    && run
                        .map(|r| rec::get_u64(s, rec::OFF_RUN_ID) == r)
                        .unwrap_or(true)
                    && rec::get_u32(s, rec::OFF_PAGE_INDEX) == *idx
                {
                    guest_ok += 1;
                }
                if let Some(st) = stamp
                    && &rec::tag_at(s, rec::OFF_HOST1) == rec::HOST1_TAG
                    && rec::get_u64(s, rec::OFF_HOST1_STAMP) == st
                {
                    host_ok += 1;
                }
                if let Some(vs) = vkstamp
                    && &rec::tag_at(s, rec::OFF_VK) == rec::VK_TAG
                    && rec::get_u64(s, rec::OFF_VK_STAMP) == vs
                {
                    // A record that is present but stamped with the wrong page
                    // index is the interesting failure: it means the two sides
                    // agree on the memory and disagree about which page it is.
                    if rec::get_u32(s, rec::OFF_VK_INDEX) == *idx {
                        vk_ok += 1;
                    } else {
                        vk_idx_bad += 1;
                    }
                }
            }
            println!("verify over {} pages:", map.len());
            println!("  guest records intact : {guest_ok}");
            if stamp.is_some() {
                println!("  our own host record  : {host_ok}");
            }
            if vkstamp.is_some() {
                println!("  VULKAN-written record: {vk_ok} ({vk_idx_bad} with a wrong page index)");
            }
            // Show one page in full, because a count is easy to fool and bytes
            // are not.
            if let Some((idx, off)) = map.first() {
                let pm = PageMap::new(&file, *off, rec::PAGE).expect("mmap page");
                let s = pm.as_slice();
                println!(
                    "  page {idx} at memfd 0x{off:x} (gpa 0x{:x}):",
                    off_to_gpa(&regions, *off).unwrap_or(0)
                );
                for (name, o) in [
                    ("magic", rec::OFF_MAGIC),
                    ("guest", rec::OFF_GUEST_TAG),
                    ("host1", rec::OFF_HOST1),
                    ("vk", rec::OFF_VK),
                    ("host2", rec::OFF_HOST2),
                ] {
                    println!(
                        "    {name:<6} {:?} {:016x}",
                        String::from_utf8_lossy(&rec::tag_at(s, o)),
                        rec::get_u64(s, o + 8)
                    );
                }
            }
        }
        "gpufill" => {
            let stamp = hexarg(&args, "--stamp").expect("--stamp");
            let map = read_map(&arg(&args, "--map").expect("--map"));
            let half = halves(&map);
            let t = Instant::now();
            let n = gpu_fill(&file, &map, half, stamp);
            println!(
                "gpufill: wrote host_fill(0x{stamp:016x}, i) into {n} payload words across \
                 {half} input pages in {:?}",
                t.elapsed()
            );
            println!("  the output pages ({half}..{}) were left alone", map.len());
        }
        "gpuverify" => {
            let stamp = hexarg(&args, "--stamp").expect("--stamp");
            let map = read_map(&arg(&args, "--map").expect("--map"));
            let run = hexarg(&args, "--run");
            gpu_verify(&file, &map, halves(&map), stamp, run);
        }
        "gpuchurn" => {
            let a = hexarg(&args, "--stampa").expect("--stampa");
            let b = hexarg(&args, "--stampb").expect("--stampb");
            let secs: u64 = arg(&args, "--seconds")
                .map(|s| s.parse().expect("--seconds wants a number"))
                .unwrap_or(10);
            let map = read_map(&arg(&args, "--map").expect("--map"));
            gpu_churn(&file, &map, halves(&map), a, b, secs);
        }
        other => panic!("unknown command {other}"),
    }
}

// ---------------------------------------------------------------------------
// the GPU-workload phase
// ---------------------------------------------------------------------------

/// Pages in the input half.
///
/// The guest splits its window down the middle: the first half is the shader's
/// input buffer, the second its output. Derived from the *page indices* rather
/// than from the map's length, because a scan that missed a page would
/// otherwise shift the split by half a page and quietly make every subsequent
/// index wrong — R21's failure mode arriving through the harness instead of
/// through the translation.
fn halves(map: &[(u32, u64)]) -> usize {
    let max = map.iter().map(|(i, _)| *i).max().unwrap_or(0) as usize;
    let window = max + 1;
    if map.len() != window {
        println!(
            "WARNING: the map has {} entries but page indices run to {max}. \
             {} pages are missing, and the split is derived from the indices.",
            map.len(),
            window - map.len()
        );
    }
    window / 2
}

/// The entries of `map` whose page index falls in `[lo, hi)`, in index order.
///
/// Selecting by index rather than by position in the map is the same
/// robustness point as `halves`: the page the guest calls 5 is the page the
/// host must write as 5, whatever order the scan found things in and whatever
/// it missed.
fn pages_in(map: &[(u32, u64)], lo: usize, hi: usize) -> Vec<(u32, u64)> {
    let mut v: Vec<(u32, u64)> = map
        .iter()
        .filter(|(i, _)| (*i as usize) >= lo && (*i as usize) < hi)
        .cloned()
        .collect();
    v.sort();
    v
}

/// Map the pages of `map[range]` as few times as possible.
///
/// The step-0 writers `mmap` one page at a time, which is fine for a one-shot
/// 60 ms write and useless for the churn phase, where the host has to keep a
/// 32 MiB region moving fast enough to overlap with GPU dispatches. Coalescing
/// contiguous memfd offsets into single mappings turns ~8192 syscalls into ~11
/// on the large-page path — which is the same fragmentation result from the
/// other side, now costing something concrete.
fn coalesce(map: &[(u32, u64)]) -> Vec<(usize, usize, u64, usize)> {
    // (first index in map, count, memfd offset, byte length)
    let mut runs: Vec<(usize, usize, u64, usize)> = Vec::new();
    for (i, (_, off)) in map.iter().enumerate() {
        match runs.last_mut() {
            Some(r) if r.2 + r.3 as u64 == *off => {
                r.1 += 1;
                r.3 += rec::PAGE;
            }
            _ => runs.push((i, 1, *off, rec::PAGE)),
        }
    }
    runs
}

/// Write `host_fill(stamp, i)` into every payload word of the input half.
///
/// The page headers are left untouched, so the `SVARENA1` record still
/// identifies each page after the workload has run. That is R26's canary at the
/// price of 6.25% of the window, and it is what makes `gpuverify` able to say
/// "this is still the page I mapped" rather than only "these bytes are wrong".
fn gpu_fill(file: &File, map: &[(u32, u64)], half: usize, stamp: u64) -> u64 {
    let input = pages_in(map, 0, half);
    let mut written = 0u64;
    for (first, count, off, len) in coalesce(&input) {
        let mut pm = PageMap::new(file, off, len).expect("mmap run");
        let s = pm.as_mut_slice();
        for p in 0..count {
            let page_index = input[first + p].0;
            let page = &mut s[p * rec::PAGE..(p + 1) * rec::PAGE];
            for w in gpu::HEADER_WORDS..gpu::WORDS_PER_PAGE {
                // The word index the *shader* sees is the index within the
                // whole input region, so it has to be derived from the page's
                // logical index and not from where the page happens to sit in
                // the memfd.
                let i = page_index * gpu::WORDS_PER_PAGE + w;
                let o = w as usize * 4;
                page[o..o + 4].copy_from_slice(&gpu::host_fill(stamp, i).to_le_bytes());
                written += 1;
            }
        }
        pm.flush();
    }
    written
}

/// Read the output half through the memfd and check it against what the shader
/// was required to compute.
///
/// This is the independent check. The guest already verified the same words
/// through its own mapping, but the guest's mapping and the Vulkan mapping are
/// the same virtual address (measured), so a guest-side pass proves less than
/// it looks like it does. Reading the bytes out of QEMU's memfd is a different
/// path through different page tables in a different process on the other side
/// of the hypervisor.
fn gpu_verify(file: &File, map: &[(u32, u64)], half: usize, stamp: u64, run: Option<u64>) {
    let output = pages_in(map, half, half * 2);
    let (mut checked, mut correct, mut wrong, mut zero) = (0u64, 0u64, 0u64, 0u64);
    let mut header_ok = 0usize;
    let mut examples: Vec<String> = Vec::new();
    for (first, count, off, len) in coalesce(&output) {
        let pm = PageMap::new(file, off, len).expect("mmap run");
        let s = pm.as_slice();
        for p in 0..count {
            let page_index = output[first + p].0;
            let page = &s[p * rec::PAGE..(p + 1) * rec::PAGE];
            // Is this still the page we mapped? The shader never writes the
            // header, so the answer is available even after the workload.
            if &rec::tag_at(page, rec::OFF_MAGIC) == rec::MAGIC
                && run.map(|r| rec::get_u64(page, rec::OFF_RUN_ID) == r).unwrap_or(true)
                && rec::get_u32(page, rec::OFF_PAGE_INDEX) == page_index
            {
                header_ok += 1;
            }
            // The output word at region index `i` was produced from the input
            // word at the same index — the input region's page `page_index -
            // half`, which is why the shader's index is region-relative.
            let region_index = page_index - half as u32;
            for w in gpu::HEADER_WORDS..gpu::WORDS_PER_PAGE {
                let i = region_index * gpu::WORDS_PER_PAGE + w;
                let o = w as usize * 4;
                let got = u32::from_le_bytes(page[o..o + 4].try_into().unwrap());
                let expect = gpu::transform(gpu::host_fill(stamp, i), i);
                checked += 1;
                if got == expect {
                    correct += 1;
                } else {
                    if got == 0 {
                        zero += 1;
                    }
                    wrong += 1;
                    if examples.len() < 4 {
                        examples.push(format!(
                            "page {page_index} word {w} (region index {i}): expected \
                             0x{expect:08x}, found 0x{got:08x}"
                        ));
                    }
                }
            }
        }
    }
    println!("gpuverify over {} output pages:", output.len());
    println!("  payload words checked : {checked}");
    println!("  as the shader required: {correct}");
    println!("  wrong                 : {wrong} (of which {zero} are zero)");
    println!("  page headers intact   : {header_ok}/{}", output.len());
    for e in &examples {
        println!("  ! {e}");
    }
    println!(
        "  VERDICT: {}",
        if checked > 0 && wrong == 0 {
            "the Linux host can read what the GPU computed"
        } else {
            "FAILED — see above"
        }
    );
}

/// Alternate the input half between two stamps for `secs`, as fast as the host
/// can, while the guest dispatches against it.
///
/// The point is overlap, so it reports how many full passes it managed: if the
/// guest only ever saw one of the two values, the two sides did not actually
/// overlap and the concurrency result means nothing. Saying that out loud is
/// the difference between a measurement and a claim.
fn gpu_churn(file: &File, map: &[(u32, u64)], half: usize, a: u64, b: u64, secs: u64) {
    let start = std::time::Instant::now();
    let mut passes = 0u64;
    let mut which = 0;
    println!(
        "gpuchurn: alternating stamps 0x{a:016x} / 0x{b:016x} over {half} input pages for {secs}s"
    );
    while start.elapsed() < Duration::from_secs(secs) {
        let stamp = if which % 2 == 0 { a } else { b };
        gpu_fill(file, map, half, stamp);
        which += 1;
        passes += 1;
    }
    let el = start.elapsed();
    println!(
        "gpuchurn: {passes} full passes over {} MiB in {el:?} ({:.1} passes/s, {:.2} GB/s of \
         host writes)",
        half * rec::PAGE / (1 << 20),
        passes as f64 / el.as_secs_f64(),
        passes as f64 * (half * rec::PAGE) as f64 / el.as_secs_f64() / 1e9
    );
    if passes < 2 {
        println!(
            "  WARNING: fewer than two passes, so the guest cannot have seen both values. \
             The concurrency result from this run means nothing."
        );
    }
}

/// Sweep the whole memfd for our pages.
///
/// Every record starts at a page boundary by construction, so only the first
/// bytes of each page are examined; the sweep is a linear read of guest RAM
/// rather than a substring search. `read` rather than `mmap` on purpose: a read
/// of an unbacked page of a tmpfs file does not allocate one, and we are
/// touching all 13 GiB of a production VM's memory.
fn scan(file: &File, size: u64, run: u64) -> Vec<(u32, u64)> {
    const CHUNK: usize = 32 << 20;
    let mut buf = vec![0u8; CHUNK];
    let mut f = file.try_clone().unwrap();
    f.seek(SeekFrom::Start(0)).unwrap();
    let mut base = 0u64;
    let mut found = Vec::new();
    let mut other_runs = 0usize;
    while base < size {
        let want = CHUNK.min((size - base) as usize);
        let mut got = 0;
        while got < want {
            match f.read(&mut buf[got..want]) {
                Ok(0) => break,
                Ok(n) => got += n,
                Err(e) => panic!("read at 0x{:x}: {e}", base + got as u64),
            }
        }
        for p in (0..got).step_by(rec::PAGE) {
            if p + rec::PAGE > got {
                break;
            }
            let page = &buf[p..p + rec::PAGE];
            if &rec::tag_at(page, rec::OFF_MAGIC) == rec::MAGIC {
                if rec::get_u64(page, rec::OFF_RUN_ID) == run {
                    found.push((rec::get_u32(page, rec::OFF_PAGE_INDEX), base + p as u64));
                } else {
                    other_runs += 1;
                }
            }
        }
        base += got as u64;
        if got == 0 {
            break;
        }
    }
    if other_runs > 0 {
        println!("note: {other_runs} pages carry the magic but a different run id");
    }
    found.sort();
    found
}

fn report_map(found: &[(u32, u64)], regions: &[Region], out: Option<String>) {
    println!("located {} pages", found.len());
    for (i, (idx, off)) in found.iter().enumerate() {
        if i < 4 || i + 4 >= found.len() {
            println!(
                "  page {idx:>4} -> memfd offset 0x{off:010x}  gpa 0x{:010x}",
                off_to_gpa(regions, *off).unwrap_or(0)
            );
        } else if i == 4 {
            println!("  ...");
        }
    }
    // Are the pages contiguous in guest-physical space? Interesting for the
    // arena's VMA count, and a sanity check on the whole picture.
    let mut runs = 1;
    for w in found.windows(2) {
        if w[1].1 != w[0].1 + rec::PAGE as u64 {
            runs += 1;
        }
    }
    println!("  contiguous runs in the memfd: {runs}");
    if let Some(path) = out {
        let mut f = File::create(&path).expect("create --out");
        for (idx, off) in found {
            writeln!(f, "{idx} {off:x}").unwrap();
        }
        println!("wrote {path}");
    }
}

// ---------------------------------------------------------------------------
// the frame-list locator, and R28's cross-check (design.md §0.14)
// ---------------------------------------------------------------------------

/// Read the frame list out of the guest's `guest-status.txt`.
///
/// The **frame** column is authoritative and the `gpa` column is ignored. They
/// are the same number shifted by twelve, and the guest computes both — so
/// reading the second would be believing the guest's arithmetic about the
/// host's address space rather than doing the translation here, against the
/// region table read off `info mtree -f`. That substitution is R21 exactly.
fn read_frames(path: &str) -> Vec<u64> {
    let text = std::fs::read_to_string(path).unwrap_or_else(|e| panic!("read {path}: {e}"));
    let mut frames = Vec::new();
    for line in text.lines() {
        let f: Vec<&str> = line.split_whitespace().collect();
        if f.first() != Some(&"pfn") {
            continue;
        }
        assert!(f.len() >= 3, "malformed frame line: {line:?}");
        let index: usize = f[1].parse().unwrap_or_else(|_| panic!("bad index in {line:?}"));
        let frame = u64::from_str_radix(f[2], 16)
            .unwrap_or_else(|_| panic!("bad frame number in {line:?}"));
        assert_eq!(index, frames.len(), "the frame list has a hole at page {index}");
        frames.push(frame);
    }
    assert!(!frames.is_empty(), "{path} carries no `pfn` lines");
    frames
}

/// Translate a frame list into memfd offsets, in page order.
///
/// This is the whole of the host's work on the frame path: 0.064 s against the
/// scan's 2.308 s on the same 64 MiB window, because it is arithmetic over a
/// table rather than a read of thirteen gigabytes. A frame that is in no RAM
/// region is reported and dropped rather than translated to something that
/// happens to parse — the pages then come back short, which is loud, instead of
/// pointing at the PCI hole, which is not.
fn translate_frames(regions: &[Region], frames: &[u64]) -> Vec<(u32, u64)> {
    let mut out = Vec::with_capacity(frames.len());
    for (i, frame) in frames.iter().enumerate() {
        let gpa = frame << 12;
        match gpa_to_off(regions, gpa) {
            Some(off) => out.push((i as u32, off)),
            None => println!("page {i}: frame 0x{frame:x} (gpa 0x{gpa:x}) is in no RAM region"),
        }
    }
    out
}

/// What the two locators made of the same extent. `security-model.md` R28.
#[derive(Debug, PartialEq, Eq)]
struct Disagreement {
    pages: usize,
    agreed: usize,
    disagreed: usize,
    /// The first page they differed about, with the scan's offset and the frame
    /// list's — `None` on either side meaning that locator did not find the page
    /// at all. One example beats a count: a uniform shift means the undocumented
    /// interface moved, one wrong page means a frame did, and the two are told
    /// apart by looking at the numbers.
    first: Option<(u32, Option<u64>, Option<u64>)>,
}

/// Compare two page maps index by index.
///
/// Keyed by page index rather than zipped by position, because the two locators
/// can disagree about *which* pages they found as well as about where they are,
/// and zipping two lists of different lengths reports the first as if it were
/// the second — the whole map shifted by one, every page "wrong".
fn compare_maps(scan: &[(u32, u64)], pfns: &[(u32, u64)]) -> Disagreement {
    let by_index = |m: &[(u32, u64)]| -> std::collections::BTreeMap<u32, u64> {
        m.iter().copied().collect()
    };
    let (a, b) = (by_index(scan), by_index(pfns));
    let mut indices: Vec<u32> = a.keys().chain(b.keys()).copied().collect();
    indices.sort_unstable();
    indices.dedup();

    let mut d = Disagreement {
        pages: indices.len(),
        agreed: 0,
        disagreed: 0,
        first: None,
    };
    for i in indices {
        let (x, y) = (a.get(&i).copied(), b.get(&i).copied());
        if x.is_some() && x == y {
            d.agreed += 1;
        } else {
            d.disagreed += 1;
            if d.first.is_none() {
                d.first = Some((i, x, y));
            }
        }
    }
    d
}

fn read_map(path: &str) -> Vec<(u32, u64)> {
    let text = std::fs::read_to_string(path).unwrap_or_else(|e| panic!("read {path}: {e}"));
    text.lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| {
            let f: Vec<&str> = l.split_whitespace().collect();
            (
                f[0].parse().unwrap(),
                u64::from_str_radix(f[1], 16).unwrap(),
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The flat view measured on `testhost`: `-machine pc`, 13312 MiB, so the
    /// below-4G split is 3 GiB and RAM resumes at 4 GiB with a 1 GiB shift.
    const TESTHOST: &str = "0:0xc0000000:0,0x100000000:0x280000000:0xc0000000";

    #[test]
    fn the_measured_mapping_translates_both_ways() {
        let r = parse_regions(TESTHOST);
        // Low RAM is identity-mapped into the backing object.
        assert_eq!(gpa_to_off(&r, 0x0010_f000), Some(0x0010_f000));
        // Above 4 GiB the offset lags the GPA by exactly the size of the hole.
        assert_eq!(gpa_to_off(&r, 0x2_b6a6_d000), Some(0x2_76a6_d000));
        assert_eq!(off_to_gpa(&r, 0x2_76a6_d000), Some(0x2_b6a6_d000));
        // Round trip over every region boundary.
        for gpa in [0u64, 0xbfff_f000, 0x1_0000_0000, 0x3_7fff_f000] {
            let off = gpa_to_off(&r, gpa).expect("in RAM");
            assert_eq!(off_to_gpa(&r, off), Some(gpa));
        }
    }

    /// R21's failure mode: the PCI hole is not RAM, and asking for it must be an
    /// answer of "no such page" rather than an offset that reads something else.
    #[test]
    fn the_pci_hole_is_not_backed() {
        let r = parse_regions(TESTHOST);
        assert_eq!(gpa_to_off(&r, 0xc000_0000), None);
        assert_eq!(gpa_to_off(&r, 0xffff_f000), None);
        assert_eq!(gpa_to_off(&r, 0x3_8000_0000), None, "past the end of RAM");
    }

    /// The mistake this whole tool exists to avoid: treating a memfd offset as a
    /// guest-physical address. Above 4 GiB the two differ by a gigabyte, which
    /// is a perfectly valid page — so the error is silent, not fatal.
    #[test]
    fn identity_and_the_real_mapping_disagree_above_4g() {
        let r = parse_regions(TESTHOST);
        let gpa = 0x2_b6a6_d000u64;
        let off = gpa_to_off(&r, gpa).unwrap();
        assert_ne!(off, gpa);
        assert_eq!(gpa - off, 0x4000_0000);
        // And the naive reading lands on a page that exists, which is why it
        // fails quietly rather than out of bounds.
        assert!(off_to_gpa(&r, off).is_some());
    }

    // ---- the frame-list locator and R28 -------------------------------------

    /// The frame path's arithmetic, on the mapping this machine actually has:
    /// a frame number becomes a GPA by a shift and a GPA becomes an offset by
    /// the region table. Above 4 GiB those are different numbers, which is the
    /// whole reason the second step is a table lookup and not a shift as well.
    #[test]
    fn a_frame_list_translates_through_the_same_table_the_scan_reports_against() {
        let r = parse_regions(TESTHOST);
        // Two frames below the hole, one above it.
        let map = translate_frames(&r, &[0x10f, 0x110, 0x2b6a6d]);
        assert_eq!(map, vec![(0, 0x10f000), (1, 0x110000), (2, 0x2_76a6_d000)]);
        // …and the offsets translate back to the GPAs the frames named, so the
        // two directions of the table agree about the frame path too.
        for (i, off) in &map {
            let frame = [0x10fu64, 0x110, 0x2b6a6d][*i as usize];
            assert_eq!(off_to_gpa(&r, *off), Some(frame << 12));
        }
    }

    /// A frame in the PCI hole is dropped and reported rather than translated,
    /// so the answer comes back *short* — which is loud — instead of pointing at
    /// memory the guest cannot see, which is not.
    #[test]
    fn a_frame_outside_ram_is_dropped_rather_than_translated() {
        let r = parse_regions(TESTHOST);
        // 0xc0000 << 12 is the first byte of the hole; 0x400000 is past the end.
        let map = translate_frames(&r, &[0x10f, 0xc_0000, 0x40_0000, 0x110]);
        assert_eq!(map.len(), 2, "two of the four frames are not RAM");
        assert_eq!(map, vec![(0, 0x10f000), (3, 0x110000)]);
        // The page indices are preserved, so "page 3" is still page 3 and the
        // shortfall cannot be mistaken for a shift.
        assert!(map.iter().all(|(i, _)| *i != 1 && *i != 2));
    }

    /// R28 as the comparison it actually is: two locators, the same extent, and
    /// the answer is per page rather than per list.
    #[test]
    fn two_locators_that_agree_page_for_page_are_the_r28_cross_check_passing() {
        let scanned = vec![(0u32, 0x1000u64), (1, 0x2000), (2, 0x9000)];
        let translated = scanned.clone();
        let d = compare_maps(&scanned, &translated);
        assert_eq!(d.agreed, 3);
        assert_eq!(d.disagreed, 0);
        assert_eq!(d.first, None);
    }

    /// The failure R28 exists for: a frame list that is confident,
    /// self-consistent, translates cleanly and names the wrong page. Nothing but
    /// the scan can tell, and what it has to say is *which* page — because one
    /// wrong page and a whole map shifted are different diagnoses.
    #[test]
    fn a_frame_list_that_names_the_wrong_page_is_caught_with_the_page_number() {
        let scanned = vec![(0u32, 0x1000u64), (1, 0x2000), (2, 0x9000)];
        let translated = vec![(0u32, 0x1000u64), (1, 0x2000), (2, 0xa000)];
        let d = compare_maps(&scanned, &translated);
        assert_eq!(d.agreed, 2);
        assert_eq!(d.disagreed, 1);
        assert_eq!(d.first, Some((2, Some(0x9000), Some(0xa000))));
    }

    /// The two locators can disagree about *which* pages they found as well as
    /// where they are — a scan of an overwritten window finds none at all, which
    /// is the measured case. Comparing by position would then report every page
    /// as wrong; comparing by index reports the pages that are missing as
    /// missing.
    #[test]
    fn a_locator_that_found_a_page_the_other_did_not_is_a_disagreement_about_that_page() {
        let scanned = vec![(0u32, 0x1000u64), (2, 0x9000)];
        let translated = vec![(0u32, 0x1000u64), (1, 0x2000), (2, 0x9000)];
        let d = compare_maps(&scanned, &translated);
        assert_eq!(d.pages, 3, "the union of the two, not the shorter of them");
        assert_eq!(d.agreed, 2);
        assert_eq!(d.first, Some((1, None, Some(0x2000))));

        // The measured case in full: the window was overwritten, so the scan
        // located zero pages and the query located all of them.
        let d = compare_maps(&[], &translated);
        assert_eq!((d.pages, d.agreed, d.disagreed), (3, 0, 3));
    }

    // ---- the GPU-workload phase --------------------------------------------

    /// The whole point of coalescing: scattered pages cost one mapping each,
    /// contiguous ones cost one mapping between them. This is the fragmentation
    /// result seen from the writing side.
    #[test]
    fn contiguous_pages_become_one_mapping_and_scattered_ones_do_not() {
        // Four pages, laid out contiguously in the memfd.
        let contiguous: Vec<(u32, u64)> = (0..4).map(|i| (i, 0x1000 * i as u64)).collect();
        let runs = coalesce(&contiguous);
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0], (0, 4, 0, 4 * rec::PAGE));

        // The same four pages scattered, as a VirtualLock'ed window is.
        let scattered = vec![(0u32, 0x1000u64), (1, 0x9000), (2, 0xa000), (3, 0x40000)];
        let runs = coalesce(&scattered);
        assert_eq!(runs.len(), 3, "0x1000 | 0x9000+0xa000 | 0x40000");
        assert_eq!(runs[1], (1, 2, 0x9000, 2 * rec::PAGE));
    }

    /// The split is derived from page indices, so a scan that missed a page
    /// must not shift it. This is the harness's own version of R21.
    #[test]
    fn the_split_survives_a_missing_page() {
        let full: Vec<(u32, u64)> = (0..16).map(|i| (i, 0x1000 * i as u64)).collect();
        assert_eq!(halves(&full), 8);
        // Drop one page from the middle. A length-based split would say 7.
        let mut gappy = full.clone();
        gappy.retain(|(i, _)| *i != 3);
        assert_eq!(halves(&gappy), 8, "the split must follow the indices");
        // And selection still picks the right pages by index, not by position.
        let input = pages_in(&gappy, 0, 8);
        assert_eq!(input.len(), 7);
        assert!(input.iter().all(|(i, _)| *i < 8));
        let output = pages_in(&gappy, 8, 16);
        assert_eq!(output.len(), 8);
        assert!(output.iter().all(|(i, _)| *i >= 8));
    }

    /// Output page `half + k` holds the transform of input page `k`. If the
    /// host and the shader disagreed about that, every word would be wrong and
    /// it would read as a GPU failure rather than as arithmetic.
    #[test]
    fn the_output_page_indexes_back_to_its_input_page() {
        let half = 8usize;
        for page_index in half as u32..(half as u32 * 2) {
            let region_index = page_index - half as u32;
            // The word the host verifies at the first payload slot...
            let verify_i = region_index * gpu::WORDS_PER_PAGE + gpu::HEADER_WORDS;
            // ...must be the word the host filled on the matching input page.
            let input_page = region_index;
            let fill_i = input_page * gpu::WORDS_PER_PAGE + gpu::HEADER_WORDS;
            assert_eq!(verify_i, fill_i);
        }
    }

    /// End to end over a fake window in memory: fill, apply the shader's
    /// function, verify. It pins the index arithmetic on both sides at once,
    /// which is the only part of this the unit tests can reach without a GPU.
    #[test]
    fn fill_then_transform_then_verify_agrees_with_itself() {
        let half = 2usize;
        let stamp = 0xFEED_FACE_1234_5678u64;
        let mut input = vec![0u8; half * rec::PAGE];
        let mut output = vec![0u8; half * rec::PAGE];
        for page in 0..half as u32 {
            for w in gpu::HEADER_WORDS..gpu::WORDS_PER_PAGE {
                let i = page * gpu::WORDS_PER_PAGE + w;
                let o = page as usize * rec::PAGE + w as usize * 4;
                input[o..o + 4].copy_from_slice(&gpu::host_fill(stamp, i).to_le_bytes());
            }
        }
        // What the shader does.
        for page in 0..half as u32 {
            for w in gpu::HEADER_WORDS..gpu::WORDS_PER_PAGE {
                let i = page * gpu::WORDS_PER_PAGE + w;
                let o = page as usize * rec::PAGE + w as usize * 4;
                let v = u32::from_le_bytes(input[o..o + 4].try_into().unwrap());
                output[o..o + 4].copy_from_slice(&gpu::transform(v, i).to_le_bytes());
            }
        }
        // What gpuverify checks, with the output page indices it would see.
        let mut checked = 0;
        for page_index in half as u32..(half as u32 * 2) {
            let region_index = page_index - half as u32;
            for w in gpu::HEADER_WORDS..gpu::WORDS_PER_PAGE {
                let i = region_index * gpu::WORDS_PER_PAGE + w;
                let o = region_index as usize * rec::PAGE + w as usize * 4;
                let got = u32::from_le_bytes(output[o..o + 4].try_into().unwrap());
                assert_eq!(got, gpu::transform(gpu::host_fill(stamp, i), i));
                checked += 1;
            }
        }
        assert_eq!(checked, half * (1024 - 64));
    }

    /// A 3.5 GiB split is what an i440fx uses at smaller RAM sizes. If someone
    /// ever hardcodes it here, this fails loudly instead of in production.
    #[test]
    fn a_different_split_gives_a_different_answer() {
        let smaller = parse_regions("0:0xe0000000:0,0x100000000:0x100000000:0xe0000000");
        let ours = parse_regions(TESTHOST);
        assert_ne!(
            gpa_to_off(&smaller, 0x1_2000_0000),
            gpa_to_off(&ours, 0x1_2000_0000)
        );
    }
}

// ---------------------------------------------------------------------------
// udmabuf: can the located pages become a real Linux dma-buf?
// ---------------------------------------------------------------------------
//
// design.md §0.7 recorded three closed routes for RADV and gave the third as
// "`/dev/udmabuf`: `Permission denied` — `root:kvm` 0660, this account is not in
// `kvm`". That was measured on **devhost**, where `sirati` is in
// `users wheel lp networkmanager scanner libvirtd` and not in `kvm`. On
// **testhost**, which is where the VM actually runs, the same account is in
// `302(kvm)` and the device opens. So the recorded blocker is a fact about a
// host that has no VM on it.
//
// That makes the question live rather than settled, and the question is not
// "can we open the device" — it is "does `/dev/udmabuf` accept **QEMU's** memfd".
// udmabuf requires `F_SEAL_SHRINK` on the memfd it wraps. QEMU's
// `memory-backend-memfd` seals by default, but "by default" is a claim about a
// version, and this project's rule is that a claim about somebody else's code
// gets asserted rather than believed.
//
// So this reports `F_GET_SEALS` and then actually performs the ioctl over the
// located pages, mapping the result and comparing its bytes with the memfd's.
// A dma-buf whose bytes are not the same memory is exactly the failure
// `sv_gpu_server::arena` records lavapipe producing, and only reading through
// both mappings catches it.

const F_GET_SEALS: i32 = 1034;
const F_SEAL_SEAL: i32 = 0x0001;
const F_SEAL_SHRINK: i32 = 0x0002;
const F_SEAL_GROW: i32 = 0x0004;
const F_SEAL_WRITE: i32 = 0x0008;

/// `_IOW('u', 0x43, struct udmabuf_create_list)` — the scatter form.
///
/// The single-item `UDMABUF_CREATE` would do for one contiguous run and the
/// pages are never one run: the driver's own 64 MiB allocation came back as
/// 26–55 discontiguous runs in the memfd. The list form takes up to
/// `/sys/module/udmabuf/parameters/list_limit` items, which is 1024 here.
const UDMABUF_CREATE_LIST: u64 = 0x4008_7543;
const UDMABUF_FLAGS_CLOEXEC: u32 = 0x01;

#[repr(C)]
struct UdmabufCreateItem {
    memfd: u32,
    pad: u32,
    offset: u64,
    size: u64,
}

unsafe extern "C" {
    fn fcntl(fd: i32, cmd: i32, ...) -> i32;
    fn ioctl(fd: i32, req: u64, ...) -> i32;
    fn __errno_location() -> *mut i32;
}

fn errno() -> i32 {
    unsafe { *__errno_location() }
}

fn udmabuf_probe(file: &File, map: &[(u32, u64)], limit_mb: u64) {
    use std::os::unix::io::AsRawFd;
    let memfd = file.as_raw_fd();

    // 1. The precondition, read rather than assumed.
    let seals = unsafe { fcntl(memfd, F_GET_SEALS) };
    if seals < 0 {
        println!("F_GET_SEALS failed (errno {}) — this fd is not a memfd", errno());
        return;
    }
    let names = [
        (F_SEAL_SEAL, "SEAL"),
        (F_SEAL_SHRINK, "SHRINK"),
        (F_SEAL_GROW, "GROW"),
        (F_SEAL_WRITE, "WRITE"),
    ];
    let set: Vec<&str> =
        names.iter().filter(|(b, _)| seals & b != 0).map(|(_, n)| *n).collect();
    println!("QEMU's memfd seals: 0x{seals:x} [{}]", set.join(" "));
    if seals & F_SEAL_SHRINK == 0 {
        println!("  no F_SEAL_SHRINK — udmabuf will refuse this memfd, and that is the answer");
    }

    // 2. Coalesce the located pages into runs, which is what the list wants.
    let mut runs: Vec<(u64, u64)> = Vec::new();
    for (_, off) in map {
        match runs.last_mut() {
            Some((start, len)) if *start + *len == *off => *len += rec::PAGE as u64,
            _ => runs.push((*off, rec::PAGE as u64)),
        }
    }
    let mut total = 0u64;
    let cap = limit_mb << 20;
    let mut items: Vec<UdmabufCreateItem> = Vec::new();
    for (off, len) in &runs {
        if total + len > cap {
            break;
        }
        total += len;
        items.push(UdmabufCreateItem { memfd: memfd as u32, pad: 0, offset: *off, size: *len });
    }
    println!(
        "located pages coalesce into {} run(s); offering the first {} ({} MiB) to udmabuf \
         (module size_limit_mb caps this)",
        runs.len(),
        items.len(),
        total >> 20
    );
    if items.is_empty() {
        println!("nothing to offer");
        return;
    }

    // 3. The ioctl. `udmabuf_create_list` is `{u32 flags; u32 count; item[]}`.
    let mut blob = Vec::with_capacity(8 + items.len() * std::mem::size_of::<UdmabufCreateItem>());
    blob.extend_from_slice(&UDMABUF_FLAGS_CLOEXEC.to_ne_bytes());
    blob.extend_from_slice(&(items.len() as u32).to_ne_bytes());
    for it in &items {
        blob.extend_from_slice(&it.memfd.to_ne_bytes());
        blob.extend_from_slice(&it.pad.to_ne_bytes());
        blob.extend_from_slice(&it.offset.to_ne_bytes());
        blob.extend_from_slice(&it.size.to_ne_bytes());
    }

    let dev = match OpenOptions::new().read(true).write(true).open("/dev/udmabuf") {
        Ok(f) => f,
        Err(e) => {
            println!("open /dev/udmabuf: {e} — this is the permission §0.7 recorded");
            return;
        }
    };
    println!("/dev/udmabuf: opened");
    let dmabuf = unsafe { ioctl(dev.as_raw_fd(), UDMABUF_CREATE_LIST, blob.as_ptr()) };
    if dmabuf < 0 {
        println!("UDMABUF_CREATE_LIST failed: errno {}", errno());
        return;
    }
    println!("UDMABUF_CREATE_LIST: got dma-buf fd {dmabuf}");

    // 4. The check that matters: are its bytes the same memory? A mapping that
    //    succeeds and shows different pages is the lavapipe failure, and only
    //    reading through both catches it.
    let pm = PageMap::new(&dev_file(dmabuf), 0, total as usize);
    match pm {
        Ok(m) => {
            let s = m.as_slice();
            let mut same = 0usize;
            let mut checked = 0usize;
            for (i, (idx, off)) in map.iter().enumerate() {
                let at = i * rec::PAGE;
                if at + rec::PAGE > s.len() {
                    break;
                }
                checked += 1;
                let page = &s[at..at + rec::PAGE];
                if rec::tag_at(page, rec::OFF_MAGIC) == *rec::MAGIC
                    && rec::get_u32(page, rec::OFF_PAGE_INDEX) == *idx
                {
                    same += 1;
                }
                let _ = off;
            }
            println!(
                "mapped the dma-buf: {same}/{checked} pages carry the guest's record at the \
                 index the map says — the dma-buf and the memfd are the same memory"
            );
        }
        Err(e) => println!("mmap of the dma-buf failed: {e}"),
    }
    // `dev_file` took ownership and has already closed it; closing again would
    // close whatever fd the number has since been reused for.
}

/// Wrap a raw fd as an owning `File`, so the dma-buf is closed when the
/// borrow that maps it goes out of scope. The mapping outlives the descriptor,
/// which is ordinary `mmap` behaviour and is also the point: **the fd is made
/// on this side and never leaves it** (design.md §1a).
fn dev_file(fd: i32) -> File {
    use std::os::unix::io::FromRawFd;
    unsafe { File::from_raw_fd(fd) }
}

