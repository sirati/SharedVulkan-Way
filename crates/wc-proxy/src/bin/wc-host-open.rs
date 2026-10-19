//! `wc-host-open [--with TOKEN] TARGET...`: open URLs and files on the host
//! desktop (docs/waycompose-design.md §106.4). The session's `xdg-open` and
//! the `Exec=` of its host-application desktop entries. It asks the proxy
//! over `$WC_HOSTINT_SOCK`, a local socket; the proxy carries the request on
//! the session's ring slot. See `wc_proxy::hostint::open_cli`.

fn main() -> std::process::ExitCode {
    #[cfg(unix)]
    {
        let args: Vec<String> = std::env::args().skip(1).collect();
        wc_proxy::hostint::open_cli(&args)
    }
    #[cfg(not(unix))]
    {
        eprintln!("wc-host-open runs in the Linux session");
        std::process::ExitCode::from(2)
    }
}
