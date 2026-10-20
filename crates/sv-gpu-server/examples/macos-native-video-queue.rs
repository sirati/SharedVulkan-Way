//! Coordinated native hardware fixture. Build on vmhost, run only in an approved window.
#[cfg(target_os = "macos")]
fn main() {
    std::thread::spawn(|| {
        std::thread::sleep(std::time::Duration::from_secs(30));
        eprintln!("native queue fixture watchdog expired");
        std::process::exit(1);
    });
    let sync2 = std::env::args().any(|a| a == "--sync2");
    match sv_gpu_server::native_video_queue_fixture::run(sync2) {
        Ok(()) => println!("PASS native queue fixture sync2={sync2}"),
        Err(error) => {
            eprintln!("FAIL native queue fixture: {error}");
            // Native owners must not unwind after an uncertain completion.
            std::process::exit(1);
        }
    }
}
#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("macos-native-video-queue requires macOS");
    std::process::exit(77);
}
