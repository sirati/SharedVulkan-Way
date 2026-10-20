//! Opt-in, bounded observation of video image/export decisions. No request or
//! response is changed. `SV_VIDEO_TRACE=1` writes at most 2048 records per
//! process, each at most 8192 payload bytes. Any omission is explicit; a
//! truncated or exhausted trace cannot establish a complete tuple.

use std::fmt::{self, Write as _};
use std::io::Write as _;
use std::sync::{OnceLock, atomic::{AtomicUsize, Ordering}};

const RECORD_LIMIT: usize = 2048;
const BYTE_LIMIT: usize = 8192;

struct Bounded {
    text: String,
    truncated: bool,
}

impl fmt::Write for Bounded {
    fn write_str(&mut self, value: &str) -> fmt::Result {
        let available = BYTE_LIMIT.saturating_sub(self.text.len());
        if value.len() <= available {
            self.text.push_str(value);
            return Ok(());
        }
        let mut end = available;
        while !value.is_char_boundary(end) {
            end -= 1;
        }
        self.text.push_str(&value[..end]);
        self.truncated = true;
        Err(fmt::Error)
    }
}

fn bounded(args: fmt::Arguments<'_>) -> Bounded {
    let mut out = Bounded { text: String::new(), truncated: false };
    let _ = out.write_fmt(args);
    out
}

pub fn enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| std::env::var_os("SV_VIDEO_TRACE").is_some_and(|s| s == "1"))
}

/// Diagnostics stay inert unless explicitly enabled. Formatting is bounded
/// even for long nested pNext vectors; stderr failures are ignored.
pub fn event(topic: &str, args: fmt::Arguments<'_>) {
    static COUNT: AtomicUsize = AtomicUsize::new(0);
    if !enabled() {
        return;
    }
    let Ok(sequence) = COUNT.fetch_update(Ordering::Relaxed, Ordering::Relaxed,
        |n| if n <= RECORD_LIMIT { Some(n + 1) } else { None }) else {
        return;
    };
    let pid = std::process::id();
    // SAFETY: a getter. Linux gettid identifies the actual calling thread.
    #[cfg(target_os = "linux")]
    let tid = unsafe { libc::syscall(libc::SYS_gettid) } as u64;
    #[cfg(not(target_os = "linux"))]
    let tid = 0u64;
    let mut stderr = std::io::stderr().lock();
    if sequence == RECORD_LIMIT {
        let _ = writeln!(stderr, "sv-video-trace pid={pid} tid={tid} seq={sequence} TRACE_EXHAUSTED limit={RECORD_LIMIT}");
        return;
    }
    let out = bounded(args);
    let _ = writeln!(stderr, "sv-video-trace pid={pid} tid={tid} seq={sequence} {topic} {}{}", out.text,
        if out.truncated { " TRACE_TRUNCATED" } else { "" });
}

#[cfg(test)]
mod tests {
    use super::*;

    // Invoked in a separate test process so opt-in state never races other
    // tests and no process-global environment mutation is needed.
    #[test]
    fn emitter_fixture() {
        if std::env::var_os("SV_VIDEO_TRACE_TEST_EMIT").is_none() {
            return;
        }
        // SAFETY: getter, matching the emitter's Linux TID receipt.
        #[cfg(target_os = "linux")]
        let tid = unsafe { libc::syscall(libc::SYS_gettid) } as u64;
        #[cfg(not(target_os = "linux"))]
        let tid = 0u64;
        event("test-tuple", format_args!("expected_tid={tid} format=1000156003 tiling=0 usage=0xc00 flags=0x8 export=0x1"));
        let large = "€".repeat(BYTE_LIMIT);
        event("test-bounded", format_args!("{large}"));
        for _ in 0..RECORD_LIMIT + 10 {
            event("test-limit", format_args!("bounded"));
        }
    }

    #[test]
    fn actual_emitter_is_opt_in_bounded_and_has_exact_pid_tid() {
        let run = |enabled: &str| {
            let child = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "video_trace::tests::emitter_fixture", "--nocapture"])
                .env("SV_VIDEO_TRACE", enabled)
                .env("SV_VIDEO_TRACE_TEST_EMIT", "1")
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .spawn().unwrap();
            let pid = child.id();
            let output = child.wait_with_output().unwrap();
            assert!(output.status.success());
            (pid, String::from_utf8(output.stderr).unwrap())
        };
        let (_, disabled) = run("0");
        assert!(disabled.is_empty());
        let (pid, enabled) = run("1");
        let lines: Vec<_> = enabled.lines().collect();
        assert_eq!(lines.len(), RECORD_LIMIT + 1);
        assert_eq!(enabled.matches("TRACE_EXHAUSTED").count(), 1);
        assert_eq!(enabled.matches("TRACE_TRUNCATED").count(), 1);
        assert!(lines.iter().all(|l| l.starts_with(&format!("sv-video-trace pid={pid} tid="))));
        assert!(lines.iter().all(|l| l.len() <= BYTE_LIMIT + 256));
        let first = lines[0];
        let tid = first.split(" tid=").nth(1).unwrap().split_whitespace().next().unwrap();
        assert!(first.contains(&format!("expected_tid={tid}")));
        assert!(first.contains("usage=0xc00 flags=0x8 export=0x1"));
    }

    #[test]
    fn nested_formatting_is_bounded_and_marks_omission() {
        let values = vec!["large"; BYTE_LIMIT];
        let out = bounded(format_args!("{:?}", values));
        assert!(out.truncated);
        assert!(out.text.len() <= BYTE_LIMIT);
        let unicode = "€".repeat(BYTE_LIMIT);
        let out = bounded(format_args!("{unicode}"));
        assert!(out.truncated);
        assert!(out.text.len() <= BYTE_LIMIT);
        assert!(out.text.is_char_boundary(out.text.len()));
    }

    #[test]
    fn exact_video_image_tuple_is_read_only_and_complete() {
        use sv_proto as p;
        let mut image = p::VkImageCreateInfo::default();
        image.format = p::VkFormat(1_000_156_003);
        image.tiling = p::VkImageTiling(0);
        image.usage = p::VkImageUsageFlags(0xc00);
        image.flags = p::VkImageCreateFlags(8);
        image.extent.width = 1280;
        image.extent.height = 720;
        let mut profile = p::VkVideoProfileInfoKHR::default();
        profile.video_codec_operation = p::VkVideoCodecOperationFlagBitsKHR(1);
        profile.chroma_subsampling = p::VkVideoChromaSubsamplingFlagsKHR(2);
        profile.luma_bit_depth = p::VkVideoComponentBitDepthFlagsKHR(1);
        profile.chroma_bit_depth = p::VkVideoComponentBitDepthFlagsKHR(1);
        let mut h264 = p::VkVideoDecodeH264ProfileInfoKHR::default();
        h264.std_profile_idc = p::StdVideoH264ProfileIdc(100);
        h264.picture_layout = p::VkVideoDecodeH264PictureLayoutFlagBitsKHR(1);
        profile.p_next.push(p::PNextLink::VkVideoDecodeH264ProfileInfoKHR(Box::new(h264)));
        let mut profiles = p::VkVideoProfileListInfoKHR::default();
        profiles.profile_count = 1;
        profiles.p_profiles.push(profile);
        image.p_next.push(p::PNextLink::VkVideoProfileListInfoKHR(Box::new(profiles)));
        let mut list = p::VkImageFormatListCreateInfo::default();
        list.view_format_count = 2;
        list.p_view_formats = vec![p::VkFormat(9), p::VkFormat(16)];
        image.p_next.push(p::PNextLink::VkImageFormatListCreateInfo(Box::new(list)));
        let mut external = p::VkExternalMemoryImageCreateInfo::default();
        external.handle_types = p::VkExternalMemoryHandleTypeFlags(1);
        image.p_next.push(p::PNextLink::VkExternalMemoryImageCreateInfo(Box::new(external)));
        let before = image.clone();
        let out = bounded(format_args!("{image:?}"));
        assert_eq!(image, before);
        assert!(!out.truncated);
        for field in ["1000156003", "1280", "720", "tiling", "usage", "flags", "p_next", "std_profile_idc: StdVideoH264ProfileIdc(100)", "picture_layout", "p_view_formats", "handle_types"] {
            assert!(out.text.contains(field), "missing {field}");
        }
    }
}
