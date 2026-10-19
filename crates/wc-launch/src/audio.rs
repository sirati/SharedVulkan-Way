//! The session's sound, played on the guest (docs/waycompose-design.md §86).
//!
//! The VM host has no speakers of its own to offer: on testhost QEMU's
//! emulated HDA owns `hw:0,0` and the person at the laptop hears whatever the
//! *guest* plays. So `wc-session` gives each session a PulseAudio with one null
//! sink, and its `audio` verb streams that sink's monitor down an ssh channel
//! as raw PCM. This is the other end: read the channel, hand it to waveOut.
//!
//! The format is fixed because both ends are ours: s16le, 48 kHz, stereo.
//!
//! **Bounded latency, by dropping.** The host's null sink runs on the host's
//! clock and the guest's device on its own, so one of them is always a little
//! fast. At most [`BUFFERS`] chunks of [`CHUNK_MS`] ms are queued; a chunk that
//! arrives with every buffer still queued is dropped rather than queued behind
//! them, so the delay never grows past `BUFFERS * CHUNK_MS`. A device that runs
//! fast drains the queue and plays nothing until the next chunk (an underrun),
//! which is also bounded. Both are counted and logged.
//!
//! What this does not do is tell Firefox how late the speakers are. Firefox's
//! audio clock is the null sink's, so the picture leads the sound by this
//! queue plus the ssh hop. §86 says what was measured of that and what was not.

/// Sample rate, channels, bytes per frame: `wc-session`'s `AUDIO_RATE` and
/// `parec --format=s16le --channels=2`.
pub const RATE: u32 = 48_000;
pub const CHANNELS: u16 = 2;
pub const FRAME_BYTES: usize = 4;
/// One waveOut buffer: 20 ms.
pub const CHUNK_MS: u32 = 20;
pub const CHUNK_BYTES: usize = (RATE * CHUNK_MS / 1000) as usize * FRAME_BYTES;
/// The most audio ever queued at the device: 8 × 20 ms = 160 ms.
pub const BUFFERS: usize = 8;

/// waveOutSetVolume's DWORD: left in the low word, right in the high, each
/// 0..=0xFFFF. `pct` is clamped to 100.
pub fn volume_word(pct: u8) -> u32 {
    let v = (u32::from(pct.min(100)) * 0xFFFF) / 100;
    v | (v << 16)
}

/// What a player did, for the session log.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Stats {
    pub chunks_played: u64,
    pub chunks_dropped: u64,
    pub underruns: u64,
    /// Chunks that were not all zero. Silence is what a null sink's monitor
    /// sends when nothing is playing, so this is the count of real sound.
    pub chunks_audible: u64,
}

impl Stats {
    pub fn line(&self) -> String {
        let secs = |n: u64| n as f64 * f64::from(CHUNK_MS) / 1000.0;
        format!(
            "audio: {:.1} s played ({:.1} s of it not silent), {} chunk(s) dropped, {} underrun(s)",
            secs(self.chunks_played),
            secs(self.chunks_audible),
            self.chunks_dropped,
            self.underruns
        )
    }
}

/// Is any sample in this chunk non-zero?
pub fn audible(chunk: &[u8]) -> bool {
    chunk.iter().any(|b| *b != 0)
}

#[cfg(windows)]
#[allow(unused_imports)]
pub use win::{play, play_credited};

#[cfg(windows)]
mod win {
    use super::*;
    use std::io::Read;
    use std::time::Duration;
    use windows_sys::Win32::Media::Audio::{
        CALLBACK_NULL, HWAVEOUT, WAVE_FORMAT_PCM, WAVE_MAPPER, WAVEFORMATEX, WAVEHDR, WHDR_DONE,
        waveOutClose, waveOutOpen, waveOutPrepareHeader, waveOutReset, waveOutSetVolume,
        waveOutUnprepareHeader, waveOutWrite,
    };

    const WHDR_PREPARED: u32 = 2;
    const HDR: u32 = std::mem::size_of::<WAVEHDR>() as u32;

    /// Play `src` until it ends. `say` gets the open, a line every ten seconds
    /// and the totals. Returns the totals, or why the device would not open.
    pub fn play(mut src: impl Read, volume_pct: u8, mut say: impl FnMut(&str)) -> Result<Stats, String> {
        let fmt = WAVEFORMATEX {
            wFormatTag: WAVE_FORMAT_PCM as u16,
            nChannels: CHANNELS,
            nSamplesPerSec: RATE,
            nAvgBytesPerSec: RATE * FRAME_BYTES as u32,
            nBlockAlign: FRAME_BYTES as u16,
            wBitsPerSample: 16,
            cbSize: 0,
        };
        let mut h: HWAVEOUT = std::ptr::null_mut();
        // SAFETY: a valid out pointer and format; CALLBACK_NULL, so nothing is
        // called back into this process.
        let rc = unsafe { waveOutOpen(&mut h, WAVE_MAPPER, &fmt, 0, 0, CALLBACK_NULL) };
        if rc != 0 {
            return Err(format!("waveOutOpen failed: MMRESULT {rc}"));
        }
        // SAFETY: an open handle.
        unsafe { waveOutSetVolume(h, volume_word(volume_pct)) };
        say(&format!(
            "audio: playing s16le {RATE} Hz stereo through the default device, {BUFFERS} x {CHUNK_MS} ms queue, volume {volume_pct}%"
        ));

        // Buffers and headers boxed so their addresses never move while the
        // device holds them.
        let mut data: Vec<Box<[u8]>> = (0..BUFFERS).map(|_| vec![0u8; CHUNK_BYTES].into_boxed_slice()).collect();
        let mut hdrs: Vec<Box<WAVEHDR>> = (0..BUFFERS).map(|_| Box::new(WAVEHDR::default())).collect();
        let mut stats = Stats::default();
        let mut chunk = vec![0u8; CHUNK_BYTES];
        let mut next_line = 0u64;
        let mut started = false;
        loop {
            if src.read_exact(&mut chunk).is_err() {
                break;
            }
            let loud = audible(&chunk);
            // A free header: never used, or done.
            let free = hdrs.iter().position(|hd| hd.dwFlags & WHDR_PREPARED == 0 || hd.dwFlags & WHDR_DONE != 0);
            let Some(i) = free else {
                stats.chunks_dropped += 1;
                continue;
            };
            let busy = hdrs.iter().filter(|hd| hd.dwFlags & WHDR_PREPARED != 0 && hd.dwFlags & WHDR_DONE == 0).count();
            if started && busy == 0 {
                stats.underruns += 1;
            }
            started = true;
            let hd = &mut hdrs[i];
            if hd.dwFlags & WHDR_PREPARED != 0 {
                // SAFETY: a header this device has finished with.
                unsafe { waveOutUnprepareHeader(h, &mut **hd, HDR) };
            }
            data[i].copy_from_slice(&chunk);
            **hd = WAVEHDR {
                lpData: data[i].as_mut_ptr(),
                dwBufferLength: CHUNK_BYTES as u32,
                ..WAVEHDR::default()
            };
            // SAFETY: the header and its buffer are boxed and outlive the
            // device's use of them (reset and unprepare below, before drop).
            unsafe {
                waveOutPrepareHeader(h, &mut **hd, HDR);
                waveOutWrite(h, &mut **hd, HDR);
            }
            stats.chunks_played += 1;
            if loud {
                stats.chunks_audible += 1;
            }
            if stats.chunks_played >= next_line {
                say(&stats.line());
                next_line = stats.chunks_played + 500; // every 10 s of sound
            }
        }
        // SAFETY: stop the device, then give every header back before the
        // boxes go.
        unsafe {
            waveOutReset(h);
            for hd in hdrs.iter_mut() {
                if hd.dwFlags & WHDR_PREPARED != 0 {
                    waveOutUnprepareHeader(h, &mut **hd, HDR);
                }
            }
            waveOutClose(h);
        }
        say(&format!("{} (the stream ended)", stats.line()));
        Ok(stats)
    }

    /// §105, the sound ring's device side (§103.16): play what arrives on
    /// `rx`, in whatever pieces it comes, and give back `credit` for every
    /// chunk handed to the device. The sender sends no more than it was
    /// granted, so nothing is ever dropped here: a device that runs slow
    /// slows the session's sink instead. Ends when `rx` closes.
    pub fn play_credited(
        rx: std::sync::mpsc::Receiver<Vec<u8>>,
        volume_pct: u8,
        mut say: impl FnMut(&str),
        mut credit: impl FnMut(u32),
    ) -> Result<Stats, String> {
        let fmt = WAVEFORMATEX {
            wFormatTag: WAVE_FORMAT_PCM as u16,
            nChannels: CHANNELS,
            nSamplesPerSec: RATE,
            nAvgBytesPerSec: RATE * FRAME_BYTES as u32,
            nBlockAlign: FRAME_BYTES as u16,
            wBitsPerSample: 16,
            cbSize: 0,
        };
        let mut h: HWAVEOUT = std::ptr::null_mut();
        // SAFETY: as in `play`.
        let rc = unsafe { waveOutOpen(&mut h, WAVE_MAPPER, &fmt, 0, 0, CALLBACK_NULL) };
        if rc != 0 {
            return Err(format!("waveOutOpen failed: MMRESULT {rc}"));
        }
        // SAFETY: an open handle.
        unsafe { waveOutSetVolume(h, volume_word(volume_pct)) };
        say(&format!(
            "audio: playing the sound ring's output stream, s16le {RATE} Hz stereo, {BUFFERS} x {CHUNK_MS} ms queue, volume {volume_pct}%, paced by credit"
        ));
        let mut data: Vec<Box<[u8]>> = (0..BUFFERS).map(|_| vec![0u8; CHUNK_BYTES].into_boxed_slice()).collect();
        let mut hdrs: Vec<Box<WAVEHDR>> = (0..BUFFERS).map(|_| Box::new(WAVEHDR::default())).collect();
        let mut stats = Stats::default();
        let mut pending: std::collections::VecDeque<u8> = std::collections::VecDeque::new();
        let mut next_line = 0u64;
        let mut open = true;
        let mut started = false;
        while open || pending.len() >= CHUNK_BYTES {
            // Take what has arrived; wait a little only when there is nothing
            // to hand the device.
            let wait = if pending.len() >= CHUNK_BYTES { Duration::ZERO } else { Duration::from_millis(5) };
            match rx.recv_timeout(wait) {
                Ok(b) => {
                    pending.extend(b);
                    while let Ok(b) = rx.try_recv() {
                        pending.extend(b);
                    }
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => open = false,
            }
            let busy = hdrs.iter().filter(|hd| hd.dwFlags & WHDR_PREPARED != 0 && hd.dwFlags & WHDR_DONE == 0).count();
            if started && busy == 0 && pending.len() < CHUNK_BYTES {
                stats.underruns += 1;
                started = false;
            }
            while pending.len() >= CHUNK_BYTES {
                let Some(i) = hdrs.iter().position(|hd| hd.dwFlags & WHDR_PREPARED == 0 || hd.dwFlags & WHDR_DONE != 0) else {
                    // Every buffer is at the device: wait for one, not drop.
                    std::thread::sleep(Duration::from_millis(2));
                    break;
                };
                let hd = &mut hdrs[i];
                if hd.dwFlags & WHDR_PREPARED != 0 {
                    // SAFETY: a header this device has finished with.
                    unsafe { waveOutUnprepareHeader(h, &mut **hd, HDR) };
                }
                for (d, s) in data[i].iter_mut().zip(pending.drain(..CHUNK_BYTES)) {
                    *d = s;
                }
                let loud = audible(&data[i]);
                **hd = WAVEHDR { lpData: data[i].as_mut_ptr(), dwBufferLength: CHUNK_BYTES as u32, ..WAVEHDR::default() };
                // SAFETY: as in `play`.
                unsafe {
                    waveOutPrepareHeader(h, &mut **hd, HDR);
                    waveOutWrite(h, &mut **hd, HDR);
                }
                started = true;
                credit(CHUNK_BYTES as u32);
                stats.chunks_played += 1;
                if loud {
                    stats.chunks_audible += 1;
                }
                if stats.chunks_played >= next_line {
                    say(&stats.line());
                    next_line = stats.chunks_played + 500;
                }
            }
        }
        // SAFETY: as in `play`.
        unsafe {
            waveOutReset(h);
            for hd in hdrs.iter_mut() {
                if hd.dwFlags & WHDR_PREPARED != 0 {
                    waveOutUnprepareHeader(h, &mut **hd, HDR);
                }
            }
            waveOutClose(h);
        }
        say(&format!("{} (the stream ended)", stats.line()));
        Ok(stats)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_chunk_is_twenty_milliseconds_of_the_format_wc_session_sends() {
        assert_eq!(CHUNK_BYTES, 960 * 4);
        assert_eq!(BUFFERS as u32 * CHUNK_MS, 160, "the bound on queued audio");
    }

    #[test]
    fn volume_is_both_channels_and_clamped() {
        assert_eq!(volume_word(100), 0xFFFF_FFFF);
        assert_eq!(volume_word(0), 0);
        assert_eq!(volume_word(250), 0xFFFF_FFFF);
        assert_eq!(volume_word(50) & 0xFFFF, volume_word(50) >> 16);
    }

    #[test]
    fn silence_is_not_counted_as_sound() {
        assert!(!audible(&[0; 16]));
        assert!(audible(&[0, 0, 1, 0]));
        let s = Stats { chunks_played: 500, chunks_audible: 50, chunks_dropped: 2, underruns: 1 };
        assert_eq!(s.line(), "audio: 10.0 s played (1.0 s of it not silent), 2 chunk(s) dropped, 1 underrun(s)");
    }
}
