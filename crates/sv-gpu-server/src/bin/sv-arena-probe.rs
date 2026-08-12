//! `sv-arena-probe` — the GPU-allocated arena, across two processes, with a
//! shader in the middle.
//!
//! ```text
//! sv-arena-probe gpu --socket <path> [--device N] [--bytes N]
//! sv-arena-probe app --socket <path>
//! ```
//!
//! # The question this answers and `sv-export-probe` does not
//!
//! `sv-export-probe` established, in one process, that a driver will export a
//! descriptor and that `mmap` of it is the same memory `vkMapMemory` sees. Three
//! things it left open, and all three are load-bearing for the flip to a
//! GPU-allocated arena:
//!
//! 1. **Does the descriptor survive `SCM_RIGHTS` into an unrelated process, and
//!    is the `mmap` there the same pages?** An in-process `mmap` proves the
//!    exporter implements `mmap`; it does not prove the object is shareable.
//! 2. **Can a `VkBuffer` be bound to it and can the GPU read what the other
//!    process wrote?** That is R15 — design.md §14's largest open item — for
//!    this topology, and it is the difference between "the CPU can share memory"
//!    and "the arena works".
//! 3. **Can the other process see what the GPU wrote?** The direction that
//!    matters for readback, and the one a bounce buffer would silently fail.
//!
//! The two processes share exactly one thing: a Unix socket. Everything else —
//! the driver, the mapping, the address space — is separate, which is what makes
//! this the container topology rather than a simulation of it.
//!
//! # The workload
//!
//! `gpu-probe`'s, unchanged and by path: the shader writes
//! `arena_record::gpu::transform` of every payload word from the input half into
//! the output half, skipping the first 64 words of each page so the page record
//! survives. Both processes and the shader compute the same function from three
//! independent sources, which is why a disagreement is informative rather than
//! ambiguous.

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("sv-arena-probe: Linux-only (SCM_RIGHTS)");
    std::process::exit(1);
}

#[cfg(target_os = "linux")]
fn main() -> std::process::ExitCode {
    match run() {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("sv-arena-probe: {e}");
            std::process::ExitCode::FAILURE
        }
    }
}

/// 4 MiB: enough to be several pages and a real dispatch, small enough that a
/// failure is quick and that allocating it on somebody's desktop GPU is polite.
#[cfg(target_os = "linux")]
const DEFAULT_BYTES: u64 = 4 << 20;

#[cfg(target_os = "linux")]
fn run() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().collect();
    let arg = |k: &str| -> Option<String> {
        args.iter().position(|a| a == k).and_then(|i| args.get(i + 1)).cloned()
    };
    let socket = arg("--socket").ok_or("--socket is required")?;
    match args.get(1).map(String::as_str).unwrap_or("") {
        "gpu" => gpu(
            &socket,
            arg("--device").map_or(Ok(0), |s| s.parse())?,
            arg("--bytes").map_or(Ok(DEFAULT_BYTES), |s| s.parse())?,
        ),
        "app" => app(&socket),
        _ => {
            eprintln!("usage: sv-arena-probe <gpu|app> --socket <path> [--device N] [--bytes N]");
            Err("bad role".into())
        }
    }
}

/// The GPU side: allocate, export, hand over, dispatch.
#[cfg(target_os = "linux")]
fn gpu(socket: &str, device: usize, bytes: u64) -> Result<(), Box<dyn std::error::Error>> {
    use arena_record::gpu as ref_gpu;
    use std::os::fd::AsFd;
    use std::os::unix::net::UnixListener;
    use sv_gpu_server::{arena, driver::Driver, workload};
    use sv_transport::cold::{Attachment, ColdMessage, recv, send, send_with_fds};

    let driver = Driver::open(device)?;
    println!("[gpu] {} ({})", driver.name, driver.driver_key());
    println!(
        "[gpu] VK_KHR_external_memory_fd {}, VK_EXT_external_memory_dma_buf {}, \
         VK_EXT_external_memory_host {}",
        driver.external_fd.is_some(),
        driver.has_dma_buf,
        driver.external_host.is_some()
    );

    let a = arena::allocate_and_export(&driver, bytes)?;
    println!(
        "[gpu] allocated {} MiB from memory type {} and exported a {} descriptor",
        bytes >> 20,
        a.memory_type,
        if driver.has_dma_buf { "dma-buf" } else { "opaque-fd" }
    );

    let _ = std::fs::remove_file(socket);
    let listener = UnixListener::bind(socket)?;
    let (sock, _) = listener.accept()?;
    let (msg, _) = recv(&sock)?;
    if !matches!(msg, ColdMessage::AttachRequest) {
        return Err(format!("expected AttachRequest, got {msg:?}").into());
    }
    let at = Attachment {
        control_offset: 0,
        control_len: 0,
        arena_offset: 0,
        arena_len: bytes,
        generation: 1,
        run_count: 0,
    };
    let mut buf = Vec::new();
    ColdMessage::Attach(at).encode(&mut buf)?;
    let fd = a.exported_fd.as_ref().ok_or("no exported descriptor")?.as_fd();
    send_with_fds(&sock, &buf, &[fd])?;
    println!("[gpu] descriptor passed over SCM_RIGHTS");

    // The app side says when it has written the input half.
    let (msg, _) = recv(&sock)?;
    if !matches!(msg, ColdMessage::Ping) {
        return Err(format!("expected Ping, got {msg:?}").into());
    }

    let split = ref_gpu::Split::for_window((bytes as usize) / sv_transport::region::PAGE);
    println!(
        "[gpu] dispatching over {} words ({} MiB in, {} MiB out)",
        split.words(),
        split.input_bytes() >> 20,
        split.input_bytes() >> 20
    );
    workload::run(&driver, a.memory, split)?;
    println!("[gpu] the shader ran and the fence was signalled");
    send(&sock, &ColdMessage::Ping)?;

    // Stay alive until the app side has read the result: the arena is our
    // allocation and freeing it out from under the other process's mapping
    // would be a use-after-free across a process boundary.
    let _ = recv(&sock);
    let _ = std::fs::remove_file(socket);
    Ok(())
}

/// The app side: map the descriptor, write, verify.
#[cfg(target_os = "linux")]
fn app(socket: &str) -> Result<(), Box<dyn std::error::Error>> {
    use arena_record::gpu as ref_gpu;
    use std::os::fd::AsFd;
    use std::os::unix::net::UnixStream;
    use sv_transport::cold::{ColdMessage, recv, send};
    use sv_transport::region::{PAGE, SharedRegion};

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    let sock = loop {
        match UnixStream::connect(socket) {
            Ok(s) => break s,
            Err(_) if std::time::Instant::now() < deadline => {
                std::thread::sleep(std::time::Duration::from_millis(20))
            }
            Err(e) => return Err(Box::new(e)),
        }
    };
    send(&sock, &ColdMessage::AttachRequest)?;
    let (msg, fd) = recv(&sock)?;
    let at = match msg {
        ColdMessage::Attach(a) => a,
        other => return Err(format!("unexpected {other:?}").into()),
    };
    let fd = fd.ok_or("the GPU side sent no descriptor")?;

    // Question 1: does the descriptor map here at all?
    let region = SharedRegion::map_fd(fd.as_fd(), 0, at.arena_len as usize).map_err(|e| {
        format!(
            "the exported descriptor does not mmap in an unrelated process ({e}). \
             The exporter implements mmap — sv-export-probe proved that in-process — \
             so what failed is sharing it, and the GPU-allocated arena cannot work \
             on this driver."
        )
    })?;
    println!("[app] mapped {} MiB of the GPU's allocation", at.arena_len >> 20);

    let split = ref_gpu::Split::for_window((at.arena_len as usize) / PAGE);
    let half = split.input_bytes();
    let words = split.words();
    let stamp: u64 = 0x5356_4152_454E_4131; // "SVARENA1"

    // Write the input half through *our* mapping. The GPU side is told nothing
    // but "go"; every byte crosses as memory, not as a message.
    let mut input = vec![0u8; half];
    for i in 0..(half / 4) as u32 {
        let v = ref_gpu::host_fill(stamp, i);
        input[i as usize * 4..i as usize * 4 + 4].copy_from_slice(&v.to_le_bytes());
    }
    region.write_bytes(0, &input)?;
    // A known sentinel in the output half, so "the GPU wrote nothing" is
    // distinguishable from "the GPU wrote zeros".
    region.fill(half as u64, half as u64, 0xCD)?;
    println!("[app] wrote {} MiB of pattern and poisoned the output half", half >> 20);

    send(&sock, &ColdMessage::Ping)?;
    let (msg, _) = recv(&sock)?;
    if !matches!(msg, ColdMessage::Ping) {
        return Err(format!("expected Ping, got {msg:?}").into());
    }

    // Questions 2 and 3, together: did the GPU read what we wrote, and can we
    // see what it wrote?
    let mut output = vec![0u8; half];
    region.read_bytes(half as u64, &mut output)?;

    let mut checked = 0u64;
    let mut wrong = 0u64;
    let mut untouched = 0u64;
    let mut first_bad = None;
    for i in 0..words {
        if !ref_gpu::is_payload(i) {
            continue;
        }
        let at_byte = i as usize * 4;
        if at_byte + 4 > output.len() {
            break;
        }
        let got = u32::from_le_bytes(output[at_byte..at_byte + 4].try_into().unwrap());
        let want = ref_gpu::transform(ref_gpu::host_fill(stamp, i), i);
        checked += 1;
        if got == 0xCDCD_CDCD {
            untouched += 1;
        } else if got != want {
            wrong += 1;
            first_bad.get_or_insert((i, got, want));
        }
    }

    println!("[app] checked {checked} payload words: {wrong} wrong, {untouched} untouched");
    if untouched == checked {
        return Err("the GPU wrote nothing: every payload word still holds the poison \
                    value. The dispatch reported success, so the buffer it wrote is \
                    not the memory this process mapped."
            .into());
    }
    if wrong != 0 || untouched != 0 {
        let (i, got, want) = first_bad.unwrap_or((0, 0, 0));
        return Err(format!(
            "{wrong} wrong and {untouched} untouched of {checked}; first bad word {i}: \
             got 0x{got:08x}, expected 0x{want:08x}"
        )
        .into());
    }

    println!(
        "[app] PASS — every payload word is transform(host_fill(i), i). The GPU read \
         bytes this process wrote through its own mapping, and this process read back \
         bytes the GPU wrote."
    );
    send(&sock, &ColdMessage::Shutdown)?;
    Ok(())
}
