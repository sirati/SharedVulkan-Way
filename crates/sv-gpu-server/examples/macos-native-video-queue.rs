//! Coordinated native hardware fixture. Build on vmhost, run only in an approved window.
#[cfg(target_os = "macos")]
fn main() {
    let cancel_waits = std::env::args().any(|a| a == "--cancel-waits");
    let sync2 = std::env::args().any(|a| a == "--sync2");
    let wire_recording = std::env::args().any(|a| a == "--wire-recording");
    let result = if std::env::args().any(|a| a == "--advanced-pcm") {
        sv_gpu_server::native_video_queue_fixture::run_advanced(sync2)
    } else if cancel_waits {
        sv_gpu_server::native_video_queue_fixture::run_cancel_waits(sync2)
    } else {
        sv_gpu_server::native_video_queue_fixture::run_mode(sync2, wire_recording)
    };
    match result {
        Ok(()) => {
            println!(
                "PASS native queue fixture sync2={sync2} wire_recording={wire_recording} cancel_waits={cancel_waits}"
            )
        }
        Err(error) => {
            eprintln!("FAIL native queue fixture: {error}");
            // The fixture returns errors only after proving safe owner release.
            // Unproven cases retain their graph inside the controller.
            std::process::exit(1);
        }
    }
}
#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("macos-native-video-queue requires macOS");
    std::process::exit(77);
}
