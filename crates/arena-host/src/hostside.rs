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
use std::ffi::c_void;
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::fs::FileExt;
use std::os::unix::io::AsRawFd;

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
             arena-host write1 --fd <path> --map <file> --stamp <hex>\n  \
             arena-host verify --fd <path> --map <file> [--stamp <hex>] [--vkstamp <hex>]\n  \
             arena-host write2 --fd <path> --map <file> --stamp <hex>"
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
        other => panic!("unknown command {other}"),
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
