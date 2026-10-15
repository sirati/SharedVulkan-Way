//! `wc-ivshmem-probe` — does the ivshmem device open, map and carry bytes, as
//! the account that will run the compositor?
//!
//! IVSHMEM ARENA, docs/waycompose-design.md §82. The core claim of that section is that
//! BAR2 of the guest's ivshmem device and the file QEMU backs it with on the VM
//! host are the same physical pages, reachable on this side **without
//! elevation** and on that side **without root**. This program is the guest's
//! half of that measurement and nothing else:
//!
//! 1. open the device by its interface GUID and map it, through exactly the
//!    code the compositor uses (`wc_handle::arena::attach_ivshmem`) — CACHED;
//! 2. write a marker at arena offset 0, which is byte 4096 of the region and of
//!    the host's file, and read it back;
//! 3. optionally time copies out of the mapping, which is what the compositor's
//!    blit does to every frame, against the same copy between two heap buffers.
//!
//! The host's half is `xxd -s 4096 -l 32` on the file, run as the user.
//!
//! The driver allows one mapping per device and releases it when the handle
//! closes, which is when this process exits; so this must not run while a
//! compositor holds the device, and it says so if one does.

use std::time::Instant;

use wc_handle::arena;

fn main() -> std::process::ExitCode {
    let mut device = 0u32;
    let mut bench = false;
    let mut hold = 0u64;
    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        match a.as_str() {
            "--device" => device = it.next().and_then(|v| v.parse().ok()).unwrap_or(0),
            "--bench" => bench = true,
            "--hold" => hold = it.next().and_then(|v| v.parse().ok()).unwrap_or(0),
            _ => {
                eprintln!("usage: wc-ivshmem-probe [--device N] [--bench] [--hold SECS]");
                return std::process::ExitCode::from(2);
            }
        }
    }
    const ID: u32 = 0;
    let t = Instant::now();
    let r = match arena::attach_ivshmem(ID, device) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("wc-ivshmem-probe: {e}");
            return std::process::ExitCode::from(1);
        }
    };
    println!(
        "mapped {} in {:.3} ms: region {} bytes, arena {} bytes, previous generation {}",
        r.device,
        t.elapsed().as_secs_f64() * 1e3,
        r.region_len,
        r.arena_len,
        r.generation - 1
    );

    // The marker: a fixed tag, this process's id and the run id, so the host
    // can tell this run's bytes from a previous one's.
    let mut marker = [0u8; 32];
    marker[..8].copy_from_slice(b"WCPROBE1");
    marker[8..12].copy_from_slice(&std::process::id().to_le_bytes());
    marker[16..24].copy_from_slice(&r.run_id.to_le_bytes());
    if let Err(e) = arena::guest_write(ID, 0, &marker) {
        eprintln!("wc-ivshmem-probe: write: {e}");
        return std::process::ExitCode::from(1);
    }
    let view = wc_handle::Handle::from_descriptor(wc_handle::Descriptor::ArenaSlice {
        arena: ID,
        offset: 0,
        len: r.arena_len,
    })
    .map();
    let view = match view {
        Ok(v) => v,
        Err(e) => {
            eprintln!("wc-ivshmem-probe: map: {e}");
            return std::process::ExitCode::from(1);
        }
    };
    let back = &view.as_slice()[..32];
    println!(
        "wrote and read back at arena offset 0 (region byte 4096): {} pid {} run 0x{:016x} -> {}",
        String::from_utf8_lossy(&marker[..8]),
        std::process::id(),
        r.run_id,
        if back == marker { "identical" } else { "DIFFERENT" }
    );

    if bench {
        // What the compositor's blit does to a frame: read a client's pixels
        // out of the arena into memory of its own. 1920x1080x4 per copy, over
        // successive offsets so the cache is not simply re-read.
        const FRAME: usize = 1920 * 1080 * 4;
        let src = view.as_slice();
        let mut dst = vec![0u8; FRAME];
        let frames = (src.len() / FRAME).min(15);
        let mut run = |label: &str, from: &[u8]| {
            let mut best = f64::MAX;
            let mut sum = 0.0;
            let reps = 5;
            for _ in 0..reps {
                let t = Instant::now();
                for f in 0..frames {
                    dst.copy_from_slice(&from[f * FRAME..(f + 1) * FRAME]);
                }
                let per = t.elapsed().as_secs_f64() / frames as f64;
                best = best.min(per);
                sum += per;
            }
            println!(
                "{label}: one 1920x1080 RGBA frame copy, best {:.3} ms, mean {:.3} ms over {} reps of {frames} frames ({:.2} GB/s best)",
                best * 1e3,
                sum / reps as f64 * 1e3,
                reps,
                FRAME as f64 / best / 1e9
            );
        };
        run("ivshmem CACHED -> heap", src);
        let heap = vec![1u8; FRAME * frames];
        run("heap -> heap (control)", &heap);
    }

    if hold > 0 {
        println!("holding the mapping for {hold} s");
        std::thread::sleep(std::time::Duration::from_secs(hold));
    }
    std::process::ExitCode::SUCCESS
}
