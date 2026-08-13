//! Argument parsing for `wc-shmclient`. Everything else is the library — see
//! its docs for what this client is and why it has to exist.

use std::process::ExitCode;

use wc_shmclient::{run, Config};

fn main() -> ExitCode {
    let mut config = Config::default();

    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--partial-damage" => config.partial = true,
            "--frames" => config.frames = next_num(&mut it).unwrap_or(config.frames),
            "--width" => config.width = next_num(&mut it).unwrap_or(config.width),
            "--height" => config.height = next_num(&mut it).unwrap_or(config.height),
            "--title" => config.title = it.next().unwrap_or(config.title),
            "--app-id" => config.app_id = it.next().unwrap_or(config.app_id),
            // STAGE 32: change the app id after the window is up, which is what
            // makes the taskbar's late-identity path measurable at all.
            "--late-app-id" => config.late_app_id = it.next(),
            "--late-after" => config.late_after = next_num(&mut it).unwrap_or(config.late_after),
            "--seconds" => config.seconds = next_num(&mut it).unwrap_or(config.seconds),
            other if other.starts_with("--") => {
                eprintln!("wc-shmclient: unknown argument {other}");
                return ExitCode::from(2);
            }
            other => config.path_arg = Some(other.to_string()),
        }
    }

    run(config)
}

fn next_num<T: std::str::FromStr>(it: &mut impl Iterator<Item = String>) -> Option<T> {
    it.next().and_then(|s| s.parse().ok())
}
