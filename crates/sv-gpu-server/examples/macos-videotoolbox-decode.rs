#![allow(unexpected_cfgs)]
//! Headless real hardware H.264 decode fixture. CPU readback exists only here.
//! Run the signed macOS example in a coordinated test window. It exits after
//! thirty seconds even if the platform decoder stops responding.
#[cfg(all(target_os = "macos", sv_nv12_sampling_fixture))]
#[path = "support/vt_joined_sample.rs"]
mod joined_sample;

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("macos-videotoolbox-decode requires macOS and VideoToolbox hardware");
    std::process::exit(77);
}

#[cfg(target_os = "macos")]
mod fixture {
    use std::{
        ffi::c_void,
        fs::{File, OpenOptions},
        io::Write,
        ops::Range,
        ptr,
        sync::Arc,
        time::{Duration, SystemTime, UNIX_EPOCH},
    };
    use sv_gpu_server::video_nv12::{
        macos::{Picture, Range as PixelRange},
        videotoolbox::{CompressedBytes, Decoder},
    };
    const VIDEO_STREAM: &[u8] = include_bytes!("assets/videotoolbox-h264/progressive-64x50.h264");
    const FULL_STREAM: &[u8] = include_bytes!("assets/videotoolbox-h264/full-range-64x50.h264");
    const WIDTH: usize = 64;
    const HEIGHT: usize = 50;
    const FRAMES: usize = 8;
    #[link(name = "CoreVideo", kind = "framework")]
    unsafe extern "C" {
        fn CVPixelBufferLockBaseAddress(buffer: *mut c_void, flags: u64) -> i32;
        fn CVPixelBufferUnlockBaseAddress(buffer: *mut c_void, flags: u64) -> i32;
        fn CVPixelBufferGetBaseAddressOfPlane(buffer: *mut c_void, plane: usize) -> *mut c_void;
    }
    struct Mapping {
        pointer: *mut u8,
        length: usize,
        _file: File,
    }
    // The unlinked file has no other writer. Mutation is allowed only after the
    // last decoder-owned Arc has gone, by consuming the unique Mapping owner.
    unsafe impl Send for Mapping {}
    unsafe impl Sync for Mapping {}
    unsafe impl CompressedBytes for Mapping {
        fn bytes(&self) -> &[u8] {
            unsafe { std::slice::from_raw_parts(self.pointer, self.length) }
        }
    }
    impl Mapping {
        fn new(bytes: &[u8]) -> Result<Self, String> {
            let nonce = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_err(|e| e.to_string())?
                .as_nanos();
            let path =
                std::env::temp_dir().join(format!("sv-vt-source-{}-{nonce}", std::process::id()));
            let mut file = OpenOptions::new()
                .read(true)
                .write(true)
                .create_new(true)
                .open(&path)
                .map_err(|e| e.to_string())?;
            // Unlink immediately. Only this fd and its mapping can access input.
            std::fs::remove_file(&path).map_err(|e| e.to_string())?;
            file.write_all(bytes).map_err(|e| e.to_string())?;
            let pointer = unsafe {
                libc::mmap(
                    ptr::null_mut(),
                    bytes.len(),
                    libc::PROT_READ | libc::PROT_WRITE,
                    libc::MAP_SHARED,
                    std::os::fd::AsRawFd::as_raw_fd(&file),
                    0,
                )
            };
            if pointer == libc::MAP_FAILED {
                return Err(std::io::Error::last_os_error().to_string());
            }
            Ok(Self {
                pointer: pointer.cast(),
                length: bytes.len(),
                _file: file,
            })
        }
        fn clear_after_release(&mut self) {
            unsafe {
                ptr::write_bytes(self.pointer, 0, self.length);
            }
        }
    }
    impl Drop for Mapping {
        fn drop(&mut self) {
            unsafe {
                libc::munmap(self.pointer.cast(), self.length);
            }
        }
    }
    fn prefix(data: &[u8]) -> Option<usize> {
        if data.starts_with(&[0, 0, 0, 1]) {
            Some(4)
        } else if data.starts_with(&[0, 0, 1]) {
            Some(3)
        } else {
            None
        }
    }
    fn nals(data: &[u8]) -> Result<Vec<(u8, Range<usize>, Range<usize>)>, String> {
        let mut starts = Vec::new();
        let mut i = 0;
        while i + 3 <= data.len() {
            if let Some(size) = prefix(&data[i..]) {
                starts.push((i, size));
                i += size;
            } else {
                i += 1;
            }
        }
        if starts.first().map(|s| s.0) != Some(0) {
            return Err("fixture does not start with Annex B".into());
        }
        let mut result = Vec::new();
        for (index, &(start, size)) in starts.iter().enumerate() {
            let end = starts.get(index + 1).map(|s| s.0).unwrap_or(data.len());
            let payload = start + size..end;
            let first = *data.get(payload.start).ok_or("empty fixture NAL")?;
            if payload.is_empty() || first & 0x80 != 0 {
                return Err("invalid fixture NAL".into());
            }
            result.push((first & 31, start..end, payload));
        }
        Ok(result)
    }
    struct Locked<'a>(&'a Picture);
    impl Drop for Locked<'_> {
        fn drop(&mut self) {
            unsafe {
                CVPixelBufferUnlockBaseAddress(self.0.pixel_buffer(), 1);
            }
        }
    }
    fn verify(
        picture: &Picture,
        frame: usize,
        expected_storage_range: PixelRange,
        coded_output: bool,
        role: &str,
    ) -> Result<(), String> {
        let layout = picture.layout();
        if layout.width != WIDTH || ![HEIGHT, 64].contains(&layout.height) {
            return Err(format!(
                "frame {frame}: unexpected output dimensions {}x{}",
                layout.width, layout.height
            ));
        }
        if picture.range() != expected_storage_range {
            return Err(format!(
                "fixture {role} requires {expected_storage_range:?} NV12 storage, got {:?}",
                picture.range()
            ));
        }
        let status = unsafe { CVPixelBufferLockBaseAddress(picture.pixel_buffer(), 1) };
        if status != 0 {
            return Err(format!("readback lock failed {status}"));
        }
        let _lock = Locked(picture);
        let y =
            unsafe { CVPixelBufferGetBaseAddressOfPlane(picture.pixel_buffer(), 0) }.cast::<u8>();
        let uv =
            unsafe { CVPixelBufferGetBaseAddressOfPlane(picture.pixel_buffer(), 1) }.cast::<u8>();
        if y.is_null() || uv.is_null() {
            return Err("fixture pixel planes are not CPU accessible".into());
        }
        if coded_output && layout.height != 64 {
            return Err("coded mode requires all 64 output rows".into());
        }
        let checked_height = if coded_output { 64 } else { HEIGHT };
        let mut max_error = [0i16; 3];
        for row in 0..checked_height {
            for column in 0..WIDTH {
                let actual = unsafe { *y.add(row * layout.planes[0].stride + column) };
                let expected = 40 + frame * 12 + (column / 16) * 8 + (row.min(HEIGHT - 1) / 10) * 6;
                max_error[0] = max_error[0].max((i16::from(actual) - expected as i16).abs());
            }
        }
        for row in 0..checked_height / 2 {
            for column in 0..WIDTH / 2 {
                let pixel = unsafe { uv.add(row * layout.planes[1].stride + column * 2) };
                for (channel, expected) in [(1, 64 + frame * 5), (2, 192 - frame * 5)] {
                    let actual = unsafe { *pixel.add(channel - 1) };
                    max_error[channel] =
                        max_error[channel].max((i16::from(actual) - expected as i16).abs());
                }
            }
        }
        if max_error.iter().any(|n| *n > 4) {
            return Err(format!(
                "frame {frame}: Y/U/V maximum error {max_error:?} exceeds 4"
            ));
        }
        println!(
            "frame={frame} role={role} NV12={}x{} visible=64x50 storage_range={expected_storage_range:?} max_yuv_error={max_error:?}",
            layout.width, layout.height
        );
        Ok(())
    }
    pub fn run() -> Result<(), String> {
        let mut full_range = false;
        let mut coded_output = false;
        let mut copy_target = false;
        #[cfg(sv_nv12_sampling_fixture)]
        let mut joined_target = false;
        for argument in std::env::args().skip(1) {
            match argument.as_str() {
                "--full-range" => full_range = true,
                "--coded-output" => coded_output = true,
                "--copy-target" => copy_target = true,
                #[cfg(sv_nv12_sampling_fixture)]
                "--joined-target" => {
                    joined_target = true;
                    copy_target = true;
                }
                _ => {
                    return Err(
                        "usage: macos-videotoolbox-decode [--full-range] [--coded-output] [--copy-target]".into(),
                    );
                }
            }
        }
        let expected_range = if full_range {
            PixelRange::Full
        } else {
            PixelRange::Video
        };
        let stream = if full_range {
            FULL_STREAM
        } else {
            VIDEO_STREAM
        };
        eprintln!(
            "fixture stage=mode expected_range={expected_range:?} coded_output={coded_output} copy_target={copy_target}"
        );
        unsafe {
            libc::alarm(30);
        }
        eprintln!("fixture stage=parse stream bytes={}", stream.len());
        let parsed = nals(stream)?;
        let sps = parsed
            .iter()
            .find(|n| n.0 == 7)
            .ok_or("missing fixture SPS")?;
        let pps = parsed
            .iter()
            .find(|n| n.0 == 8)
            .ok_or("missing fixture PPS")?;
        for entry in &parsed {
            if entry.0 == 7 && stream[entry.2.clone()] != stream[sps.2.clone()]
                || entry.0 == 8 && stream[entry.2.clone()] != stream[pps.2.clone()]
            {
                return Err("fixture changes parameter sets".into());
            }
        }
        let slices: Vec<_> = parsed.iter().filter(|n| matches!(n.0, 1 | 5)).collect();
        if slices.len() != FRAMES || slices.iter().filter(|n| n.0 == 5).count() != 2 {
            return Err("fixture must contain two IDR and six P pictures".into());
        }
        eprintln!("fixture stage=map shared source");
        let source = Arc::new(Mapping::new(stream)?);
        // Allocate application-like storage before any decoded output exists.
        // Deliberately use video-range allocation metadata even for full-range
        // streams: copy_from_gpu must preserve raw samples without interpreting
        // storage's fourcc as the stream's sampling/conversion metadata.
        let target = if copy_target {
            let target_height = if coded_output { 64 } else { HEIGHT };
            eprintln!(
                "fixture stage=allocate fixed target extent={WIDTH}x{target_height} storage_range=Video stream_range={expected_range:?}"
            );
            Some(Picture::allocate_target(
                WIDTH,
                target_height,
                PixelRange::Video,
            )?)
        } else {
            None
        };
        #[cfg(sv_nv12_sampling_fixture)]
        let mut joined_probe = if joined_target {
            Some(super::joined_sample::Probe::new(Arc::clone(
                target.as_ref().unwrap(),
            ))?)
        } else {
            None
        };
        eprintln!("fixture stage=create hardware-required decoder");
        let mut decoder = if coded_output {
            Decoder::new_coded(&stream[sps.2.clone()], &stream[pps.2.clone()])?
        } else {
            Decoder::new(&stream[sps.2.clone()], &stream[pps.2.clone()])?
        };
        eprintln!(
            "fixture stage=decoder created coded={:?} display={:?}",
            decoder.coded_extent, decoder.display_extent
        );
        if decoder.output_range != expected_range {
            return Err(format!(
                "format range {:?} disagrees with fixture {expected_range:?}",
                decoder.output_range
            ));
        }
        if decoder.coded_extent != [64, 64]
            || decoder.display_extent != [64, 50]
            || decoder.crop_origin != [0, 0]
        {
            return Err(format!(
                "unexpected SPS geometry coded={:?} display={:?} origin={:?}",
                decoder.coded_extent, decoder.display_extent, decoder.crop_origin
            ));
        }
        let mut pictures = Vec::new();
        for (frame, slice) in slices.iter().enumerate() {
            decoder.update_parameters(&stream[sps.2.clone()], &stream[pps.2.clone()])?;
            eprintln!("fixture stage=submit frame={frame} nal_type={}", slice.0);
            let completion =
                decoder.submit_annex_b_tracked(source.clone(), slice.1.clone(), &[0])?;
            // Exercise drain without resetting references, so the following P
            // picture must use the preceding IDR/P reference in this session.
            eprintln!("fixture stage=drain frame={frame}");
            decoder.drain()?;
            eprintln!("fixture stage=receive frame={frame}");
            let picture = completion
                .picture
                .recv_timeout(Duration::from_secs(2))
                .map_err(|e| format!("frame {frame}: callback {e}"))??;
            eprintln!("fixture stage=wait source release frame={frame}");
            completion
                .input_released
                .recv_timeout(Duration::from_secs(2))
                .map_err(|e| {
                    format!("frame {frame}: compressed source did not release before fence: {e}")
                })?;
            if Arc::strong_count(&source) != 1 {
                return Err(format!(
                    "frame {frame}: source still retained after release event"
                ));
            }
            eprintln!("fixture stage=source released before fence frame={frame}");
            eprintln!("fixture stage=verify frame={frame}");
            verify(&picture, frame, expected_range, coded_output, "decoder")?;
            if let Some(target) = &target {
                eprintln!(
                    "fixture stage=copy fixed target frame={frame} source_range={expected_range:?} target_storage_range={:?}",
                    target.range()
                );
                // This fixture has exclusive producer ownership. Each previous
                // GPU copy has completed and its readback lock has been dropped.
                // The synchronous helper retains both pictures until Metal
                // completion; no reader can overlap the next overwrite.
                #[cfg(sv_nv12_sampling_fixture)]
                if let Some(probe) = joined_probe.as_mut() {
                    unsafe {
                        probe.copy_sample(Arc::clone(&picture), frame, full_range)?;
                    }
                } else {
                    unsafe {
                        target.copy_from_gpu(&picture)?;
                    }
                }
                #[cfg(not(sv_nv12_sampling_fixture))]
                unsafe {
                    target.copy_from_gpu(&picture)?;
                }
                verify(
                    target,
                    frame,
                    PixelRange::Video,
                    coded_output,
                    "fixed-target",
                )?;
            }
            pictures.push(picture);
        }
        eprintln!("fixture stage=final drain");
        decoder.drain()?;
        eprintln!("fixture stage=drop decoder");
        drop(decoder);
        eprintln!("fixture stage=decoder dropped");
        let references = Arc::strong_count(&source);
        let mut source = Arc::try_unwrap(source).map_err(|_| {
            format!("decoder retained {references} compressed source references after invalidate")
        })?;
        // Mutation occurs only after all callbacks, drain, session invalidation,
        // and proof that CoreMedia released every compressed-block owner.
        eprintln!("fixture stage=overwrite released source mmap");
        source.clear_after_release();
        if source.bytes().iter().any(|b| *b != 0) {
            return Err("source mutation did not reach mapping".into());
        }
        for (frame, picture) in pictures.iter().enumerate() {
            verify(
                picture,
                frame,
                expected_range,
                coded_output,
                "retained-decoder",
            )?;
        }
        if let Some(target) = &target {
            eprintln!(
                "fixture stage=verify fixed target after source overwrite stream_range={expected_range:?}"
            );
            verify(
                target,
                FRAMES - 1,
                PixelRange::Video,
                coded_output,
                "retained-fixed-target",
            )?;
            println!(
                "PASS one preallocated NV12 target reused for all {FRAMES} GPU copies without sample-range conversion; stream_range={expected_range:?} target_storage_range=Video"
            );
        }
        unsafe {
            libc::alarm(0);
        }
        println!(
            "PASS hardware-required VideoToolbox decoded 2 IDR + 6 P; shared mmap released; retained NV12 pictures unchanged after source overwrite"
        );
        Ok(())
    }
}
#[cfg(target_os = "macos")]
fn main() {
    eprintln!("fixture stage=main entered pid={}", std::process::id());
    if let Err(error) = fixture::run() {
        eprintln!("FAIL VideoToolbox fixture: {error}");
        std::process::exit(1);
    }
}
