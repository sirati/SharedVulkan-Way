//! The GPU sessions' arena on an ivshmem device, from the VM host's side
//! (waycompose-design.md §87): check a session's slot in the backing file, say
//! where it is to an ICD, and share the one doorbell port among every ICD.
//!
//! # What replaced what
//!
//! `qemu-serve` located each session's pages in QEMU's memory backend, which
//! takes QMP and `/proc/<qemu-pid>/fd` — root's on testhost — and then either
//! ran as root or was started by the root `broker` and dropped to the user,
//! still holding a descriptor for all of guest RAM (security-model.md R30). The
//! slot is now a byte range of a file QEMU backs an ivshmem BAR with, the file
//! is `0660 root:wc-arena`, and this runs as the user from start to end. There
//! is no locate, so there is nothing for root to do.
//!
//! # What is still checked
//!
//! The guest stamps every page of the slot before it says hello, exactly as on
//! guest RAM, and [`check_slot`] reads all of them back at the offset the hello
//! names. A host pointed at the wrong file, a guest that names the wrong slot,
//! and a slot another session is using all fail here, at attach, with the page
//! that disagreed — rather than as an ICD that maps live memory belonging to
//! somebody else.

use std::fs::File;
use std::io::{Read, Write};
use std::os::unix::fs::FileExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::{Arc, Mutex};
use sv_transport::cold::{Attachment, ExtentDesc, Locator};
use sv_transport::region::PAGE;

/// Check the slot `extent` names in `file` (a region of `region_len` bytes)
/// and return the attachment that makes an ICD map exactly it. A pooled
/// session (`Locator::IvshmemPool`, waycompose-design.md §97) is checked by
/// [`check_pool_session`] instead.
///
/// `path` is what the ICD will open, by name, as itself.
pub fn check_slot(
    file: &File,
    path: &str,
    region_len: u64,
    extent: &ExtentDesc,
    generation: u32,
) -> Result<Attachment, String> {
    let region_offset = match extent.locator {
        Locator::Ivshmem { region_offset } => region_offset,
        Locator::IvshmemPool { control_offset, pool_nonce } => {
            return check_pool_session(file, path, region_len, extent, control_offset, pool_nonce, generation);
        }
        _ => {
            return Err(format!(
                "the guest offered a {:?} extent and this host serves only ivshmem slots",
                extent.locator
            ));
        }
    };
    let page = PAGE as u64;
    let control_len = extent.control_pages.checked_mul(page).ok_or("control size overflows")?;
    let arena_len = extent.arena_pages.checked_mul(page).ok_or("arena size overflows")?;
    let len = control_len.checked_add(arena_len).ok_or("slot size overflows")?;
    if control_len == 0 || arena_len == 0 {
        return Err("a slot with no control region or no arena".into());
    }
    check_stamps(file, region_len, region_offset, len, extent.run_id, "slot")?;
    Ok(Attachment {
        object: path.to_string(),
        arena_object: path.to_string(),
        control_offset: region_offset,
        control_len,
        arena_offset: region_offset + control_len,
        arena_len,
        generation,
        // Zero: the slot *is* contiguous at these file offsets, which is the
        // one case the shortcut is for (compare `qemu-serve`'s comment, where
        // the file was guest RAM and it was not).
        run_count: 0,
    })
}

/// Read back every stamped page of `[offset, offset+len)` (§87): a host
/// pointed at the wrong file, a guest that names the wrong range, and a range
/// another session is using all fail here, with the page that disagreed.
fn check_stamps(file: &File, region_len: u64, offset: u64, len: u64, run_id: u64, what: &str) -> Result<(), String> {
    let page = PAGE as u64;
    if !offset.is_multiple_of(page) {
        return Err(format!("{what} offset {offset:#x} is not page-aligned"));
    }
    let end = offset.checked_add(len).ok_or("the range's end overflows")?;
    if end > region_len {
        return Err(format!("{what} {offset:#x}+{len:#x} runs past the {region_len:#x}-byte region"));
    }
    let mut header = [0u8; 20];
    for i in 0..len / page {
        file.read_exact_at(&mut header, offset + i * page)
            .map_err(|e| format!("reading page {i} of the {what}: {e}"))?;
        let magic_ok = &header[0..8] == arena_record::MAGIC;
        let run = u64::from_le_bytes(header[8..16].try_into().unwrap());
        let index = u32::from_le_bytes(header[16..20].try_into().unwrap());
        if !magic_ok || run != run_id || u64::from(index) != i {
            return Err(format!(
                "page {i} of the {what} at {offset:#x} is not the guest's (magic {}, run \
                 0x{run:016x} against 0x{run_id:016x}, index {index}): the wrong file, the wrong \
                 {what}, or a {what} another session holds",
                if magic_ok { "ok" } else { "absent" },
            ));
        }
    }
    Ok(())
}

/// A session of the leased pool (waycompose-design.md §97): its control
/// region's stamps at `control_offset`, and the pool header's nonce at the
/// start of the file, which must be the one the `Hello` carries. The ICD gets
/// the control region at its offset and the **whole file** as the arena: it
/// maps leases out of it as the GPU side grants them, by offset.
fn check_pool_session(
    file: &File,
    path: &str,
    region_len: u64,
    extent: &ExtentDesc,
    control_offset: u64,
    pool_nonce: u64,
    generation: u32,
) -> Result<Attachment, String> {
    let page = PAGE as u64;
    let control_len = extent.control_pages.checked_mul(page).ok_or("control size overflows")?;
    if control_len == 0 || extent.arena_pages != 0 {
        return Err("a pool session has a control region and no arena of its own".into());
    }
    let mut h = [0u8; 32];
    file.read_exact_at(&mut h, 0).map_err(|e| format!("reading the pool header: {e}"))?;
    let s = sv_transport::poolhdr::Snapshot::decode(&h);
    if !s.valid {
        return Err(format!("{path} has no pool header: the guest's GPU process is not serving a pool on this device"));
    }
    if s.nonce != pool_nonce {
        return Err(format!(
            "the session belongs to pool {pool_nonce:#018x} and {path} is pool {:#018x}: the guest's GPU \
             process restarted, or this is the wrong file",
            s.nonce
        ));
    }
    if control_offset < sv_transport::poolhdr::HEADER_LEN {
        return Err(format!("a control region at {control_offset:#x}, inside the pool header"));
    }
    check_stamps(file, region_len, control_offset, control_len, extent.run_id, "control region")?;
    Ok(Attachment {
        object: path.to_string(),
        arena_object: path.to_string(),
        control_offset,
        control_len,
        arena_offset: 0,
        arena_len: region_len,
        generation,
        run_count: 0,
    })
}

/// THE DOORBELL HUB (§87). QEMU's chardev for the port serves one client, the
/// guest's GPU process serves every session over one port, and on this side
/// every session is a different ICD in a different process. So one process
/// holds the port and every ICD connects here instead (`SV_BELL`): bytes from
/// the guest go to every ICD, bytes from any ICD go to the guest. A byte
/// names a lane and not a session, so each ICD rings that lane in its own
/// session — the right one wakes and the rest see a spurious wake, which the
/// doorbell's framing argument already prices at one syscall
/// (`sv_transport::vdoorbell`).
///
/// Returns once the threads are running; they end when `upstream` closes.
pub fn bell_hub(upstream: UnixStream, listener: UnixListener) -> std::io::Result<()> {
    let clients: Arc<Mutex<Vec<UnixStream>>> = Arc::new(Mutex::new(Vec::new()));
    let up_write = Arc::new(Mutex::new(upstream.try_clone()?));
    {
        let clients = Arc::clone(&clients);
        let mut up = upstream;
        std::thread::Builder::new().name("bell-hub-down".into()).spawn(move || {
            let mut buf = [0u8; 64];
            loop {
                let n = match up.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => n,
                };
                if let Ok(mut c) = clients.lock() {
                    c.retain_mut(|s| s.write_all(&buf[..n]).is_ok());
                }
            }
            // The port is gone: close every ICD's side so their doormen end
            // and their waits fall back to the slice, which is the doorbell's
            // documented degradation rather than a hang.
            if let Ok(mut c) = clients.lock() {
                for s in c.drain(..) {
                    let _ = s.shutdown(std::net::Shutdown::Both);
                }
            }
        })?;
    }
    std::thread::Builder::new().name("bell-hub-accept".into()).spawn(move || {
        for conn in listener.incoming() {
            let Ok(s) = conn else { continue };
            let Ok(mut reader) = s.try_clone() else { continue };
            if let Ok(mut c) = clients.lock() {
                c.push(s);
            }
            let up = Arc::clone(&up_write);
            let _ = std::thread::Builder::new().name("bell-hub-up".into()).spawn(move || {
                let mut buf = [0u8; 64];
                loop {
                    let n = match reader.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => n,
                    };
                    let Ok(mut u) = up.lock() else { break };
                    if u.write_all(&buf[..n]).is_err() {
                        break;
                    }
                }
            });
        }
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use sv_transport::cold::Granularity;

    fn extent(run_id: u64, offset: u64) -> ExtentDesc {
        ExtentDesc {
            run_id,
            granularity: Granularity::Small4K,
            control_pages: 2,
            arena_pages: 3,
            locator: Locator::Ivshmem { region_offset: offset },
        }
    }

    fn region(slots: &[(u64, u64)], len: u64) -> (tempfile_path::Tmp, File) {
        let tmp = tempfile_path::Tmp::new();
        let f = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&tmp.0)
            .unwrap();
        f.set_len(len).unwrap();
        for &(off, run) in slots {
            for i in 0..5u32 {
                let page = arena_record::make_guest_page(run, i, arena_record::MODE_IVSHMEM, 0);
                f.write_all_at(&page, off + u64::from(i) * PAGE as u64).unwrap();
            }
        }
        (tmp, f)
    }

    /// A tiny temporary file without a dependency: the test's own directory.
    mod tempfile_path {
        pub struct Tmp(pub std::path::PathBuf);
        impl Tmp {
            pub fn new() -> Tmp {
                use std::sync::atomic::{AtomicU32, Ordering};
                static N: AtomicU32 = AtomicU32::new(0);
                let p = std::env::temp_dir().join(format!(
                    "sv-hostd-ivshmem-{}-{}",
                    std::process::id(),
                    N.fetch_add(1, Ordering::Relaxed)
                ));
                Tmp(p)
            }
        }
        impl Drop for Tmp {
            fn drop(&mut self) {
                let _ = std::fs::remove_file(&self.0);
            }
        }
    }

    /// The slot the guest stamped, at the offset it named, becomes an
    /// attachment for exactly that byte range of the file.
    #[test]
    fn a_stamped_slot_becomes_an_attachment_at_its_offsets() {
        let p = PAGE as u64;
        let (_t, f) = region(&[(5 * p, 0xabc1)], 16 * p);
        let a = check_slot(&f, "/x", 16 * p, &extent(0xabc1, 5 * p), 1).unwrap();
        assert_eq!((a.control_offset, a.control_len), (5 * p, 2 * p));
        assert_eq!((a.arena_offset, a.arena_len), (7 * p, 3 * p));
        assert_eq!((a.object.as_str(), a.arena_object.as_str(), a.run_count), ("/x", "/x", 0));
    }

    /// Every way of pointing at the wrong bytes is refused, with what was
    /// found: another run's slot, a slot nobody stamped, a slot past the end,
    /// an unaligned offset, and an extent that was not an ivshmem one.
    #[test]
    fn the_wrong_slot_the_wrong_run_and_the_wrong_kind_are_refused() {
        let p = PAGE as u64;
        let (_t, f) = region(&[(0, 0x1111), (5 * p, 0x2222)], 16 * p);
        let e = check_slot(&f, "/x", 16 * p, &extent(0x2222, 0), 1).unwrap_err();
        assert!(e.contains("not the guest's"), "{e}");
        let e = check_slot(&f, "/x", 16 * p, &extent(0x3333, 10 * p), 1).unwrap_err();
        assert!(e.contains("magic absent"), "{e}");
        let e = check_slot(&f, "/x", 16 * p, &extent(0x2222, 12 * p), 1).unwrap_err();
        assert!(e.contains("runs past"), "{e}");
        let e = check_slot(&f, "/x", 16 * p, &extent(0x2222, 5 * p + 8), 1).unwrap_err();
        assert!(e.contains("page-aligned"), "{e}");
        let mut scan = extent(0x2222, 0);
        scan.locator = Locator::Scan;
        assert!(check_slot(&f, "/x", 16 * p, &scan, 1).unwrap_err().contains("only ivshmem"));
    }

    /// §97: a pooled session is its stamped control region plus the pool
    /// header's nonce, and its arena is the whole file. The wrong nonce, no
    /// header, a control region inside the header, an arena of its own and a
    /// control region another run stamped are each refused by name.
    #[test]
    fn a_pool_session_is_its_control_region_and_the_pools_nonce() {
        let p = PAGE as u64;
        let ctl = sv_transport::poolhdr::HEADER_LEN;
        let (_t, f) = region(&[(ctl, 0x7777)], 64 * p);
        let ext = |nonce, at, arena_pages| ExtentDesc {
            run_id: 0x7777,
            granularity: Granularity::Small4K,
            control_pages: 5,
            arena_pages,
            locator: Locator::IvshmemPool { control_offset: at, pool_nonce: nonce },
        };
        let e = check_slot(&f, "/gpu", 64 * p, &ext(0xabc, ctl, 0), 1).unwrap_err();
        assert!(e.contains("no pool header"), "{e}");
        let r = sv_transport::region::SharedRegion::heap(sv_transport::poolhdr::HEADER_LEN as usize).unwrap();
        sv_transport::poolhdr::PoolHeader::new(&r).unwrap().publish(0xabc);
        let mut h = [0u8; 32];
        r.read_bytes(0, &mut h).unwrap();
        f.write_all_at(&h, 0).unwrap();
        let a = check_slot(&f, "/gpu", 64 * p, &ext(0xabc, ctl, 0), 1).unwrap();
        assert_eq!((a.control_offset, a.control_len), (ctl, 5 * p));
        assert_eq!((a.arena_offset, a.arena_len, a.arena_object.as_str()), (0, 64 * p, "/gpu"));
        let e = check_slot(&f, "/gpu", 64 * p, &ext(0xdef, ctl, 0), 1).unwrap_err();
        assert!(e.contains("restarted"), "{e}");
        let e = check_slot(&f, "/gpu", 64 * p, &ext(0xabc, 0, 0), 1).unwrap_err();
        assert!(e.contains("inside the pool header"), "{e}");
        let e = check_slot(&f, "/gpu", 64 * p, &ext(0xabc, ctl, 3), 1).unwrap_err();
        assert!(e.contains("no arena of its own"), "{e}");
        let e = check_slot(&f, "/gpu", 64 * p, &ext(0xabc, ctl + p, 0), 1).unwrap_err();
        assert!(e.contains("not the guest's"), "{e}");
    }

    /// The hub: a byte from the guest reaches every ICD, a byte from any ICD
    /// reaches the guest, and an ICD that leaves does not stop the others.
    #[test]
    fn the_hub_fans_the_port_out_and_in() {
        let dir = std::env::temp_dir().join(format!("sv-hub-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("bell.sock");
        let _ = std::fs::remove_file(&path);
        let (mut guest, host_end) = UnixStream::pair().unwrap();
        bell_hub(host_end, UnixListener::bind(&path).unwrap()).unwrap();
        let mut a = UnixStream::connect(&path).unwrap();
        let mut b = UnixStream::connect(&path).unwrap();
        let mut c = UnixStream::connect(&path).unwrap();
        for s in [&a, &b, &c] {
            s.set_read_timeout(Some(std::time::Duration::from_secs(10))).unwrap();
        }
        guest.set_read_timeout(Some(std::time::Duration::from_secs(10))).unwrap();
        // Wait until all three are registered before ringing.
        std::thread::sleep(std::time::Duration::from_millis(200));
        guest.write_all(&[7]).unwrap();
        let mut one = [0u8; 1];
        for s in [&mut a, &mut b, &mut c] {
            s.read_exact(&mut one).unwrap();
            assert_eq!(one[0], 7);
        }
        b.write_all(&[9]).unwrap();
        guest.read_exact(&mut one).unwrap();
        assert_eq!(one[0], 9);
        drop(c);
        guest.write_all(&[5]).unwrap();
        for s in [&mut a, &mut b] {
            s.read_exact(&mut one).unwrap();
            assert_eq!(one[0], 5);
        }
        drop(guest);
        // The port closed: every ICD sees its end.
        assert_eq!(a.read(&mut one).unwrap(), 0);
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir(&dir);
    }
}
