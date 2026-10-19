//! Per-region handles over the guest-RAM memfd: udmabufs
//! (waycompose-design.md §108.16, `security-model.md` R43).
//!
//! The broker is the one process that holds QEMU's RAM memfd. A client (an
//! ICD, a proxy) must reach the pages of the regions it may use and nothing
//! else. A udmabuf is a dma-buf made of a list of ranges of a sealed memfd
//! (`UDMABUF_CREATE_LIST`). Mapping one maps exactly those pages, so a client
//! given a region's udmabufs can reach that region and no other byte of the
//! guest. It is zero-copy and costs no host RAM: the pages are the memfd's,
//! already pinned by vfio.
//!
//! The kernel limits one udmabuf to `list_limit` ranges and `size_limit_mb`
//! MiB (1024 and 64 by default, on both machines here). So a region becomes
//! as many udmabufs as those limits need, each a contiguous part of the
//! region, mapped side by side.

use std::os::fd::{AsRawFd, BorrowedFd, FromRawFd, OwnedFd};

/// One udmabuf: the part `[offset, offset + len)` of a region.
pub struct Part {
    pub fd: OwnedFd,
    pub offset: u64,
    pub len: u64,
}

/// The kernel's limits, read from the module's parameters.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    pub list_limit: usize,
    pub size_limit: u64,
}

impl Limits {
    pub fn read() -> Limits {
        let p = |n: &str, d: u64| {
            std::fs::read_to_string(format!("/sys/module/udmabuf/parameters/{n}")).ok().and_then(|s| s.trim().parse().ok()).unwrap_or(d)
        };
        Limits { list_limit: p("list_limit", 1024) as usize, size_limit: p("size_limit_mb", 64) << 20 }
    }
}

/// Cut a region's runs `(file_offset, len)` (in region order) into parts that
/// each fit one udmabuf: at most `list_limit` ranges and `size_limit` bytes.
/// Returns, per part, its ranges and its offset in the region.
pub fn plan(runs: &[(u64, u64)], l: Limits) -> Vec<(u64, Vec<(u64, u64)>)> {
    let mut out: Vec<(u64, Vec<(u64, u64)>)> = Vec::new();
    let (mut at, mut cur, mut cur_len, mut cur_at) = (0u64, Vec::new(), 0u64, 0u64);
    for &(mut off, mut len) in runs {
        while len > 0 {
            if cur.len() == l.list_limit || cur_len == l.size_limit {
                out.push((cur_at, std::mem::take(&mut cur)));
                cur_at = at;
                cur_len = 0;
            }
            let take = len.min(l.size_limit - cur_len);
            cur.push((off, take));
            cur_len += take;
            at += take;
            off += take;
            len -= take;
        }
    }
    if !cur.is_empty() {
        out.push((cur_at, cur));
    }
    out
}

#[repr(C)]
struct CreateItem {
    memfd: u32,
    pad: u32,
    offset: u64,
    size: u64,
}

#[repr(C)]
struct CreateListHdr {
    flags: u32,
    count: u32,
}

// _IOW('u', 0x43, struct udmabuf_create_list)
const UDMABUF_CREATE_LIST: libc::c_ulong = 0x4008_7543;
const UDMABUF_FLAGS_CLOEXEC: u32 = 1;

/// Make the udmabufs of one region from `memfd` (which must be sealed against
/// shrinking, as QEMU's RAM memfd is).
pub fn create(memfd: BorrowedFd<'_>, runs: &[(u64, u64)], l: Limits) -> std::io::Result<Vec<Part>> {
    let dev = std::fs::OpenOptions::new().read(true).write(true).open("/dev/udmabuf")?;
    let mut parts = Vec::new();
    for (offset, items) in plan(runs, l) {
        let mut buf: Vec<u64> = vec![0; 1 + items.len() * 3];
        // SAFETY: `buf` is large enough for the header and `items.len()`
        // items, all of which are plain integers.
        unsafe {
            let hdr = buf.as_mut_ptr() as *mut CreateListHdr;
            (*hdr).flags = UDMABUF_FLAGS_CLOEXEC;
            (*hdr).count = items.len() as u32;
            let list = (buf.as_mut_ptr() as *mut u8).add(8) as *mut CreateItem;
            for (i, &(o, n)) in items.iter().enumerate() {
                *list.add(i) = CreateItem { memfd: memfd.as_raw_fd() as u32, pad: 0, offset: o, size: n };
            }
        }
        let fd = unsafe { libc::ioctl(dev.as_raw_fd(), UDMABUF_CREATE_LIST as _, buf.as_ptr()) };
        if fd < 0 {
            return Err(std::io::Error::last_os_error());
        }
        let len = items.iter().map(|i| i.1).sum();
        parts.push(Part { fd: unsafe { OwnedFd::from_raw_fd(fd) }, offset, len });
    }
    Ok(parts)
}

#[cfg(test)]
mod tests {
    use super::*;
    const M: u64 = 1 << 20;

    #[test]
    fn a_region_is_cut_at_the_kernels_limits_and_nothing_is_lost() {
        let l = Limits { list_limit: 3, size_limit: 8 * M };
        // five 2 MiB runs and one 10 MiB run
        let runs = vec![(0, 2 * M), (100 * M, 2 * M), (50 * M, 2 * M), (60 * M, 2 * M), (70 * M, 2 * M), (200 * M, 10 * M)];
        let p = plan(&runs, l);
        for (_, items) in &p {
            assert!(items.len() <= 3);
            assert!(items.iter().map(|i| i.1).sum::<u64>() <= 8 * M);
        }
        // parts tile the region in order, and the ranges are the runs, in order
        let mut at = 0;
        let mut flat = Vec::new();
        for (off, items) in &p {
            assert_eq!(*off, at);
            at += items.iter().map(|i| i.1).sum::<u64>();
            flat.extend(items.iter().copied());
        }
        assert_eq!(at, 20 * M);
        let mut merged: Vec<(u64, u64)> = Vec::new();
        for (o, n) in flat {
            match merged.last_mut() {
                Some((mo, mn)) if *mo + *mn == o => *mn += n,
                _ => merged.push((o, n)),
            }
        }
        assert_eq!(merged, runs);
        // the defaults put a 2 GiB region of 1024 scattered units in 32 parts
        let units: Vec<(u64, u64)> = (0..1024u64).map(|i| (i * 4 * M, 2 * M)).collect();
        assert_eq!(plan(&units, Limits { list_limit: 1024, size_limit: 64 * M }).len(), 32);
    }

    /// The real thing, where `/dev/udmabuf` can be opened (root in the test
    /// VM of §108.16, or a user in group kvm): a sealed memfd, a udmabuf of two
    /// of its ranges, mapped; the bytes are the memfd's and nothing else is
    /// reachable through it.
    #[test]
    fn a_udmabuf_maps_exactly_its_ranges() {
        if std::fs::OpenOptions::new().read(true).write(true).open("/dev/udmabuf").is_err() {
            eprintln!("skipped: /dev/udmabuf is not open to this user");
            return;
        }
        use std::os::unix::fs::FileExt;
        let name = std::ffi::CString::new("sv-ram-udma-test").unwrap();
        // MFD_ALLOW_SEALING = 2
        let fd = unsafe { libc::memfd_create(name.as_ptr(), 2) };
        assert!(fd >= 0);
        let f = unsafe { std::fs::File::from_raw_fd(fd) };
        f.set_len(16 * M).unwrap();
        // F_ADD_SEALS = 1033, F_SEAL_SHRINK = 2
        assert_eq!(unsafe { libc::fcntl(fd, 1033, 2) }, 0);
        for i in 0..8u64 {
            f.write_all_at(&[i as u8 + 1; 64], i * 2 * M).unwrap();
        }
        let runs = [(6 * M, 2 * M), (2 * M, 2 * M)];
        let parts = create(std::os::fd::AsFd::as_fd(&f), &runs, Limits::read()).unwrap();
        assert_eq!(parts.len(), 1);
        let p = unsafe { libc::mmap(std::ptr::null_mut(), (4 * M) as usize, libc::PROT_READ | libc::PROT_WRITE, libc::MAP_SHARED, parts[0].fd.as_raw_fd(), 0) };
        assert_ne!(p, libc::MAP_FAILED);
        let s = unsafe { std::slice::from_raw_parts_mut(p as *mut u8, (4 * M) as usize) };
        assert_eq!(s[0], 4, "the first range is the memfd's unit 3");
        assert_eq!(s[(2 * M) as usize], 2, "the second range is unit 1");
        // a write through the udmabuf lands in the memfd
        s[100] = 0xEE;
        let mut b = [0u8; 1];
        f.read_exact_at(&mut b, 6 * M + 100).unwrap();
        assert_eq!(b[0], 0xEE);
        // and the udmabuf is exactly its ranges: mapping past them fails
        let past = unsafe { libc::mmap(std::ptr::null_mut(), (6 * M) as usize, libc::PROT_READ, libc::MAP_SHARED, parts[0].fd.as_raw_fd(), 0) };
        assert!(past == libc::MAP_FAILED || {
            unsafe { libc::munmap(past, (6 * M) as usize) };
            false
        }, "a mapping larger than the udmabuf must fail");
        unsafe { libc::munmap(p, (4 * M) as usize) };
    }
}
