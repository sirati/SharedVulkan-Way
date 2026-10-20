//! One-shot native portal action, after the portal's per-request consent.
//! Uses the same URL validation and Launch Services checks as the compositor.
#[cfg(target_os = "macos")]
fn main() -> std::process::ExitCode {
    use wc_hostint::host::Opener;
    let arguments: Vec<_> = std::env::args().skip(1).collect();
    let result = if arguments.len() == 1 {
        wc_hostint::openurl::validate_url(
            &arguments[0],
            &["https".into(), "http".into(), "mailto".into()],
        )
        .and_then(|url| {
            // Empty browser selects the host's default, preserving its choices.
            let mut opener = wc_hostplat::mac::open::MacOpener {
                browser: String::new(),
                before_open: None,
            };
            opener.open_url(&url)
        })
    } else {
        Err("expected one URL".into())
    };
    match result {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{error}");
            std::process::ExitCode::FAILURE
        }
    }
}

#[cfg(not(target_os = "macos"))]
fn main() -> std::process::ExitCode {
    eprintln!("wc-portal-open requires macOS");
    std::process::ExitCode::FAILURE
}
