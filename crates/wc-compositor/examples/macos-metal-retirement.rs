//! Cross-build on the build host. This probe opens no windows.
#[cfg(target_os = "macos")]
fn main() {
    wc_compositor::probe_metal_retirement().expect("Metal source retirement and immutable output");
    println!("Metal padded-source copies completed before source reuse; both immutable outputs match");
}
#[cfg(not(target_os = "macos"))]
fn main() { eprintln!("this probe requires macOS"); }
