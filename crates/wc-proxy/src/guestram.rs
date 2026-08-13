//! The host half of a guest-RAM arena: take the plan `wc-arena-host` produced
//! and turn it into one contiguous, writable view of the guest's memory.
//!
//! STAGE 7. docs/design.md §10.13 item 1 in one module, and it is deliberately
//! the *small* half.
//!
//! # What this module is not allowed to contain, and why
//!
//! It contains **no guest-physical-address translation**. Not a formula, not a
//! table, not a parse of `info mtree -f`. That logic exists exactly once across
//! the two projects, in `shared_vulkan`'s `sv-hostd`, whose module header
//! explains why it is a table lookup rather than arithmetic:
//!
//! > reading the memfd offset as if it were the GPA lands on valid, zeroed,
//! > unrelated memory and reports no error whatsoever — a silent wrong answer,
//! > which is the worst thing this codebase can produce.
//!
//! A second implementation of that would be a second thing to get wrong, and
//! its being wrong would look exactly like working. So `crates/wc-arena-host`
//! links `sv-hostd` and does the discovery; what arrives here is the *result* —
//! a name to open and a list of runs — and this module opens and maps it.
//!
//! That split is not a compromise invented here. It is `sv-hostd`'s own
//! boundary, stated in its `qemu-serve` doc comment: it "hands **name + runs**
//! to the ICDs", each of which opens the name itself and does "one `mmap` per
//! run into one reservation". This proxy is one more such client.
//!
//! # Why a name rather than a descriptor, when both sides are on one host
//!
//! Because the rule is the rule (see the crate docs). The name here is
//! `/proc/<qemu-pid>/fd/<n>` — a same-machine name, and `shared_vulkan`'s
//! design.md §1a says exactly what that costs and that it may only ever be
//! given to the host side, which is where every reader of it is. Nothing about
//! this crosses into the guest: the guest allocated the memory, stamped it, and
//! said a 64-bit number.
//!
//! # `unsafe`
//!
//! Reserving a `PROT_NONE` range and `mmap(MAP_FIXED)`ing runs into it has no
//! safe spelling, and neither does handing the result out as a slice. Both are
//! confined to this file.

use std::io;
use std::path::{Path, PathBuf};

/// One run of the arena: `[arena_offset, +len)` of the region the guest thinks
/// it has is `[file_offset, +len)` of QEMU's memory backend.
///
/// The guest's pages are scattered — `shared_vulkan` measured 1471 runs per
/// 64 MiB for `VirtualLock`ed 4 KiB pages, and 22 for large pages — so the
/// arena is contiguous only after this list has been applied.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Run {
    pub arena_offset: u64,
    pub file_offset: u64,
    pub len: u64,
}

/// What `wc-arena-host` found, as a file.
///
/// A file rather than a pipe or a socket because the two programs run at
/// different privileges: the locator needs to read `/proc/<qemu-pid>/fd`, and
/// what it produces is a small, inspectable, greppable artefact that a human
/// can read when a run goes wrong. It is also the record of what was mapped,
/// which is the first thing anyone will want after a frozen window.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    /// Which region of the arena family this is — the number that travels in
    /// `Descriptor::ArenaSlice { arena, .. }` and names the mapping a slice
    /// lives in.
    ///
    /// DYNAMIC ARENA. A plan used to describe exactly one region because an
    /// arena was committed once and never grew (design.md §5.7 D22: "the arena
    /// is committed at `sv-gpu-server` startup and does not grow, on either
    /// path"). Growth makes a plan a description of *n* regions, and each one
    /// needs a name, because the two are separate `VirtualAlloc`s in the guest
    /// located at unrelated ranges of QEMU's backing object and mapped at
    /// unrelated host addresses. There is no offset arithmetic that reaches
    /// from one into the other, which is exactly why the region id is part of
    /// every descriptor rather than a thing the host infers.
    ///
    /// **It is an integer, and that is load-bearing.** design.md §1a: no
    /// descriptor crosses. A region id names a mapping the way an arena id
    /// always did (waycompose §17.2: "an arena id names a region, not a
    /// client"); making growth work by handing over a second *object* would be
    /// the one thing this design may not do.
    pub id: u32,
    /// The object to open, normally `/proc/<qemu-pid>/fd/<n>`.
    pub object: PathBuf,
    /// The arena's length. Must equal the sum of the runs' lengths.
    pub len: u64,
    /// The run id the guest stamped, carried only so a log line can say it.
    pub run_id: u64,
    /// The guest compositor's **outbound window**, `(offset, len)`, if it
    /// declared one.
    ///
    /// STAGE 10. These bytes are the guest's to hand to clients — a keymap, a
    /// clipboard offer — and this side's pool allocator must not hand them out
    /// as well. It travels in the plan rather than in a flag on both programs
    /// because the number is *decided* in the guest, by
    /// `wc_handle::arena::reserve_outbound`, and the plan is already the
    /// description of the region that crossed from there to here. A number
    /// typed into two command lines is a number that can differ between them,
    /// and the symptom of its differing is two allocators writing the same
    /// bytes — pixels in a keymap, which presents as a client dying inside
    /// libxkbcommon a long way from the flag.
    pub outbound: Option<(u64, u64)>,
    pub runs: Vec<Run>,
}

impl Plan {
    /// Parse a plan that describes exactly one region.
    ///
    /// The pre-growth spelling, kept because it is what every existing caller
    /// and every existing harness writes. A plan naming more than one region is
    /// refused here rather than silently truncated to its first: a caller that
    /// asked for one region and was handed the first of four would map a
    /// quarter of the arena and allocate confidently into memory the guest
    /// believes is somewhere else.
    pub fn read(path: &Path) -> io::Result<Plan> {
        let mut all = Plan::read_all(path)?;
        if all.len() != 1 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "{}: describes {} regions and this caller can hold one; \
                     use Plan::read_all",
                    path.display(),
                    all.len()
                ),
            ));
        }
        Ok(all.remove(0))
    }

    /// Parse a plan describing one **or more** regions.
    ///
    /// Whitespace-separated words, one record per line, `#` for a comment — the
    /// format is dull on purpose, because the interesting failure is a plan
    /// from a *previous* VM boot rather than a plan that is hard to parse.
    ///
    /// # The grammar, and why growth did not need a new file
    ///
    /// A `region <id>` line opens a new region record; every `object`, `len`,
    /// `run`, `outbound` and `r` line after it belongs to that region. A file
    /// with no `region` line at all is one region with id 0, which is exactly
    /// the pre-growth format — so every plan ever written by `wc-arena-host`
    /// still reads, and reads to the same thing.
    ///
    /// Appending rather than replacing is the point. The host locates a new
    /// region while the old ones are live and full of pixels; rewriting the
    /// file from scratch would mean re-locating regions whose stamps the first
    /// pool overwrote (§12.10), which is the thing the content sweep
    /// structurally cannot do. A plan is therefore a log of regions, and the
    /// proxy re-reads it to find the ones it has not mapped yet.
    pub fn read_all(path: &Path) -> io::Result<Vec<Plan>> {
        let text = std::fs::read_to_string(path)?;
        let bad = |m: String| io::Error::new(io::ErrorKind::InvalidData, m);

        // One partially built region. `None` for `id` marks "no `region` line
        // has been seen", which is what makes the old format the new format's
        // one-element case rather than a second parser.
        struct Partial {
            id: Option<u32>,
            object: PathBuf,
            len: u64,
            run_id: u64,
            outbound: Option<(u64, u64)>,
            runs: Vec<Run>,
        }
        impl Partial {
            fn new(id: Option<u32>) -> Partial {
                Partial {
                    id,
                    object: PathBuf::new(),
                    len: 0,
                    run_id: 0,
                    outbound: None,
                    runs: Vec::new(),
                }
            }
            fn is_empty(&self) -> bool {
                self.object.as_os_str().is_empty() && self.len == 0 && self.runs.is_empty()
            }
        }

        let mut cur = Partial::new(None);
        let mut done: Vec<Partial> = Vec::new();

        for (n, line) in text.lines().enumerate() {
            let line = line.split('#').next().unwrap_or("").trim();
            if line.is_empty() {
                continue;
            }
            let f: Vec<&str> = line.split_whitespace().collect();
            // Hex where it is written as hex. The run id is 64 unguessable bits
            // and is only ever readable as `0x…`; the offsets are whichever the
            // locator found clearer, so both spellings are accepted.
            let num = |s: &str| -> io::Result<u64> {
                match s.strip_prefix("0x") {
                    Some(h) => u64::from_str_radix(h, 16),
                    None => s.parse::<u64>(),
                }
                .map_err(|e| bad(format!("{}:{}: {s:?}: {e}", path.display(), n + 1)))
            };
            match f.as_slice() {
                ["region", v] => {
                    let id = u32::try_from(num(v)?).map_err(|_| {
                        bad(format!("{}:{}: region id {v:?} is not a u32", path.display(), n + 1))
                    })?;
                    if !cur.is_empty() {
                        done.push(std::mem::replace(&mut cur, Partial::new(Some(id))));
                    } else {
                        cur.id = Some(id);
                    }
                }
                ["object", v] => cur.object = PathBuf::from(v),
                ["len", v] => cur.len = num(v)?,
                ["run", v] => cur.run_id = num(v)?,
                ["outbound", o, l] => cur.outbound = Some((num(o)?, num(l)?)),
                ["r", a, o, l] => cur.runs.push(Run {
                    arena_offset: num(a)?,
                    file_offset: num(o)?,
                    len: num(l)?,
                }),
                _ => return Err(bad(format!("{}:{}: cannot read {line:?}", path.display(), n + 1))),
            }
        }
        if !cur.is_empty() {
            done.push(cur);
        }
        if done.is_empty() {
            return Err(bad(format!("{}: no object, no length or no runs", path.display())));
        }

        let mut out = Vec::with_capacity(done.len());
        for (i, p) in done.into_iter().enumerate() {
            let id = p.id.unwrap_or(0);
            let (len, runs, outbound) = (p.len, p.runs, p.outbound);
            let where_ = format!("{} region {id}", path.display());
            if p.object.as_os_str().is_empty() || len == 0 || runs.is_empty() {
                return Err(bad(format!("{where_}: no object, no length or no runs")));
            }
            // The one consistency check worth making here, because the
            // alternative is an arena with a hole in it that reads as zeroes: a
            // pool allocated into the hole would be silently invisible to the
            // compositor, which is §10.7's symptom arriving by a different road.
            let covered: u64 = runs.iter().map(|r| r.len).sum();
            if covered != len {
                return Err(bad(format!(
                    "{where_}: the runs cover {covered} bytes of a {len}-byte arena"
                )));
            }
            // The window has to be inside the region, and it has to be the tail
            // — which is what `reserve_outbound` produces and what this side's
            // allocator can express by simply being shorter. A window in the
            // middle would need a hole in the free list, and refusing here is
            // better than silently allocating around only part of one.
            if let Some((off, wlen)) = outbound {
                if off.checked_add(wlen) != Some(len) {
                    return Err(bad(format!(
                        "{where_}: an outbound window at {off}+{wlen} is not the tail \
                         of a {len}-byte arena"
                    )));
                }
                if wlen >= len {
                    return Err(bad(format!(
                        "{where_}: an outbound window of {wlen} bytes leaves nothing \
                         of a {len}-byte arena"
                    )));
                }
            }
            // Two regions with one id would be two mappings a descriptor cannot
            // tell apart, and a slice naming that id would land in whichever the
            // proxy happened to map second. Refused where it is cheap, for the
            // same reason `sv-hostd` refuses a duplicate page index rather than
            // choosing between two claimants.
            if out.iter().any(|q: &Plan| q.id == id) {
                return Err(bad(format!(
                    "{}: region {id} is named twice; a region id names a mapping and \
                     two mappings cannot share one",
                    path.display()
                )));
            }
            debug_assert_eq!(i, out.len());
            out.push(Plan { id, object: p.object, len, run_id: p.run_id, outbound, runs });
        }
        Ok(out)
    }
}

/// The guest's arena, scattered through QEMU's backing object, stitched into
/// one address range.
pub struct StitchedRegion {
    base: *mut u8,
    len: usize,
    runs: usize,
    origin: String,
}

// SAFETY: the mapping is owned solely by this struct, and moving it between
// threads moves the whole reservation. It is shared memory — the guest writes
// it too — which is a `wl_shm` hazard rather than a Rust one, and is the same
// hazard `wc_handle::Mapping` documents.
unsafe impl Send for StitchedRegion {}

impl StitchedRegion {
    /// Open the named object and map every run of the plan into one reservation.
    ///
    /// **`MAP_SHARED`, and writable.** This process is the *sender*: it copies a
    /// client's pixels into the arena and the compositor reads them out. A
    /// private mapping here would produce a proxy that works perfectly and a
    /// compositor that sees nothing, which is the most expensive possible bug
    /// in this file and is one flag away.
    pub fn attach(plan: &Plan) -> io::Result<StitchedRegion> {
        use std::os::fd::AsRawFd;

        let len = usize::try_from(plan.len)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "arena too large"))?;
        let file = std::fs::OpenOptions::new().read(true).write(true).open(&plan.object)?;

        // One reservation first, so the runs land next to each other and the
        // arena's offsets mean something. `PROT_NONE` so that a hole — a run
        // list that does not cover the whole arena — faults loudly instead of
        // reading as zeroes. `Plan::read` refuses such a list, and this is the
        // second line of the same defence.
        // SAFETY: a fresh anonymous reservation at an address of the kernel's
        // choosing; the result is checked.
        let base = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_NONE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | libc::MAP_NORESERVE,
                -1,
                0,
            )
        };
        if base == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }
        let region = StitchedRegion {
            base: base.cast::<u8>(),
            len,
            runs: plan.runs.len(),
            origin: plan.object.display().to_string(),
        };
        // Owns the reservation from here, so every early return unmaps it.
        for (i, r) in plan.runs.iter().enumerate() {
            let (off, rlen) = (
                usize::try_from(r.arena_offset).unwrap_or(usize::MAX),
                usize::try_from(r.len).unwrap_or(usize::MAX),
            );
            if off.checked_add(rlen).map(|e| e > len).unwrap_or(true) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("run {i} at {off}+{rlen} runs past the end of a {len}-byte arena"),
                ));
            }
            // SAFETY: `base + off` is inside the reservation this struct owns,
            // `MAP_FIXED` replaces exactly that part of it, and the descriptor
            // is live for the duration of the call.
            let got = unsafe {
                libc::mmap(
                    region.base.add(off).cast(),
                    rlen,
                    libc::PROT_READ | libc::PROT_WRITE,
                    libc::MAP_SHARED | libc::MAP_FIXED,
                    file.as_raw_fd(),
                    r.file_offset as libc::off_t,
                )
            };
            if got == libc::MAP_FAILED {
                let e = io::Error::last_os_error();
                return Err(io::Error::new(
                    e.kind(),
                    format!("mapping run {i} (file offset {}, {rlen} bytes): {e}", r.file_offset),
                ));
            }
        }
        Ok(region)
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// How many `mmap(MAP_FIXED)` calls it took, which is the guest allocator's
    /// fragmentation as a single number.
    pub fn runs(&self) -> usize {
        self.runs
    }

    pub fn origin(&self) -> &str {
        &self.origin
    }

    /// The same bytes, for reading. STAGE 10: the guest's outbound window is
    /// read from here while the pool path holds the writable half.
    pub fn as_slice(&self) -> &[u8] {
        // SAFETY: as `as_mut_slice`, with `&self`, which rules out a `&mut`
        // through this struct existing at the same time. The guest writes these
        // bytes concurrently, which is the point of the region.
        unsafe { std::slice::from_raw_parts(self.base.cast_const(), self.len) }
    }

    pub fn as_mut_slice(&mut self) -> &mut [u8] {
        // SAFETY: the whole reservation is mapped read-write by `attach`, which
        // refuses a plan that does not cover it, and `&mut self` is the only
        // way to reach it from this process.
        unsafe { std::slice::from_raw_parts_mut(self.base, self.len) }
    }
}

impl Drop for StitchedRegion {
    fn drop(&mut self) {
        if self.len > 0 {
            // SAFETY: one `munmap` of exactly the reservation `attach` made.
            // `MAP_FIXED` runs inside it are torn down by the same call.
            unsafe {
                libc::munmap(self.base.cast(), self.len);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write_plan(body: &str) -> (tempdir::Guard, PathBuf) {
        let dir = tempdir::Guard::new();
        let p = dir.path().join("plan");
        let mut f = std::fs::File::create(&p).unwrap();
        f.write_all(body.as_bytes()).unwrap();
        (dir, p)
    }

    /// A directory that goes away with the test. Not worth a dependency.
    mod tempdir {
        use std::path::{Path, PathBuf};
        pub struct Guard(PathBuf);
        impl Guard {
            pub fn new() -> Guard {
                let p = std::env::temp_dir().join(format!(
                    "wc-guestram-{}-{:?}",
                    std::process::id(),
                    std::thread::current().id()
                ));
                let _ = std::fs::remove_dir_all(&p);
                std::fs::create_dir_all(&p).unwrap();
                Guard(p)
            }
            pub fn path(&self) -> &Path {
                &self.0
            }
        }
        impl Drop for Guard {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
    }

    #[test]
    fn a_plan_round_trips() {
        let (_d, p) = write_plan(
            "# wc-arena-host plan\nobject /proc/1/fd/9\nlen 8192\nrun 0xdeadbeef\n\
             r 0 0x1000 4096\nr 4096 0x9000 4096\n",
        );
        let plan = Plan::read(&p).unwrap();
        assert_eq!(plan.object, PathBuf::from("/proc/1/fd/9"));
        assert_eq!(plan.len, 8192);
        assert_eq!(plan.run_id, 0xdead_beef);
        assert_eq!(plan.runs.len(), 2);
        assert_eq!(plan.runs[1], Run { arena_offset: 4096, file_offset: 0x9000, len: 4096 });
    }

    /// The check that matters: a plan whose runs do not cover the arena would
    /// leave a `PROT_NONE` hole, and a pool allocated into it would be pixels
    /// nobody ever sees. Refused where it is cheap to refuse.
    #[test]
    fn a_plan_with_a_hole_is_refused() {
        let (_d, p) = write_plan("object /proc/1/fd/9\nlen 8192\nr 0 0x1000 4096\n");
        let e = Plan::read(&p).unwrap_err();
        assert!(e.to_string().contains("4096 bytes of a 8192-byte arena"), "{e}");
    }

    /// STAGE 10. The guest compositor's window, carried in the plan so that it
    /// has one origin. The two refusals matter more than the parse: a window
    /// that is not the tail cannot be expressed by shortening a free list, and a
    /// window that is the whole arena leaves the pool allocator nothing — and
    /// both would otherwise be discovered as a pool allocated on top of a live
    /// keymap.
    #[test]
    fn an_outbound_window_is_read_and_must_be_the_tail() {
        let (_d, p) = write_plan(
            "object /proc/1/fd/9\nlen 8192\nrun 0x1\noutbound 4096 4096\nr 0 0x0 8192\n",
        );
        assert_eq!(Plan::read(&p).unwrap().outbound, Some((4096, 4096)));

        let (_d2, p2) = write_plan(
            "object /proc/1/fd/9\nlen 8192\nrun 0x1\noutbound 0 4096\nr 0 0x0 8192\n",
        );
        let e = Plan::read(&p2).unwrap_err();
        assert!(e.to_string().contains("is not the tail"), "{e}");

        let (_d3, p3) = write_plan(
            "object /proc/1/fd/9\nlen 8192\nrun 0x1\noutbound 0 8192\nr 0 0x0 8192\n",
        );
        assert!(Plan::read(&p3).is_err(), "a window of the whole arena must be refused");

        // And a plan without one is the pre-stage-10 plan, still read.
        let (_d4, p4) = write_plan("object /proc/1/fd/9\nlen 8192\nrun 0x1\nr 0 0x0 8192\n");
        assert_eq!(Plan::read(&p4).unwrap().outbound, None);
    }

    #[test]
    fn an_empty_plan_is_refused() {
        let (_d, p) = write_plan("# nothing here\n");
        assert!(Plan::read(&p).is_err());
    }

    /// DYNAMIC ARENA. Every plan `wc-arena-host` has ever written has no
    /// `region` line, and must go on meaning what it meant. The old format is
    /// the new format's one-element case rather than a second parser, and this
    /// is the test that says so.
    #[test]
    fn a_plan_without_a_region_line_is_one_region_numbered_zero() {
        let (_d, p) = write_plan(
            "object /proc/1/fd/9\nlen 8192\nrun 0xdeadbeef\nr 0 0x1000 4096\nr 4096 0x9000 4096\n",
        );
        let all = Plan::read_all(&p).unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].id, 0);
        assert_eq!(all[0].len, 8192);
        // And the single-region reader still answers, byte for byte.
        assert_eq!(Plan::read(&p).unwrap(), all[0]);
    }

    /// The growth format. Two regions, each with its own object, length, run id
    /// and run list — because they are two separate `VirtualAlloc`s in the
    /// guest, located at two unrelated ranges of QEMU's backing object, and
    /// nothing about the second is derivable from the first.
    #[test]
    fn a_plan_may_describe_several_regions_each_with_its_own_runs() {
        let (_d, p) = write_plan(
            "# grown twice\n\
             region 0\nobject /proc/1/fd/9\nlen 8192\nrun 0xaaa\nr 0 0x1000 8192\n\
             region 1\nobject /proc/1/fd/9\nlen 4096\nrun 0xbbb\nr 0 0x40000 4096\n\
             region 7\nobject /proc/1/fd/11\nlen 4096\nrun 0xccc\nr 0 0x0 4096\n",
        );
        let all = Plan::read_all(&p).unwrap();
        assert_eq!(all.len(), 3);
        assert_eq!(all.iter().map(|r| r.id).collect::<Vec<_>>(), vec![0, 1, 7]);
        assert_eq!(all[1].run_id, 0xbbb);
        assert_eq!(all[2].object, PathBuf::from("/proc/1/fd/11"));
        // Ids need not be dense: a released region's id is not reused, because
        // a client may still name it in a request already in flight.
        assert_eq!(all[2].len, 4096);

        // And the single-region reader refuses it rather than quietly taking
        // the first of three.
        let e = Plan::read(&p).unwrap_err();
        assert!(e.to_string().contains("describes 3 regions"), "{e}");
    }

    /// Two regions with one id would be two mappings a descriptor cannot tell
    /// apart. `Descriptor::ArenaSlice { arena, offset, len }` names the region
    /// by that number and nothing else, so a duplicate would put a slice in
    /// whichever mapping the proxy happened to attach second — the silent wrong
    /// answer this project spends most of its checks avoiding.
    #[test]
    fn two_regions_may_not_share_an_id() {
        let (_d, p) = write_plan(
            "region 3\nobject /proc/1/fd/9\nlen 4096\nrun 0x1\nr 0 0x0 4096\n\
             region 3\nobject /proc/1/fd/9\nlen 4096\nrun 0x2\nr 0 0x1000 4096\n",
        );
        let e = Plan::read_all(&p).unwrap_err();
        assert!(e.to_string().contains("region 3 is named twice"), "{e}");
    }

    /// Each region carries its own outbound window and its own hole check, and
    /// the message says which region was wrong. A plan with four regions whose
    /// error message did not name one would be a plan a human cannot fix.
    #[test]
    fn a_hole_is_refused_per_region_and_the_message_names_it() {
        let (_d, p) = write_plan(
            "region 0\nobject /proc/1/fd/9\nlen 4096\nrun 0x1\nr 0 0x0 4096\n\
             region 5\nobject /proc/1/fd/9\nlen 8192\nrun 0x2\nr 0 0x1000 4096\n",
        );
        let e = Plan::read_all(&p).unwrap_err();
        assert!(e.to_string().contains("region 5"), "{e}");
        assert!(e.to_string().contains("cover 4096 bytes of a 8192-byte arena"), "{e}");
    }

    /// The guest's own memory is a perfectly good stand-in for the guest's
    /// memory *as QEMU sees it*, for the one property this type owns: that a
    /// scattered run list becomes one contiguous view in the right order. Two
    /// runs taken from the file back to front prove the order is the plan's and
    /// not the file's.
    #[test]
    fn runs_are_stitched_in_arena_order_not_file_order() {
        let dir = tempdir::Guard::new();
        let backing = dir.path().join("backing");
        std::fs::write(&backing, {
            let mut v = vec![0u8; 8192];
            v[0..4].copy_from_slice(b"SCND");
            v[4096..4100].copy_from_slice(b"FRST");
            v
        })
        .unwrap();
        let plan = Plan {
            id: 0,
            object: backing,
            len: 8192,
            run_id: 1,
            outbound: None,
            runs: vec![
                Run { arena_offset: 0, file_offset: 4096, len: 4096 },
                Run { arena_offset: 4096, file_offset: 0, len: 4096 },
            ],
        };
        let mut region = StitchedRegion::attach(&plan).unwrap();
        assert_eq!(region.runs(), 2);
        assert_eq!(&region.as_mut_slice()[0..4], b"FRST");
        assert_eq!(&region.as_mut_slice()[4096..4100], b"SCND");
        // And a write goes back to the object, which is the direction the whole
        // transport depends on.
        region.as_mut_slice()[4] = b'!';
        drop(region);
        assert_eq!(std::fs::read(&plan.object).unwrap()[4100], b'!');
    }
}
