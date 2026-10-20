//! Hardware-only H.264 decoder. Compressed input stays pinned until CoreMedia
//! releases its block; decoded pixels are retained by `Picture` in the callback.
use super::macos::Picture;
use std::{
    collections::HashMap,
    ffi::c_void,
    ops::Range,
    ptr,
    sync::{Arc, Mutex, mpsc},
};
type Ref = *mut c_void;
#[repr(C)]
#[derive(Clone, Copy)]
struct Time {
    value: i64,
    scale: i32,
    flags: u32,
    epoch: i64,
}
#[repr(C)]
struct Timing {
    duration: Time,
    pts: Time,
    dts: Time,
}
#[repr(C)]
struct BlockSource {
    version: u32,
    allocate: Option<unsafe extern "C" fn(Ref, usize) -> Ref>,
    free: Option<unsafe extern "C" fn(Ref, Ref, usize)>,
    context: Ref,
}
#[repr(C)]
struct Callback {
    function: unsafe extern "C" fn(Ref, Ref, i32, u32, Ref, Time, Time),
    context: Ref,
}
#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFRelease(value: Ref);
    fn CFDictionaryCreate(
        allocator: Ref,
        keys: *const Ref,
        values: *const Ref,
        count: isize,
        key_callbacks: Ref,
        value_callbacks: Ref,
    ) -> Ref;
    fn CFNumberCreate(allocator: Ref, kind: isize, value: *const c_void) -> Ref;
    static kCFBooleanTrue: Ref;
}
#[link(name = "CoreVideo", kind = "framework")]
unsafe extern "C" {
    static kCVPixelBufferPixelFormatTypeKey: Ref;
    static kCVPixelBufferIOSurfacePropertiesKey: Ref;
    static kCVPixelBufferMetalCompatibilityKey: Ref;
}
#[link(name = "CoreMedia", kind = "framework")]
unsafe extern "C" {
    fn CMVideoFormatDescriptionCreateFromH264ParameterSets(
        allocator: Ref,
        count: usize,
        pointers: *const *const u8,
        sizes: *const usize,
        length_size: i32,
        output: *mut Ref,
    ) -> i32;
    fn CMBlockBufferCreateWithMemoryBlock(
        allocator: Ref,
        memory: Ref,
        length: usize,
        block_allocator: Ref,
        source: *const BlockSource,
        offset: usize,
        data_length: usize,
        flags: u32,
        output: *mut Ref,
    ) -> i32;
    fn CMSampleBufferCreateReady(
        allocator: Ref,
        data: Ref,
        format: Ref,
        count: isize,
        timing_count: isize,
        timing: *const Timing,
        size_count: isize,
        sizes: *const usize,
        output: *mut Ref,
    ) -> i32;
}
#[link(name = "VideoToolbox", kind = "framework")]
unsafe extern "C" {
    static kVTVideoDecoderSpecification_RequireHardwareAcceleratedVideoDecoder: Ref;
    fn VTDecompressionSessionCreate(
        allocator: Ref,
        format: Ref,
        specification: Ref,
        attributes: Ref,
        callback: *const Callback,
        output: *mut Ref,
    ) -> i32;
    fn VTDecompressionSessionDecodeFrame(
        session: Ref,
        sample: Ref,
        flags: u32,
        frame_context: Ref,
        info: *mut u32,
    ) -> i32;
    fn VTDecompressionSessionFinishDelayedFrames(session: Ref) -> i32;
    fn VTDecompressionSessionWaitForAsynchronousFrames(session: Ref) -> i32;
    fn VTDecompressionSessionInvalidate(session: Ref);
}
struct Owned(Ref);
impl Drop for Owned {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { CFRelease(self.0) }
        }
    }
}
fn checked(status: i32, operation: &str) -> Result<(), String> {
    if status == 0 {
        Ok(())
    } else {
        Err(format!("{operation}: OSStatus {status}"))
    }
}
unsafe fn dictionary(keys: &[Ref], values: &[Ref]) -> Result<Owned, String> {
    let value = unsafe {
        CFDictionaryCreate(
            ptr::null_mut(),
            keys.as_ptr(),
            values.as_ptr(),
            keys.len() as isize,
            ptr::null_mut(),
            ptr::null_mut(),
        )
    };
    if value.is_null() {
        Err("CFDictionaryCreate failed".into())
    } else {
        Ok(Owned(value))
    }
}
/// Implementors must keep the returned slice at the same address, readable and
/// unchanged, until the last Arc is dropped. Shared guest memory must be pinned
/// and protected against guest reuse for that entire lifetime.
///
/// # Safety
/// The immutability and address-stability guarantee applies across threads.
pub unsafe trait CompressedBytes: Send + Sync {
    fn bytes(&self) -> &[u8];
}
unsafe impl CompressedBytes for Vec<u8> {
    fn bytes(&self) -> &[u8] {
        self
    }
}
unsafe impl CompressedBytes for Box<[u8]> {
    fn bytes(&self) -> &[u8] {
        self
    }
}
struct Input {
    _owner: Arc<dyn CompressedBytes>,
}
unsafe extern "C" fn release_input(context: Ref, _: Ref, _: usize) {
    unsafe { drop(Box::from_raw(context.cast::<Input>())) }
}
struct Callbacks {
    pending: Mutex<HashMap<usize, mpsc::Sender<Result<Arc<Picture>, String>>>>,
}
unsafe extern "C" fn completed(
    context: Ref,
    frame: Ref,
    status: i32,
    flags: u32,
    image: Ref,
    _: Time,
    _: Time,
) {
    // No Rust panic may cross the C ABI, including a poisoned mutex.
    let state = unsafe { &*context.cast::<Callbacks>() };
    let sender = state
        .pending
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(&(frame as usize));
    if let Some(sender) = sender {
        let result = if status != 0 {
            Err(format!("VideoToolbox decode: OSStatus {status}"))
        } else if flags & 2 != 0 || image.is_null() {
            Err("VideoToolbox dropped the frame".into())
        } else {
            unsafe { Picture::from_completed_buffer(image) }.map_err(str::to_owned)
        };
        let _ = sender.send(result);
    }
}
/// Sessions serialize submissions through `&mut self`. The completion receiver
/// resolves only after the decoder has completed and retained the pixel buffer.
pub struct Decoder {
    session: Owned,
    _format: Owned,
    callbacks: Box<Callbacks>,
    next: usize,
    pub coded_extent: [u32; 2],
}
// VideoToolbox sessions may move between threads. All mutating calls require &mut.
unsafe impl Send for Decoder {}
impl Decoder {
    /// SPS and PPS include their NAL headers, without Annex B start codes.
    pub fn new(sps: &[u8], pps: &[u8]) -> Result<Self, String> {
        let extent = validate_sps(sps)?;
        validate_pps(pps)?;
        if !super::macos::hardware_h264() {
            return Err("hardware H.264 decode unavailable".into());
        }
        unsafe {
            let mut format = Owned(ptr::null_mut());
            checked(
                CMVideoFormatDescriptionCreateFromH264ParameterSets(
                    ptr::null_mut(),
                    2,
                    [sps.as_ptr(), pps.as_ptr()].as_ptr(),
                    [sps.len(), pps.len()].as_ptr(),
                    4,
                    &mut format.0,
                ),
                "H.264 format",
            )?;
            let specification = dictionary(
                &[kVTVideoDecoderSpecification_RequireHardwareAcceleratedVideoDecoder],
                &[kCFBooleanTrue],
            )?;
            let empty = dictionary(&[], &[])?;
            let nv12 = u32::from_be_bytes(*b"420v");
            let number = Owned(CFNumberCreate(
                ptr::null_mut(),
                3,
                (&nv12 as *const u32).cast(),
            ));
            if number.0.is_null() {
                return Err("CFNumberCreate failed".into());
            }
            let attributes = dictionary(
                &[
                    kCVPixelBufferPixelFormatTypeKey,
                    kCVPixelBufferIOSurfacePropertiesKey,
                    kCVPixelBufferMetalCompatibilityKey,
                ],
                &[number.0, empty.0, kCFBooleanTrue],
            )?;
            let mut callbacks = Box::new(Callbacks {
                pending: Mutex::new(HashMap::new()),
            });
            let callback = Callback {
                function: completed,
                context: (&mut *callbacks as *mut Callbacks).cast(),
            };
            let mut session = Owned(ptr::null_mut());
            checked(
                VTDecompressionSessionCreate(
                    ptr::null_mut(),
                    format.0,
                    specification.0,
                    attributes.0,
                    &callback,
                    &mut session.0,
                ),
                "hardware decoder creation",
            )?;
            Ok(Self {
                session,
                _format: format,
                callbacks,
                next: 1,
                coded_extent: extent,
            })
        }
    }
    /// Submit one complete AVC access unit, with four-byte big-endian NAL sizes.
    /// The caller converts Annex B framing separately. The payload is not copied.
    /// All dependent pictures must be submitted in decode order in this session.
    pub fn submit(
        &mut self,
        owner: Arc<dyn CompressedBytes>,
        range: Range<usize>,
    ) -> Result<mpsc::Receiver<Result<Arc<Picture>, String>>, String> {
        let bytes = owner
            .bytes()
            .get(range)
            .ok_or("compressed range outside allocation")?;
        validate_avc(bytes)?;
        let length = bytes.len();
        let memory = bytes.as_ptr() as Ref;
        let id = self.next;
        self.next = self
            .next
            .checked_add(1)
            .ok_or("decode sequence exhausted")?;
        unsafe {
            let input = Box::into_raw(Box::new(Input { _owner: owner }));
            let source = BlockSource {
                version: 0,
                allocate: None,
                free: Some(release_input),
                context: input.cast(),
            };
            let mut block = Owned(ptr::null_mut());
            let status = CMBlockBufferCreateWithMemoryBlock(
                ptr::null_mut(),
                memory,
                length,
                ptr::null_mut(),
                &source,
                0,
                length,
                0,
                &mut block.0,
            );
            if status != 0 {
                // CoreMedia does not take the memory block on creation failure.
                drop(Box::from_raw(input));
                checked(status, "compressed block")?;
            }
            let mut sample = Owned(ptr::null_mut());
            let timing = Timing {
                duration: Time {
                    value: 0,
                    scale: 0,
                    flags: 0,
                    epoch: 0,
                },
                pts: Time {
                    value: id as i64,
                    scale: 1,
                    flags: 1,
                    epoch: 0,
                },
                dts: Time {
                    value: 0,
                    scale: 0,
                    flags: 0,
                    epoch: 0,
                },
            };
            checked(
                CMSampleBufferCreateReady(
                    ptr::null_mut(),
                    block.0,
                    self._format.0,
                    1,
                    1,
                    &timing,
                    1,
                    &length,
                    &mut sample.0,
                ),
                "compressed sample",
            )?;
            let (tx, rx) = mpsc::channel();
            self.callbacks
                .pending
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(id, tx);
            let status = VTDecompressionSessionDecodeFrame(
                self.session.0,
                sample.0,
                1,
                id as Ref,
                ptr::null_mut(),
            );
            if status != 0 {
                self.callbacks
                    .pending
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .remove(&id);
                checked(status, "decode submission")?;
            }
            Ok(rx)
        }
    }
    /// Complete delayed output before resolving a Vulkan submission's fence.
    pub fn drain(&mut self) -> Result<(), String> {
        let finish = unsafe { VTDecompressionSessionFinishDelayedFrames(self.session.0) };
        let wait = unsafe { VTDecompressionSessionWaitForAsynchronousFrames(self.session.0) };
        checked(finish, "finish delayed frames")?;
        checked(wait, "wait for decoder")?;
        let pending = std::mem::take(
            &mut *self
                .callbacks
                .pending
                .lock()
                .unwrap_or_else(|e| e.into_inner()),
        );
        for (_, sender) in pending {
            let _ = sender.send(Err("decoder completed without picture output".into()));
        }
        Ok(())
    }
}
impl Drop for Decoder {
    fn drop(&mut self) {
        let _ = self.drain();
        unsafe {
            VTDecompressionSessionInvalidate(self.session.0);
        }
        // Invalidate synchronously cancels remaining callbacks. Context outlives it.
    }
}
fn validate_avc(mut data: &[u8]) -> Result<(), String> {
    let mut vcl = false;
    while !data.is_empty() {
        let size = u32::from_be_bytes(
            data.get(..4)
                .ok_or("truncated AVC length")?
                .try_into()
                .unwrap(),
        ) as usize;
        data = &data[4..];
        let nal = data
            .get(..size)
            .filter(|n| !n.is_empty())
            .ok_or("invalid AVC NAL length")?;
        if nal[0] & 0x80 != 0 {
            return Err("forbidden H.264 NAL bit".into());
        }
        match nal[0] & 31 {
            1 | 5 => vcl = true,
            6 | 9 | 12 => {}
            _ => {
                return Err("access unit contains parameter updates or unsupported NAL type".into());
            }
        }
        data = &data[size..];
    }
    if !vcl {
        return Err("access unit has no coded slice".into());
    }
    Ok(())
}
struct Bits {
    data: Vec<u8>,
    bit: usize,
}
impl Bits {
    fn read(&mut self, count: usize) -> Result<u32, String> {
        if count > 32
            || self
                .bit
                .checked_add(count)
                .is_none_or(|end| end > self.data.len() * 8)
        {
            return Err("truncated SPS".into());
        }
        let mut value = 0;
        for _ in 0..count {
            value = (value << 1) | u32::from((self.data[self.bit / 8] >> (7 - self.bit % 8)) & 1);
            self.bit += 1;
        }
        Ok(value)
    }
    fn ue(&mut self) -> Result<u32, String> {
        let mut zeros = 0;
        while self.read(1)? == 0 {
            zeros += 1;
            if zeros > 30 {
                return Err("invalid SPS Exp-Golomb".into());
            }
        }
        Ok((1 << zeros) - 1 + self.read(zeros)?)
    }
    fn se(&mut self) -> Result<i32, String> {
        let value = self.ue()?;
        Ok(if value & 1 == 1 {
            (value / 2 + 1) as i32
        } else {
            -(value as i32 / 2)
        })
    }
}
/// Check the actual SPS rather than trusting the Vulkan profile declaration.
fn validate_sps(sps: &[u8]) -> Result<[u32; 2], String> {
    if sps.first().map(|x| x & 0x9f) != Some(7) {
        return Err("expected an SPS NAL".into());
    }
    let mut data = Vec::new();
    let mut zeros = 0;
    for &byte in &sps[1..] {
        if zeros == 2 && byte == 3 {
            zeros = 0;
            continue;
        }
        data.push(byte);
        zeros = if byte == 0 { zeros + 1 } else { 0 };
    }
    let mut b = Bits { data, bit: 0 };
    let profile = b.read(8)?;
    b.read(8)?;
    b.read(8)?;
    if !matches!(profile, 66 | 77 | 100) {
        return Err("only Baseline, Main and High H.264 profiles are supported".into());
    }
    if b.ue()? > 31 {
        return Err("SPS id outside H.264 range".into());
    }
    if profile == 100 {
        if b.ue()? != 1 || b.ue()? != 0 || b.ue()? != 0 {
            return Err("decoder requires 8-bit 4:2:0".into());
        }
        b.read(1)?;
        if b.read(1)? != 0 {
            // Scaling matrices have variable length; parse all lists.
            for i in 0..8 {
                if b.read(1)? != 0 {
                    let mut last = 8;
                    let mut next = 8;
                    for _ in 0..if i < 6 { 16 } else { 64 } {
                        if next != 0 {
                            next = (last + b.se()? + 256).rem_euclid(256);
                        }
                        if next != 0 {
                            last = next;
                        }
                    }
                }
            }
        }
    }
    if b.ue()? > 12 {
        return Err("invalid SPS frame number width".into());
    }
    match b.ue()? {
        0 => {
            if b.ue()? > 12 {
                return Err("invalid SPS POC width".into());
            }
        }
        1 => {
            b.read(1)?;
            b.se()?;
            b.se()?;
            let count = b.ue()?;
            if count > 255 {
                return Err("invalid SPS POC cycle".into());
            }
            for _ in 0..count {
                b.se()?;
            }
        }
        2 => {}
        _ => return Err("invalid SPS POC type".into()),
    }
    if b.ue()? > 16 {
        return Err("too many H.264 reference frames".into());
    }
    b.read(1)?;
    let width = b
        .ue()?
        .checked_add(1)
        .and_then(|n| n.checked_mul(16))
        .ok_or("SPS width overflow")?;
    let height = b
        .ue()?
        .checked_add(1)
        .and_then(|n| n.checked_mul(16))
        .ok_or("SPS height overflow")?;
    if b.read(1)? != 1 {
        return Err("interlaced H.264 is unsupported".into());
    }
    b.read(1)?;
    if b.read(1)? != 0 {
        // Reject cropping until Vulkan coded/display extents are represented separately.
        let mut crop = 0u64;
        for _ in 0..4 {
            crop += u64::from(b.ue()?);
        }
        if crop != 0 {
            return Err("cropped H.264 SPS is unsupported".into());
        }
    }
    if width > 8192 || height > 8192 {
        return Err("H.264 coded extent exceeds decoder limit".into());
    }
    Ok([width, height])
}

#[cfg(test)]
mod tests {
    use super::*;
    fn baseline(progressive: bool) -> Vec<u8> {
        // SPS id=0, frame-num width=4, POC type=2, one reference,
        // gaps forbidden, 2x2 macroblocks, supplied frame_mbs_only_flag.
        let bits = format!("110110100010010{}10", u8::from(progressive));
        let mut bytes = vec![0x67, 66, 0, 30];
        let padded = format!("{bits}{}", "0".repeat((8 - bits.len() % 8) % 8));
        for chunk in padded.as_bytes().chunks(8) {
            bytes.push(chunk.iter().fold(0, |n, b| n * 2 + (b - b'0')));
        }
        bytes
    }
    #[test]
    fn accepts_progressive_baseline_and_rejects_interlace() {
        assert_eq!(validate_sps(&baseline(true)).unwrap(), [32, 32]);
        assert!(
            validate_sps(&baseline(false))
                .unwrap_err()
                .contains("interlaced")
        );
    }
    #[test]
    fn rejects_truncated_sps_and_unsupported_profiles() {
        for length in 0..4 {
            assert!(validate_sps(&baseline(true)[..length]).is_err());
        }
        let mut sps = baseline(true);
        sps[1] = 110;
        assert!(validate_sps(&sps).unwrap_err().contains("profiles"));
    }
    #[test]
    fn validates_avc_framing_and_blocks_in_band_parameter_changes() {
        assert!(validate_avc(&[0, 0, 0, 2, 0x65, 0x88]).is_ok());
        assert!(validate_avc(&[0, 0, 0, 2, 0x65]).is_err());
        assert!(validate_avc(&[0, 0, 0, 1, 0x67]).is_err());
        assert!(validate_avc(&[0, 0, 0, 1, 0x09]).is_err());
        assert!(validate_avc(&[0, 0, 0, 1, 0xe5]).is_err());
    }
}

fn validate_pps(pps: &[u8]) -> Result<(), String> {
    if pps.first().map(|x| x & 0x9f) != Some(8) {
        return Err("expected a PPS NAL".into());
    }
    let mut data = Vec::new();
    let mut zeros = 0;
    for &byte in &pps[1..] {
        if zeros == 2 && byte == 3 {
            zeros = 0;
            continue;
        }
        data.push(byte);
        zeros = if byte == 0 { zeros + 1 } else { 0 };
    }
    let mut bits = Bits { data, bit: 0 };
    if bits.ue()? > 255 || bits.ue()? > 31 {
        return Err("PPS parameter set id outside H.264 range".into());
    }
    bits.read(1)?;
    bits.read(1)?;
    if bits.ue()? != 0 {
        return Err("H.264 flexible macroblock ordering is unsupported".into());
    }
    Ok(())
}
