//! Disposable root-only native fixture, never a daemon mode or deployment path.
//! Remote Windows bootstrap is SIMULATED; actual QMP/original/local owner and
//! kernel projections are real. No GPU, cross-VM completion or grant release.
#[cfg(target_os = "linux")]
#[path = "protected_qmp_flow/canonical.rs"]
mod canonical;
#[cfg(target_os = "linux")]
fn main() -> std::io::Result<()> {
    use std::{
        fs::{self, DirBuilder},
        io,
        os::unix::fs::DirBuilderExt,
        path::PathBuf,
        process::{Command, Stdio},
        time::{Duration, Instant},
    };
    use sv_hostd::protected_qmp::PendingOriginal;
    use sv_ram::linux_payload::{
        ServiceLocalBackend,
        protected::{NativeExportState, ProtectedBackend},
    };
    use wc_broker::{
        service_auth::ServicePolicy, service_control::Source, service_grants::OriginalBacking,
    };
    fn require(ok: bool, s: &str) -> io::Result<()> {
        if ok { Ok(()) } else { Err(io::Error::other(s)) }
    }
    fn retired(m: &sv_ram::linux_payload::protected::ProtectedMonitor) -> io::Result<()> {
        let until = Instant::now() + Duration::from_secs(5);
        loop {
            match m.native_state()? {
                NativeExportState::Retired => return Ok(()),
                NativeExportState::Failed => {
                    return Err(io::Error::other("kernel exporter failed"));
                }
                _ => {}
            }
            require(
                Instant::now() < until,
                "native retirement deadline exceeded, no reuse",
            )?;
            std::thread::sleep(Duration::from_millis(1));
        }
    }
    let args: Vec<_> = std::env::args_os().collect();
    require(
        args.len() == 4,
        "usage: protected_qmp_flow NEW_PRIVATE_DIRECTORY TRUSTED_QEMU_EXECUTABLE PRIVATE_OVERLAY_DEVICE",
    )?;
    require(
        unsafe { libc::getuid() } == 0 && unsafe { libc::geteuid() } == 0,
        "fixture requires actual root primary credentials",
    )?;
    let scratch = PathBuf::from(&args[1]);
    DirBuilder::new().mode(0o700).create(&scratch)?;
    let state = scratch.join("registry");
    DirBuilder::new().mode(0o700).create(&state)?;
    let qmp = scratch.join("qmp.sock");
    let mut command = Command::new(&args[2]);
    command.args([
        "-accel",
        "tcg,thread=single",
        "-machine",
        "q35",
        "-m",
        "64",
        "-smp",
        "1",
        "-nodefaults",
        "-no-user-config",
        "-display",
        "none",
        "-no-reboot",
    ]);
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit());
    // All args belong solely to this explicitly invoked private root fixture.
    let launched = unsafe { PendingOriginal::create(16 << 20)?.launch(command, &qmp)? };
    let until = Instant::now() + Duration::from_secs(15);
    while !qmp.exists() {
        require(Instant::now() < until, "own QMP socket did not appear")?;
        std::thread::sleep(Duration::from_millis(10));
    }
    let enrolled = launched.enroll(4096)?;
    enrolled.ensure_live()?;
    println!(
        "ACTUAL QMP original pid={} start={} dev={} ino={} full_bytes={}",
        enrolled.pid(),
        enrolled.start(),
        enrolled.physical().host_device,
        enrolled.physical().host_inode,
        enrolled.physical().bar_bytes
    );
    let local = unsafe { ServiceLocalBackend::for_current_service(&ServicePolicy::linux_root())? };
    let device = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&args[3])?;
    // The surrounding private matching-kernel harness verifies module hash/source.
    let mut backend = unsafe { ProtectedBackend::from_verified_device(local, device)? };
    let source = Source {
        vm: "private-qmp-native-fixture".into(),
        windows_device: "SIMULATED-WINDOWS-PEER".into(),
        host_device: enrolled.physical().host_device,
        host_inode: enrolled.physical().host_inode,
        qemu_start: enrolled.start(),
        driver_generation: 1,
        bar_run: 1,
        bar_bytes: enrolled.physical().bar_bytes,
    };
    let flow = unsafe { canonical::run(enrolled.backing(), &mut backend, source, &state)? };
    println!("CANONICAL: durable context/grant and typed RW/RO projections created");
    let canonical::Flow {
        producer,
        consumer,
        owner,
        physical_offset,
        capacity,
        durable_exclusions,
    } = flow;
    require(
        producer.capacity() == capacity && consumer.capacity() == capacity,
        "typed capacity differs",
    )?;
    let writable = producer.address()?.cast_mut();
    let readable = consumer.address()?;
    unsafe {
        std::ptr::write_volatile(writable, 0x3d);
        std::ptr::write_volatile(writable.add(4096 + 100), 0x7b);
    }
    // Complete CPU writes before observing the second mapping. Volatile alone
    // is not a producer/consumer memory barrier for write-combined mappings.
    std::sync::atomic::fence(std::sync::atomic::Ordering::SeqCst);
    require(
        unsafe { std::ptr::read_volatile(readable) } == 0x3d
            && unsafe { std::ptr::read_volatile(readable.add(4096 + 100)) } == 0x7b,
        "canonical RW and RO do not share exact original pages",
    )?;
    require(
        unsafe {
            libc::mprotect(
                readable.cast_mut().cast(),
                capacity as usize,
                libc::PROT_READ | libc::PROT_WRITE,
            )
        } < 0,
        "canonical RO mapping upgraded to write",
    )?;
    require(
        unsafe {
            libc::mprotect(
                readable.cast_mut().cast(),
                capacity as usize,
                libc::PROT_READ | libc::PROT_EXEC,
            )
        } < 0,
        "canonical RO mapping upgraded to execute",
    )?;
    println!("CANONICAL: shared original bytes and immutable RO write/execute denial verified");
    let consumer_monitor = consumer.into_monitor();
    retired(&consumer_monitor)?;
    println!("CANONICAL: exact consumer monitor retired");
    let mut pipe = [-1; 2];
    require(
        unsafe { libc::pipe2(pipe.as_mut_ptr(), libc::O_CLOEXEC) } == 0,
        "fixture pipe failed",
    )?;
    let child = unsafe { libc::fork() };
    require(child >= 0, "fixture fork failed")?;
    if child == 0 {
        // Protected native fixture alias only. No Rust allocator, locks, registry or
        // service API runs after fork. The actual VMA/data references survive.
        unsafe {
            libc::close(pipe[1]);
            let mut byte = 0u8;
            let n = libc::read(pipe[0], (&mut byte as *mut u8).cast(), 1);
            let valid = n == 1 && std::ptr::read_volatile(writable) == 0x3d;
            libc::_exit(if valid { 0 } else { 21 });
        }
    }
    unsafe {
        libc::close(pipe[0]);
    }
    let producer_monitor = producer.into_monitor();
    require(
        producer_monitor.native_state()? == NativeExportState::Live,
        "surviving child alias was falsely retired",
    )?;
    let byte = 1u8;
    require(
        unsafe { libc::write(pipe[1], (&byte as *const u8).cast(), 1) } == 1,
        "fixture child release failed",
    )?;
    unsafe {
        libc::close(pipe[1]);
    }
    let mut status = 0;
    require(
        unsafe { libc::waitpid(child, &mut status, 0) } == child
            && libc::WIFEXITED(status)
            && libc::WEXITSTATUS(status) == 0,
        "protected alias child failed",
    )?;
    // Actual kernel completion is checked independently after child exit.
    retired(&producer_monitor)?;
    println!(
        "CANONICAL: surviving child alias kept producer LIVE; exact monitor retired after final alias"
    );
    require(
        owner.exclusions()? == durable_exclusions,
        "canonical exclusion changed after native retirement",
    )?;
    // Mutating our own disposable QEMU status must invalidate native source
    // liveness without treating either source death or pause as grant retirement.
    enrolled.pause_owned_source()?;
    require(
        enrolled.ensure_live().is_err(),
        "paused QMP source remained enrolled live",
    )?;
    require(
        enrolled
            .backing()
            .initialize_zeroed(physical_offset, 4096)
            .is_err(),
        "source invalidation allowed original mutation",
    )?;
    println!("CANONICAL: paused original source refused further mutation");
    // Canonical retained guards intentionally need not drop this backing. Use
    // explicit controller authority after both actual kernel monitors retired;
    // do not weaken retention merely to make a Child destructor run.
    unsafe {
        enrolled.shutdown_owned_fixture_source()?;
    }
    require(
        enrolled.ensure_live().is_err(),
        "exited source remained live",
    )?;
    require(
        enrolled
            .backing()
            .initialize_zeroed(physical_offset, 4096)
            .is_err(),
        "exited source allowed original mutation",
    )?;
    require(
        owner.exclusions()? == durable_exclusions,
        "controller cleanup changed canonical exclusions",
    )?;
    println!(
        "CANONICAL: retained-channel quit and exact pidfd/Child wait completed; dead source refused, exclusions retained"
    );
    drop((owner, producer_monitor, consumer_monitor));
    // Reopen the protected snapshot after its exclusive context lock ends.
    // enrolled still retains the exact original memfd and native owner.
    let reopened = unsafe { wc_broker::bar_registry::unix::DirectoryStore::open(&state)? };
    let reopened = wc_broker::bar_registry::Registry::open(
        reopened,
        enrolled.physical(),
        wc_broker::service_control::Origin::Linux,
    )?;
    require(
        reopened.exclusions() == durable_exclusions,
        "durable snapshot lost exclusion after native retirement",
    )?;
    drop(reopened);
    drop((backend, enrolled));
    println!(
        "CANONICAL NATIVE PASS: actual QMP inherited original -> durable registry/context/grant -> typed producerRW/consumerRO -> surviving native alias LIVE -> exact monitors RETIRED; durable exclusions retained {:?}. SIMULATED remote bootstrap only; no GPU/crossVM/release",
        durable_exclusions
    );
    // Registry evidence remains in this private directory. No deletion/reset API.
    Ok(())
}
#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("private protected QMP fixture requires Linux");
    std::process::exit(1)
}
