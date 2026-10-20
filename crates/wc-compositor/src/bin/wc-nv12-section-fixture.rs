#[cfg(target_os = "macos")]
fn main() {
    if let Err(error) = wc_compositor::run_headless_section_fixture() {
        eprintln!("NV12_SECTION_SERVICE_ERROR: {error}");
        std::process::exit(2);
    }
}
#[cfg(not(target_os = "macos")]
fn main() { eprintln!("NV12_SECTION_SERVICE_ERROR: macOS only"); std::process::exit(2); }
