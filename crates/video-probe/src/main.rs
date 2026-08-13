//! What can this machine's drivers actually decode, and can one of them decode
//! a frame through a forwarded Vulkan?
//!
//! Two subcommands, deliberately in one binary so that the same process, the
//! same loader and the same `VK_DRIVER_FILES` answer both questions:
//!
//! ```text
//! video-probe caps                       what every device in reach reports
//! video-probe decode <in.264> <out.nv12> one frame, on the device it picks
//! ```
//!
//! Nothing here knows about `shared_vulkan`. It is an ordinary Vulkan program,
//! which is the point: pointing `VK_DRIVER_FILES` at the generated ICD manifest
//! is the whole difference between "decoded on the driver" and "decoded through
//! the forwarder", and a program that had a forwarding mode would be able to
//! cheat.

mod caps;
mod decode;
mod h264;

use anyhow::{Result, bail};

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let cmd = args.get(1).map(String::as_str).unwrap_or("caps");
    match cmd {
        "caps" => caps::run(),
        "decode" => {
            let Some(input) = args.get(2) else {
                bail!("usage: video-probe decode <in.264> [out.nv12]");
            };
            decode::run(input, args.get(3).map(String::as_str))
        }
        "--help" | "-h" | "help" => {
            println!("usage: video-probe [caps | decode <in.264> [out.nv12]]");
            Ok(())
        }
        other => bail!("unknown subcommand {other:?}; try caps or decode"),
    }
}
