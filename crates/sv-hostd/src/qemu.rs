//! `Mode::Qemu`: find the guest's RAM, and learn how guest-physical addresses
//! map into the backing object.
//!
//! docs/design.md §5.1 is unusually firm about this module, and the reason is a
//! measured failure mode rather than taste:
//!
//! > **`sv-hostd` parses QEMU's flat view (`info mtree -f`) into a region table
//! > and looks addresses up in it. It does not evaluate a formula, and there is
//! > no compiled-in default that is trusted without being re-read.** … reading
//! > the memfd offset as if it were the GPA lands on valid, zeroed, unrelated
//! > memory and reports no error whatsoever — a silent wrong answer, which is
//! > the worst thing this codebase can produce.
//!
//! Three specific things on the target machine defeat arithmetic, and all three
//! are in the fixture the tests run against:
//!
//! - the below-4 GiB split is **3 GiB**, not the 3.5 GiB an i440fx uses at
//!   smaller RAM sizes, so `gpa - 0x4000_0000` above 4 GiB is right *here* and
//!   would be 512 MiB wrong on the same machine type with less RAM;
//! - `0xc0000-0xfffff` is the ROM window and is **not** the backend at its
//!   identity offset;
//! - `0x103000-0x10efff` is twelve Hyper-V SynIC message pages overlaying the
//!   backend, so guest RAM is not even contiguous below 1 MiB.
//!
//! A table lookup survives all three. A formula survives none of them, and
//! fails silently on each.
//!
//! # What is discovered rather than configured
//!
//! Everything except the QMP socket path:
//!
//! | thing | how |
//! |---|---|
//! | QEMU's pid | the process holding the QMP socket, or a name match |
//! | the backend's id | `query-memdev` over QMP |
//! | the backend's fd | the `/proc/<pid>/fd` entry whose link names the memfd |
//! | the GPA↔offset table | `info mtree -f` over QMP |
//!
//! The alternative — remembering `/proc/205866/fd/34` — is wrong after every VM
//! start, and a stale fd number reads *another process's memory* without an
//! error. That is R21's shape again and it is worth the discovery code.

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

/// One RAM alias in the guest's flat view: `[gpa, gpa+len)` of guest-physical
/// address space is `[file_offset, file_offset+len)` of the backing object.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RamAlias {
    pub gpa: u64,
    pub len: u64,
    pub file_offset: u64,
}

/// The guest's RAM, as QEMU says it is laid out.
///
/// Sorted by `file_offset` as well as by `gpa`, because this table is used in
/// both directions: `gpa_to_offset` when a frame list is available (the AWE
/// path), and `offset_to_gpa` when reporting what the scan found, which is how
/// a human checks a result against QEMU's own `xp`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RegionTable {
    by_gpa: Vec<RamAlias>,
}

impl RegionTable {
    pub fn new(mut aliases: Vec<RamAlias>) -> RegionTable {
        aliases.sort_by_key(|a| a.gpa);
        RegionTable { by_gpa: aliases }
    }

    pub fn aliases(&self) -> &[RamAlias] {
        &self.by_gpa
    }

    pub fn is_empty(&self) -> bool {
        self.by_gpa.is_empty()
    }

    /// Total guest RAM the table accounts for. The scan's cost is linear in
    /// this and in nothing else (§5.7), so it is worth reporting.
    pub fn total_ram(&self) -> u64 {
        self.by_gpa.iter().map(|a| a.len).sum()
    }

    /// `None` for an address that is not RAM — the PCI hole, the ROM window,
    /// a SynIC page. Returning `None` rather than a plausible number is the
    /// whole point of the type.
    pub fn gpa_to_offset(&self, gpa: u64) -> Option<u64> {
        self.by_gpa
            .iter()
            .find(|a| gpa >= a.gpa && gpa - a.gpa < a.len)
            .map(|a| a.file_offset + (gpa - a.gpa))
    }

    /// The inverse. Used for reporting, and for the `xp` cross-check that
    /// caught R21 in the first place.
    pub fn offset_to_gpa(&self, off: u64) -> Option<u64> {
        self.by_gpa
            .iter()
            .find(|a| off >= a.file_offset && off - a.file_offset < a.len)
            .map(|a| a.gpa + (off - a.file_offset))
    }

    /// Is this file offset inside RAM the guest can see at all?
    ///
    /// The scan uses it to skip regions of the backing object that are not
    /// mapped into the guest. There are such regions: the backend is sized to
    /// the guest's RAM but the flat view maps it in pieces, and the holes
    /// between the pieces are never written by the guest — so a stamp found
    /// there would be a stale one from a previous boot.
    pub fn is_ram_offset(&self, off: u64) -> bool {
        self.offset_to_gpa(off).is_some()
    }
}

/// Parse `info mtree -f` into the RAM aliases of one memory backend.
///
/// The lines that matter look like this, and the three shapes below are all
/// taken verbatim from the target machine:
///
/// ```text
///   0000000000000000-00000000000bffff (prio 0, ram): virtiofsmem KVM
///   0000000000100000-0000000000102fff (prio 0, ram): virtiofsmem @0000000000100000 KVM
///   0000000100000000-000000037fffffff (prio 0, ram): virtiofsmem @00000000c0000000 KVM
/// ```
///
/// Four rules, each of which a naive parser gets wrong:
///
/// 1. **`@offset` is optional**, and its absence means offset 0 — not "same as
///    the GPA". The first line above is both, which is exactly why it is
///    ambiguous to a reader and must not be to the parser.
/// 2. **The end address is inclusive**, so the length is `end - start + 1`. An
///    exclusive reading loses the last page of every alias, which on the
///    2.9 GiB alias is a page 3 GiB into the arena — found by nothing until it
///    is the page an application wanted.
/// 3. **Only `ram` entries count.** `rom`, `romd` and `i/o` share the format
///    and are not memory we may write.
/// 4. **Only entries whose region name is the backend's id count.** The
///    `synic-*-msg-page` regions sit inside the backend's GPA span and are
///    different memory; taking them would make the table claim RAM that is not
///    the backing object.
///
/// **Only the system address space is parsed**, and that is a correction rather
/// than a refinement.
///
/// This function used to take every matching line in the dump, with the comment
/// "aliases of one backend appear only in the system flat view in practice, and
/// filtering by name is both simpler and stricter than tracking which
/// `FlatView` block we are inside". That is true of `testhost`, whose guest
/// runs with `smm=off`. It is false of a guest with SMM: **every alias appeared
/// exactly twice**, once in the `memory` flat view and once in the SMM one, and
/// the symptom was a 2 GiB guest reporting 4095 MiB of RAM.
///
/// It was benign in that run only because the duplicates were identical, so
/// `gpa_to_offset` took the first and got the right answer. It would not stay
/// benign: SMRAM legitimately aliases the same backend at *different* guest
/// addresses, and a table holding both would answer `offset_to_gpa` with
/// whichever it happened to sort first. That is R21's silent-wrong-answer shape
/// arriving through a different door.
///
/// So the parser tracks the `FlatView` blocks and takes the one whose address
/// space list contains `"memory"` — the system view, which is the one an
/// application's memory actually lives in.
pub fn parse_flat_view(text: &str, backend_id: &str) -> RegionTable {
    let mut out = Vec::new();
    // Which FlatView block we are inside, and whether it is the system one.
    // `AS "memory"` marks it; a block may list many address spaces before its
    // first region line, which is why this is a flag rather than a match on the
    // immediately preceding line.
    let mut in_system = false;
    let mut seen_any_flatview = false;
    for line in text.lines() {
        let t = line.trim_end_matches('\r').trim_start();
        if t.starts_with("FlatView") {
            seen_any_flatview = true;
            in_system = false;
            continue;
        }
        if t.starts_with("AS \"memory\"") {
            in_system = true;
            continue;
        }
        // A dump with no `FlatView` header at all is a fragment — the test
        // fixtures are exactly that — and filtering it to nothing would make
        // every one of them silently pass. Treat "no blocks seen" as "the whole
        // text is the system view".
        if seen_any_flatview && !in_system {
            continue;
        }
        let line = t;
        let line = line.trim_end_matches('\r').trim();
        let Some((range, rest)) = line.split_once(' ') else { continue };
        let Some((start, end)) = range.split_once('-') else { continue };
        let (Ok(start), Ok(end)) = (u64::from_str_radix(start, 16), u64::from_str_radix(end, 16))
        else {
            continue;
        };
        // "(prio 0, ram): name …" — the kind is the last word before the colon.
        let Some((attrs, name_part)) = rest.split_once("): ") else { continue };
        if !attrs.ends_with(", ram") && !attrs.ends_with(",ram") {
            continue;
        }
        let mut words = name_part.split_whitespace();
        let Some(name) = words.next() else { continue };
        if name != backend_id {
            continue;
        }
        let file_offset = match words.next() {
            Some(w) if w.starts_with('@') => match u64::from_str_radix(&w[1..], 16) {
                Ok(v) => v,
                // A malformed `@` is not something to guess past: the whole
                // point of this module is that a wrong offset is silent.
                Err(_) => continue,
            },
            _ => 0,
        };
        if end < start {
            continue;
        }
        let alias = RamAlias { gpa: start, len: end - start + 1, file_offset };
        // Belt and braces: even inside one flat view an alias can be listed
        // twice (the `cpu-memory-N` spaces share the system root). Identical
        // triples are the same memory described twice and must not be counted
        // twice — `total_ram()` is reported to a human and a doubled figure is
        // the kind of number nobody questions.
        if !out.contains(&alias) {
            out.push(alias);
        }
    }
    RegionTable::new(out)
}

// ---------------------------------------------------------------------------
// QMP
// ---------------------------------------------------------------------------

/// A QMP connection, reduced to the two questions this daemon asks.
///
/// Hand-rolled rather than `serde_json`, and the reason is scope rather than
/// pride: the two replies we read are a flat view (one long string) and a list
/// of memory backends (id and size). Both are extracted by locating a key and
/// reading the value after it, which is ~40 lines and no dependency. If this
/// ever needs `query-migrate` or anything with nesting, take the dependency.
pub struct Qmp {
    sock: std::os::unix::net::UnixStream,
    reader: BufReader<std::os::unix::net::UnixStream>,
}

impl Qmp {
    /// Connect and complete the capability negotiation, which QMP requires
    /// before it will answer anything.
    pub fn connect(path: &Path) -> std::io::Result<Qmp> {
        let sock = std::os::unix::net::UnixStream::connect(path)?;
        sock.set_read_timeout(Some(std::time::Duration::from_secs(15)))?;
        let reader = BufReader::new(sock.try_clone()?);
        let mut q = Qmp { sock, reader };
        // The greeting.
        q.read_line()?;
        q.command("{\"execute\":\"qmp_capabilities\"}")?;
        Ok(q)
    }

    fn read_line(&mut self) -> std::io::Result<String> {
        let mut s = String::new();
        let n = self.reader.read_line(&mut s)?;
        if n == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "QMP closed the connection",
            ));
        }
        Ok(s)
    }

    /// Send one command and return the first reply that is not an event.
    ///
    /// QMP interleaves asynchronous events with replies, and a reader that does
    /// not skip them attributes the next event to this command. On a machine
    /// running a Windows guest events arrive constantly (RTC, balloon), so this
    /// is not a rare case.
    pub fn command(&mut self, json: &str) -> std::io::Result<String> {
        self.sock.write_all(json.as_bytes())?;
        self.sock.write_all(b"\n")?;
        self.sock.flush()?;
        for _ in 0..256 {
            let line = self.read_line()?;
            if line.contains("\"event\"") {
                continue;
            }
            if line.contains("\"error\"") {
                return Err(std::io::Error::other(format!("QMP error: {}", line.trim())));
            }
            return Ok(line);
        }
        Err(std::io::Error::other("QMP produced only events"))
    }

    /// `info mtree -f`, through the human monitor. There is no structured QMP
    /// command that reports the flat view; `query-memory-map` does not exist and
    /// `query-memdev` reports the backends without their guest addresses.
    pub fn flat_view(&mut self) -> std::io::Result<String> {
        let reply = self.command(
            "{\"execute\":\"human-monitor-command\",\
             \"arguments\":{\"command-line\":\"info mtree -f\"}}",
        )?;
        Ok(unescape_json_string_after(&reply, "\"return\":").unwrap_or_default())
    }

    /// The memory backends' ids, so the backend is discovered rather than
    /// configured.
    ///
    /// Only the ids are extracted. The size is in the reply and is not used:
    /// which backend to take is decided by which one actually has RAM aliases
    /// in the flat view, which is a stronger test than "the biggest one" and
    /// does not depend on field order in a hand-parsed reply. (QEMU emits
    /// `"size"` before `"id"` within each object, so pairing them positionally
    /// is exactly the kind of thing that works until it does not.)
    pub fn memdevs(&mut self) -> std::io::Result<Vec<String>> {
        let reply = self.command("{\"execute\":\"query-memdev\"}")?;
        let mut out = Vec::new();
        let mut rest = reply.as_str();
        while let Some(i) = rest.find("\"id\":") {
            rest = &rest[i + 5..];
            if let Some(id) = json_string(rest) {
                out.push(id);
            }
        }
        Ok(out)
    }
}

fn json_string(after: &str) -> Option<String> {
    let s = after.trim_start();
    let s = s.strip_prefix('"')?;
    let end = s.find('"')?;
    Some(s[..end].to_string())
}

/// Pull the JSON string value that follows `key` and undo the escaping.
///
/// The flat view is megabytes of text arriving as one JSON string, so this is
/// the one place the hand-rolled parsing earns itself: it is a single pass with
/// no allocation per line.
fn unescape_json_string_after(text: &str, key: &str) -> Option<String> {
    let i = text.find(key)? + key.len();
    let s = text[i..].trim_start().strip_prefix('"')?;
    let mut out = String::with_capacity(s.len());
    let mut it = s.chars();
    while let Some(c) = it.next() {
        match c {
            '"' => return Some(out),
            '\\' => match it.next() {
                Some('n') => out.push('\n'),
                Some('r') => out.push('\r'),
                Some('t') => out.push('\t'),
                Some('\\') => out.push('\\'),
                Some('"') => out.push('"'),
                Some('u') => {
                    let hex: String = it.by_ref().take(4).collect();
                    if let Some(c) = u32::from_str_radix(&hex, 16).ok().and_then(char::from_u32) {
                        out.push(c);
                    }
                }
                Some(other) => out.push(other),
                None => break,
            },
            c => out.push(c),
        }
    }
    Some(out)
}

// ---------------------------------------------------------------------------
// finding the process and the descriptor
// ---------------------------------------------------------------------------

/// Find the QEMU process whose command line matches `hint`.
pub fn find_qemu_pid(hint: &str) -> std::io::Result<i32> {
    let mut found = Vec::new();
    for entry in std::fs::read_dir("/proc")? {
        let entry = entry?;
        let Some(pid) = entry.file_name().to_str().and_then(|s| s.parse::<i32>().ok()) else {
            continue;
        };
        let Ok(cmdline) = std::fs::read(entry.path().join("cmdline")) else { continue };
        let cmdline = String::from_utf8_lossy(&cmdline).replace('\0', " ");
        if cmdline.contains("qemu-system") && cmdline.contains(hint) {
            found.push(pid);
        }
    }
    match found.len() {
        1 => Ok(found[0]),
        0 => Err(std::io::Error::other(format!("no qemu-system process matching {hint:?}"))),
        // Refuse rather than pick. Two VMs matching one hint and the daemon
        // guessing is how a client ends up mapping the wrong guest's RAM.
        _ => Err(std::io::Error::other(format!(
            "{} qemu-system processes match {hint:?}: {found:?} — narrow the hint",
            found.len()
        ))),
    }
}

/// Find the file descriptor QEMU holds open on the memory backend.
///
/// Matched by the *link text*, which for `memory-backend-memfd` is
/// `/memfd:memory-backend-memfd (deleted)`. There is no other way to reach the
/// object: a memfd has no path, which is the whole reason `sv-hostd` needs
/// privilege and therefore the whole reason `sv-hostd` exists (§3).
pub fn find_backend_fd(pid: i32, link_hint: &str) -> std::io::Result<PathBuf> {
    let dir = format!("/proc/{pid}/fd");
    let mut found = Vec::new();
    for entry in std::fs::read_dir(&dir)? {
        let entry = entry?;
        let Ok(target) = std::fs::read_link(entry.path()) else { continue };
        if target.to_string_lossy().contains(link_hint) {
            found.push(entry.path());
        }
    }
    found.sort();
    match found.len() {
        0 => Err(std::io::Error::other(format!(
            "{dir} has no descriptor whose link contains {link_hint:?} — \
             is the guest on memory-backend-memfd with share=on?"
        ))),
        // QEMU may hold more than one descriptor on the same object; they are
        // the same file, so the first is as good as any.
        _ => Ok(found.remove(0)),
    }
}

/// Open the backend read-write. `MAP_SHARED` writes through this descriptor land
/// in guest RAM, which is the entire mechanism.
pub fn open_backend(path: &Path) -> std::io::Result<std::fs::File> {
    std::fs::OpenOptions::new().read(true).write(true).open(path)
}

/// The **name** an ICD is to open the backend by (docs/design.md §1a).
///
/// A name rather than a descriptor, because a descriptor may not cross a
/// boundary and this project no longer has a mechanism for one to. Two cases,
/// and which one applies is a property of the deployment rather than of this
/// code:
///
/// - `memory-backend-file` — the link resolves to a real path, and that path is
///   the name. It is openable by anyone with permission on the file, it is
///   stable across a QEMU restart, and it is what a real deployment should use.
/// - `memory-backend-memfd` — the object has no path, and the only name it has
///   is `/proc/<qemu-pid>/fd/<n>`. That is the name docs/design.md §1a calls out as
///   legal — the app side reaching the hypervisor's own backing object — and the
///   cost of it is stated rather than hidden: it is a *same-machine* name, it
///   requires the reader to have `PTRACE_MODE_READ` on QEMU, and it moves that
///   requirement from `sv-hostd` to every ICD, which weakens §3's argument for
///   the daemon existing. It never crosses to the GPU side, on any topology:
///   the GPU side is in the VM and locates its own memory by content (§5.1).
pub fn backend_name(fd_path: &Path) -> String {
    if let Ok(target) = std::fs::read_link(fd_path)
        && target.is_absolute()
        && target.exists()
    {
        return target.to_string_lossy().into_owned();
    }
    fd_path.to_string_lossy().into_owned()
}

/// The backing object's size, which bounds the scan.
pub fn backend_len(f: &std::fs::File) -> std::io::Result<u64> {
    Ok(f.metadata()?.len())
}

/// Everything `Mode::Qemu` needs, discovered in one call.
pub struct QemuTarget {
    pub pid: i32,
    pub fd_path: PathBuf,
    pub file: std::fs::File,
    pub file_len: u64,
    pub backend_id: String,
    pub regions: RegionTable,
}

impl QemuTarget {
    /// Discover the guest: pid, backend id, descriptor, and the region table.
    ///
    /// `qmp` is the monitor socket; `hint` narrows the process match. Nothing
    /// here is cached between calls — docs/design.md §5.1 asks for the table to be
    /// re-read whenever the guest's RAM size, machine type or QEMU version
    /// changes, and the cheapest way to honour that is never to remember it.
    pub fn discover(qmp: &Path, hint: &str) -> std::io::Result<QemuTarget> {
        let pid = find_qemu_pid(hint)?;
        let mut mon = Qmp::connect(qmp)?;
        let memdevs = mon.memdevs()?;
        let flat = mon.flat_view()?;

        // Pick the backend that actually has RAM aliases in the flat view. A
        // guest may have several memory backends (NUMA nodes, a separate ROM
        // backend); choosing the largest one that appears is right for every
        // configuration measured, and picking blind is not.
        let mut best: Option<(String, RegionTable)> = None;
        for id in &memdevs {
            let t = parse_flat_view(&flat, id);
            if t.is_empty() {
                continue;
            }
            if best.as_ref().is_none_or(|(_, b)| t.total_ram() > b.total_ram()) {
                best = Some((id.clone(), t));
            }
        }
        let (backend_id, regions) = best.ok_or_else(|| {
            std::io::Error::other(format!(
                "none of the {} memory backend(s) {memdevs:?} has RAM aliases in the flat view",
                memdevs.len(),
            ))
        })?;

        let fd_path = find_backend_fd(pid, "memfd:")
            .or_else(|_| find_backend_fd(pid, &backend_id))?;
        let file = open_backend(&fd_path)?;
        let file_len = backend_len(&file)?;
        Ok(QemuTarget { pid, fd_path, file, file_len, backend_id, regions })
    }
}

/// Read `len` bytes at `offset` of the backend. Used by the scan.
pub fn read_at(f: &std::fs::File, offset: u64, buf: &mut [u8]) -> std::io::Result<usize> {
    use std::os::unix::fs::FileExt;
    let mut got = 0;
    while got < buf.len() {
        match f.read_at(&mut buf[got..], offset + got as u64) {
            Ok(0) => break,
            Ok(n) => got += n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
    Ok(got)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The **measured** flat view of `testhost`'s Windows guest, taken
    /// verbatim from QMP on 2026-08-12. Kept as a fixture because every constant
    /// design.md warns about is in it: the 3 GiB split, the ROM window, and the
    /// twelve SynIC pages.
    const TESTHOST: &str = "\
  0000000000000000-00000000000bffff (prio 0, ram): virtiofsmem KVM
  00000000000c0000-00000000000dffff (prio 1, rom): pc.rom KVM
  00000000000e0000-00000000000fffff (prio 0, rom): system.flash0 @000000000035c000 KVM
  0000000000100000-0000000000102fff (prio 0, ram): virtiofsmem @0000000000100000 KVM
  0000000000103000-0000000000103fff (prio 0, ram): synic-0-msg-page KVM
  000000000010e000-000000000010efff (prio 0, ram): synic-11-msg-page KVM
  000000000010f000-00000000bfffffff (prio 0, ram): virtiofsmem @000000000010f000 KVM
  00000000c0040000-00000000c0041fff (prio 0, i/o): intel-hda
  00000000fed40080-00000000fed40fff (prio 0, ram): tpm-crb-cmd
  0000000100000000-000000037fffffff (prio 0, ram): virtiofsmem @00000000c0000000 KVM
";

    #[test]
    fn the_measured_flat_view_parses_to_the_measured_table() {
        let t = parse_flat_view(TESTHOST, "virtiofsmem");
        assert_eq!(
            t.aliases(),
            &[
                RamAlias { gpa: 0, len: 0xc_0000, file_offset: 0 },
                RamAlias { gpa: 0x10_0000, len: 0x3000, file_offset: 0x10_0000 },
                RamAlias { gpa: 0x10_f000, len: 0xbfff_ffff - 0x10_f000 + 1, file_offset: 0x10_f000 },
                RamAlias { gpa: 0x1_0000_0000, len: 0x2_8000_0000, file_offset: 0xc000_0000 },
            ]
        );
        // **The addressable RAM is smaller than the backing object**, and by an
        // exact amount: the guest was given 13312 MiB and the aliases account
        // for 311296 bytes less. That is the 256 KiB ROM window
        // (0xc0000-0xfffff) plus the twelve 4 KiB SynIC message pages, both of
        // which overlay the backend and make those offsets unreachable from the
        // guest.
        //
        // It is worth asserting rather than rounding away, because it is the
        // justification for `is_ram_offset`: 76 pages of this memfd are not
        // guest memory, and a scan that accepted a stamp found in them would
        // hand out an arena page the guest cannot see. `arena-host`, which
        // sweeps the whole file, would.
        let backend_size = 13312u64 * 1024 * 1024;
        let unreachable = 0x40000u64 + 12 * 4096;
        assert_eq!(unreachable, 311_296);
        assert_eq!(t.total_ram(), backend_size - unreachable);
        assert!(!t.is_ram_offset(0xc_0000), "the ROM window is not guest RAM");
        assert!(!t.is_ram_offset(0x10_3000), "a SynIC message page is not guest RAM");
        assert!(t.is_ram_offset(0x10_f000), "…and the page after them is");
    }

    /// The end address is inclusive. An exclusive reading loses one page per
    /// alias, and the lost page of the 3 GiB alias is the one at 0xbffff000 —
    /// nothing finds it and nothing reports it missing.
    #[test]
    fn the_last_page_of_an_alias_is_in_the_table() {
        let t = parse_flat_view(TESTHOST, "virtiofsmem");
        assert_eq!(t.gpa_to_offset(0xbfff_f000), Some(0xbfff_f000));
        assert_eq!(t.gpa_to_offset(0xbfff_ffff), Some(0xbfff_ffff));
        // …and the byte after it is the PCI hole, not RAM.
        assert_eq!(t.gpa_to_offset(0xc000_0000), None);
    }

    /// R21, as an assertion. The three ranges that are not RAM at their identity
    /// offsets, and the one that is not at its identity offset at all.
    #[test]
    fn the_three_traps_r21_names_are_all_refused_or_translated() {
        let t = parse_flat_view(TESTHOST, "virtiofsmem");
        // The ROM window: inside the guest's low memory and not the backend.
        assert_eq!(t.gpa_to_offset(0x000c_0000), None);
        assert_eq!(t.gpa_to_offset(0x000f_ffff), None);
        // A SynIC message page: RAM, and not *this* RAM.
        assert_eq!(t.gpa_to_offset(0x0010_3000), None);
        assert_eq!(t.gpa_to_offset(0x0010_e000), None);
        // Above 4 GiB the offset is the GPA less 1 GiB, because the split is at
        // 3 GiB. The naive `- 0x8000_0000` (a 2 GiB below-4G split) and the
        // i440fx-at-small-RAM `- 0x2000_0000` are both wrong here.
        assert_eq!(t.gpa_to_offset(0x1_0000_0000), Some(0xc000_0000));
        assert_eq!(t.gpa_to_offset(0x2_b6a6_d000), Some(0x2_76a6_d000));
    }

    /// The exact pair from `measured-gpu-side.md`: the right translation finds
    /// the stamped page, the identity mistake reads unrelated zeroed memory.
    /// This is the one case in the project where the *wrong* answer is silent,
    /// so it is asserted in both directions.
    #[test]
    fn the_r21_measurement_round_trips() {
        let t = parse_flat_view(TESTHOST, "virtiofsmem");
        // Page 0 of the 64 MiB step-0 window sat at memfd offset 0x276a6d000.
        assert_eq!(t.offset_to_gpa(0x2_76a6_d000), Some(0x2_b6a6_d000));
        assert_eq!(t.gpa_to_offset(0x2_b6a6_d000), Some(0x2_76a6_d000));
        // The identity mistake: treating the offset as a GPA lands somewhere
        // that *is* valid RAM, at a different offset. Valid, wrong, and silent.
        assert_eq!(t.gpa_to_offset(0x2_76a6_d000), Some(0x2_36a6_d000));
        assert_ne!(t.gpa_to_offset(0x2_76a6_d000), Some(0x2_76a6_d000));
    }

    /// The flat view of a guest **with SMM**, where every alias of the backend
    /// appears twice — once in the system view and once in the SMM one. Measured
    /// on the local development guest 2026-08-12; `testhost` runs `smm=off`
    /// and never showed it.
    ///
    /// The symptom without this filter was a 2 GiB guest reporting 4095 MiB.
    const SMM_GUEST: &str = "\
FlatView #0
 AS \"memory\", root: system
 AS \"cpu-memory-0\", root: system
 Root memory region: system
  0000000000000000-00000000000cafff (prio 0, ram): svmem KVM
  00000000000cb000-00000000000e7fff (prio 0, ram): svmem @00000000000cb000 KVM
  0000000000100000-000000007fffffff (prio 0, ram): svmem @0000000000100000 KVM

FlatView #1
 AS \"smm-memory\", root: smram-region
 Root memory region: smram-region
  0000000000000000-00000000000cafff (prio 0, ram): svmem KVM
  00000000000cb000-00000000000e7fff (prio 0, ram): svmem @00000000000cb000 KVM
  0000000000100000-000000007fffffff (prio 0, ram): svmem @0000000000100000 KVM
";

    #[test]
    fn only_the_system_flat_view_is_parsed() {
        let t = parse_flat_view(SMM_GUEST, "svmem");
        assert_eq!(t.aliases().len(), 3, "the SMM view's copies must not be counted");
        // 2 GiB less the two holes the fixture carries.
        let holes = (0xcb000 - 0xcb000) + (0x100000 - 0xe8000);
        assert_eq!(t.total_ram(), 0x8000_0000 - holes);
        assert!(t.total_ram() < 0x8000_0000, "and it is under the guest's RAM, not double it");
    }

    /// The fixtures in this file are fragments with no `FlatView` header, and a
    /// filter that required one would make every one of them pass by matching
    /// nothing at all.
    #[test]
    fn a_headerless_fragment_is_still_parsed() {
        assert_eq!(parse_flat_view(TESTHOST, "virtiofsmem").aliases().len(), 4);
    }

    #[test]
    fn a_different_backend_id_selects_nothing() {
        // Naming the wrong backend must give an empty table rather than the
        // SynIC pages or the ROM.
        assert!(parse_flat_view(TESTHOST, "pc.ram").is_empty());
        assert!(parse_flat_view(TESTHOST, "synic-0-msg-page").total_ram() == 4096);
    }

    #[test]
    fn a_missing_at_offset_means_zero_and_not_the_gpa() {
        let t = parse_flat_view(
            "  0000000040000000-000000004000ffff (prio 0, ram): mem KVM\n",
            "mem",
        );
        assert_eq!(t.aliases()[0].file_offset, 0, "no @ means offset 0");
        assert_eq!(t.gpa_to_offset(0x4000_1000), Some(0x1000));
    }

    #[test]
    fn rom_and_io_lines_are_not_ram() {
        let t = parse_flat_view(
            "  0000000000000000-0000000000000fff (prio 0, rom): mem KVM\n\
             \x20 0000000000001000-0000000000001fff (prio 0, romd): mem KVM\n\
             \x20 0000000000002000-0000000000002fff (prio 0, i/o): mem\n\
             \x20 0000000000003000-0000000000003fff (prio 0, ram): mem @0000000000003000\n",
            "mem",
        );
        assert_eq!(t.aliases().len(), 1, "only the ram line");
        assert_eq!(t.aliases()[0].gpa, 0x3000);
    }

    #[test]
    fn json_string_unescaping_handles_the_flat_view_shape() {
        // The `AS "memory"` line is part of the fixture because it is part of
        // every real dump, and because the parser now needs it to know which
        // flat view it is in.
        let reply =
            r#"{"return": "FlatView #0\r\n AS \"memory\", root: system\r\n  0000-0fff (prio 0, ram): mem KVM\r\n"}"#;
        let s = unescape_json_string_after(reply, "\"return\":").unwrap();
        assert!(s.contains("FlatView #0"));
        assert!(s.contains("(prio 0, ram): mem KVM"));
        // \r\n survived as two characters, so `lines()` sees a trailing \r —
        // which is why the parser trims it.
        assert!(s.contains('\r'));
        let t = parse_flat_view(&s, "mem");
        assert_eq!(t.aliases().len(), 1);
    }

    #[test]
    fn memdev_ids_are_read_out_of_the_reply() {
        let reply = r#"{"return": [{"size": 13958643712, "merge": false, "dump": true, "prealloc": false, "host-nodes": [], "policy": "default", "id": "virtiofsmem"}]}"#;
        // The scanner takes the first "id" then the next "size" *after* it, so a
        // reply that puts size first needs the id-first ordering to be handled;
        // assert the shape QEMU actually emits rather than a convenient one.
        let mut rest = reply;
        let i = rest.find("\"id\":").unwrap();
        rest = &rest[i + 5..];
        assert_eq!(json_string(rest).unwrap(), "virtiofsmem");
    }
}
