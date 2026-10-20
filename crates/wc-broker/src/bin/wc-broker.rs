//! `wc-broker`: the broker as a program (§111). On Unix, `crate::daemon`
//! (on macOS the LaunchDaemon `org.waycompose.broker`, §111.9).

fn main() -> std::process::ExitCode {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        wc_broker::daemon::main(std::env::args().skip(1).collect())
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        eprintln!("wc-broker: this program is the Unix daemon; on Windows the broker is a service");
        std::process::ExitCode::from(2)
    }
}
