//! Direct original PCM/skip hardware boundary proof. This is not a wire proof.
//! A failed accepted case parks one bounded graph/process. No watchdog or exit
//! may release an unproved callback, compressed-input owner, or GPU job.
#[path = "../../src/native_video_fixture_stream.rs"]
mod stream;
use ash::vk;
use std::{
    ffi::{CStr, c_void},
    fs::{File, OpenOptions},
    io::Write,
    panic::{AssertUnwindSafe, catch_unwind},
    ptr,
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use stream::{PcmCase, Stream};
use sv_gpu_server::{
    driver::Driver,
    video_nv12::{
        joined,
        macos::{Picture, Range as PixelRange},
        videotoolbox::{CompressedBytes, Decoder, Submission},
    },
};
#[link(name = "CoreVideo", kind = "framework")]
unsafe extern "C" {
    fn CVPixelBufferLockBaseAddress(buffer: *mut c_void, flags: u64) -> i32;
    fn CVPixelBufferUnlockBaseAddress(buffer: *mut c_void, flags: u64) -> i32;
    fn CVPixelBufferGetBaseAddressOfPlane(buffer: *mut c_void, plane: usize) -> *mut c_void;
    fn CVPixelBufferGetIOSurface(buffer: *mut c_void) -> *mut c_void;
}
#[link(name = "Metal", kind = "framework")]
unsafe extern "C" {
    fn MTLCreateSystemDefaultDevice() -> *mut c_void;
}
#[link(name = "objc")]
unsafe extern "C" {
    fn sel_registerName(name: *const std::ffi::c_char) -> *mut c_void;
    fn objc_getClass(name: *const std::ffi::c_char) -> *mut c_void;
    fn objc_msgSend();
    fn objc_autoreleasePoolPush() -> *mut c_void;
    fn objc_autoreleasePoolPop(pool: *mut c_void);
}
unsafe fn object(object: *mut c_void, selector: &CStr) -> *mut c_void {
    let send: unsafe extern "C" fn(*mut c_void, *mut c_void) -> *mut c_void =
        unsafe { std::mem::transmute(objc_msgSend as *const ()) };
    unsafe { send(object, sel_registerName(selector.as_ptr())) }
}
unsafe fn call(object: *mut c_void, selector: &CStr) {
    let send: unsafe extern "C" fn(*mut c_void, *mut c_void) =
        unsafe { std::mem::transmute(objc_msgSend as *const ()) };
    unsafe { send(object, sel_registerName(selector.as_ptr())) }
}
struct Owned(*mut c_void);
impl Owned {
    fn new(pointer: *mut c_void, role: &str) -> Result<Self, String> {
        if pointer.is_null() {
            Err(format!("null Metal {role}"))
        } else {
            Ok(Self(pointer))
        }
    }
    unsafe fn retain(pointer: *mut c_void, role: &str) -> Result<Self, String> {
        if pointer.is_null() {
            return Err(format!("null Metal {role}"));
        }
        Self::new(unsafe { object(pointer, c"retain") }, role)
    }
}
impl Drop for Owned {
    fn drop(&mut self) {
        unsafe { call(self.0, c"release") };
    }
}
struct Pool(*mut c_void);
impl Drop for Pool {
    fn drop(&mut self) {
        unsafe { objc_autoreleasePoolPop(self.0) };
    }
}
struct Mapping {
    pointer: *mut u8,
    length: usize,
    _file: File,
}
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
            std::env::temp_dir().join(format!("sv-vt-pcm-source-{}-{nonce}", std::process::id()));
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&path)
            .map_err(|e| e.to_string())?;
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
    fn clear(&mut self) {
        unsafe { ptr::write_bytes(self.pointer, 0, self.length) };
    }
}
impl Drop for Mapping {
    fn drop(&mut self) {
        unsafe { libc::munmap(self.pointer.cast(), self.length) };
    }
}
#[repr(C)]
struct Info {
    get_device_proc_addr: vk::PFN_vkGetDeviceProcAddr,
    device: vk::Device,
    queue: vk::Queue,
    queue_family: u32,
    src_queue_family: u32,
    dst_queue_family: u32,
    memory_properties: *const vk::PhysicalDeviceMemoryProperties,
    image: vk::Image,
    old_layout: vk::ImageLayout,
    src_stage: vk::PipelineStageFlags,
    src_access: vk::AccessFlags,
    model: vk::SamplerYcbcrModelConversion,
    range: vk::SamplerYcbcrRange,
    x_chroma: vk::ChromaLocation,
    y_chroma: vk::ChromaLocation,
}
#[link(name = "kk_nv12_fixture", kind = "static")]
unsafe extern "C" {
    fn kk_nv12_fixture_start(info: *const Info, job: *mut *mut c_void) -> vk::Result;
    fn kk_nv12_fixture_transition_start(
        info: *const Info,
        layout: vk::ImageLayout,
        stage: vk::PipelineStageFlags,
        access: vk::AccessFlags,
        job: *mut *mut c_void,
    ) -> vk::Result;
    fn kk_nv12_fixture_wait(job: *mut c_void, timeout: u64) -> vk::Result;
    fn kk_nv12_fixture_read(job: *mut c_void, timeout: u64, rgba: *mut f32) -> vk::Result;
    fn kk_nv12_fixture_destroy(job: *mut c_void) -> vk::Result;
}
fn checked(result: vk::Result, stage: &str) -> Result<(), String> {
    if result == vk::Result::SUCCESS {
        Ok(())
    } else {
        Err(format!("{stage}: {result:?}"))
    }
}
/// The complete actual Metal body is installed in CaseGraph before commit.
/// No pending command/texture/queue owner is hidden in a fallible stack helper.
struct MetalJob {
    command: Owned,
    _encoder: Owned,
    _queue: Owned,
    _textures: Vec<Owned>,
    _device: Owned,
    _source: Arc<Picture>,
    _target: Arc<Picture>,
}
impl MetalJob {
    fn prepare(source: Arc<Picture>, target: Arc<Picture>) -> Result<Self, String> {
        let device = Owned::new(unsafe { MTLCreateSystemDefaultDevice() }, "device")?;
        let mut textures = Vec::with_capacity(4);
        for picture in [&source, &target] {
            let layout = picture.layout();
            let surface = unsafe { CVPixelBufferGetIOSurface(picture.pixel_buffer()) };
            if surface.is_null() {
                return Err("Metal copy picture has no IOSurface".into());
            }
            for plane in 0..2 {
                let p = layout.planes[plane];
                let make: unsafe extern "C" fn(
                    *mut c_void,
                    *mut c_void,
                    u64,
                    u64,
                    u64,
                    bool,
                ) -> *mut c_void = unsafe { std::mem::transmute(objc_msgSend as *const ()) };
                let descriptor = unsafe {
                    make(
                        objc_getClass(c"MTLTextureDescriptor".as_ptr()),
                        sel_registerName(
                            c"texture2DDescriptorWithPixelFormat:width:height:mipmapped:".as_ptr(),
                        ),
                        if plane == 0 { 10 } else { 30 },
                        p.width as u64,
                        p.height as u64,
                        false,
                    )
                };
                if descriptor.is_null() {
                    return Err("Metal copy descriptor missing".into());
                }
                let set: unsafe extern "C" fn(*mut c_void, *mut c_void, u64) =
                    unsafe { std::mem::transmute(objc_msgSend as *const ()) };
                unsafe {
                    set(descriptor, sel_registerName(c"setUsage:".as_ptr()), 0);
                    set(descriptor, sel_registerName(c"setStorageMode:".as_ptr()), 0);
                }
                let make: unsafe extern "C" fn(
                    *mut c_void,
                    *mut c_void,
                    *mut c_void,
                    *mut c_void,
                    u64,
                ) -> *mut c_void = unsafe { std::mem::transmute(objc_msgSend as *const ()) };
                textures.push(Owned::new(
                    unsafe {
                        make(
                            device.0,
                            sel_registerName(c"newTextureWithDescriptor:iosurface:plane:".as_ptr()),
                            descriptor,
                            surface,
                            plane as u64,
                        )
                    },
                    "copy texture",
                )?);
            }
        }
        let queue = Owned::new(
            unsafe { object(device.0, c"newCommandQueue") },
            "copy queue",
        )?;
        let command = unsafe { Owned::retain(object(queue.0, c"commandBuffer"), "copy command") }?;
        let encoder =
            unsafe { Owned::retain(object(command.0, c"blitCommandEncoder"), "copy encoder") }?;
        #[repr(C)]
        #[derive(Clone, Copy)]
        struct Three {
            x: usize,
            y: usize,
            z: usize,
        }
        let copy: unsafe extern "C" fn(
            *mut c_void,
            *mut c_void,
            *mut c_void,
            usize,
            usize,
            Three,
            Three,
            *mut c_void,
            usize,
            usize,
            Three,
        ) = unsafe { std::mem::transmute(objc_msgSend as *const ()) };
        for plane in 0..2 {
            let p = target.layout().planes[plane];
            unsafe {
                copy(encoder.0,sel_registerName(c"copyFromTexture:sourceSlice:sourceLevel:sourceOrigin:sourceSize:toTexture:destinationSlice:destinationLevel:destinationOrigin:".as_ptr()),textures[plane].0,0,0,Three{x:0,y:0,z:0},Three{x:p.width,y:p.height,z:1},textures[plane+2].0,0,0,Three{x:0,y:0,z:0});
            }
        }
        unsafe { call(encoder.0, c"endEncoding") };
        Ok(Self {
            command,
            _encoder: encoder,
            _queue: queue,
            _textures: textures,
            _device: device,
            _source: source,
            _target: target,
        })
    }
    fn commit(&self) {
        unsafe { call(self.command.0, c"commit") };
    }
    fn wait(&self) -> Result<(), String> {
        unsafe { call(self.command.0, c"waitUntilCompleted") };
        let status: unsafe extern "C" fn(*mut c_void, *mut c_void) -> usize =
            unsafe { std::mem::transmute(objc_msgSend as *const ()) };
        let status = unsafe { status(self.command.0, sel_registerName(c"status".as_ptr())) };
        if status == 4 {
            Ok(())
        } else {
            Err(format!(
                "Metal command has no successful completion proof: status={status}"
            ))
        }
    }
}
struct Locked<'a>(&'a Picture);
impl Drop for Locked<'_> {
    fn drop(&mut self) {
        unsafe { CVPixelBufferUnlockBaseAddress(self.0.pixel_buffer(), 1) };
    }
}
fn verify(picture: &Picture, y: u8, role: &str) -> Result<(), String> {
    let layout = picture.layout();
    if layout.width != 64 || layout.height != 64 || picture.range() != PixelRange::Video {
        return Err(format!("{role}: exact64x64 video-range NV12 required"));
    }
    let status = unsafe { CVPixelBufferLockBaseAddress(picture.pixel_buffer(), 1) };
    if status != 0 {
        return Err(format!("{role}: pixel lock failed {status}"));
    }
    let _locked = Locked(picture);
    let luma =
        unsafe { CVPixelBufferGetBaseAddressOfPlane(picture.pixel_buffer(), 0) }.cast::<u8>();
    let chroma =
        unsafe { CVPixelBufferGetBaseAddressOfPlane(picture.pixel_buffer(), 1) }.cast::<u8>();
    if luma.is_null() || chroma.is_null() {
        return Err(format!("{role}: missing diagnostic planes"));
    }
    for row in 0..64 {
        for column in 0..64 {
            if unsafe { *luma.add(row * layout.planes[0].stride + column) } != y {
                return Err(format!(
                    "{role}: luma differs at {column},{row}; expected={y}"
                ));
            }
        }
    }
    for row in 0..32 {
        for column in 0..32 {
            let pixel = unsafe { chroma.add(row * layout.planes[1].stride + column * 2) };
            if unsafe { *pixel } != 80 || unsafe { *pixel.add(1) } != 180 {
                return Err(format!("{role}: chroma differs at {column},{row}"));
            }
        }
    }
    Ok(())
}
struct CaseGraph {
    label: String,
    stream: Stream,
    frame: usize,
    phase: &'static str,
    source: Arc<Mapping>,
    decoder: Option<Decoder>,
    submission: Option<Submission>,
    current: Option<Arc<Picture>>,
    pictures: Vec<Arc<Picture>>,
    joined: joined::Image,
    driver: Arc<Driver>,
    target: Arc<Picture>,
    gpu_job: *mut c_void,
    metal_job: Option<MetalJob>,
    vt_pending: bool,
    sampled: bool,
    accepted: bool,
    retired: bool,
    requirement: u64,
    _pool: Pool,
}
impl CaseGraph {
    fn new(label: String, case: PcmCase) -> Result<Box<Self>, String> {
        let stream = stream::pcm_original(case)?;
        let pointer = unsafe { objc_autoreleasePoolPush() };
        if pointer.is_null() {
            return Err("Objective-C autorelease pool unavailable".into());
        }
        let pool = Pool(pointer);
        let source = Arc::new(Mapping::new(stream.bytes)?);
        // The application target and original joined VkImage predate decoding.
        let target = Picture::allocate_target(64, 64, PixelRange::Video)?;
        let driver = Arc::new(Driver::open(0).map_err(|e| e.to_string())?);
        let joined = unsafe { joined::Image::import(Arc::clone(&driver), Arc::clone(&target)) }?;
        let requirement =
            unsafe { driver.device.get_image_memory_requirements(joined.image()) }.size;
        let decoder = Decoder::new(
            &source.bytes()[stream.sps.clone()],
            &source.bytes()[stream.pps.clone()],
        )?;
        if decoder.coded_extent != [64, 64]
            || decoder.display_extent != [64, 64]
            || decoder.crop_origin != [0, 0]
            || decoder.output_range != PixelRange::Video
        {
            return Err("PCM original SPS geometry/range mismatch".into());
        }
        let graph = Box::new(Self {
            label,
            stream,
            frame: 0,
            phase: "prepared",
            source,
            decoder: Some(decoder),
            submission: None,
            current: None,
            pictures: Vec::with_capacity(7),
            joined,
            driver,
            target,
            gpu_job: ptr::null_mut(),
            metal_job: None,
            vt_pending: false,
            sampled: false,
            accepted: false,
            retired: false,
            requirement,
            _pool: pool,
        });
        graph.report();
        Ok(graph)
    }
    fn report(&self) {
        let pid = std::process::id();
        let rss = std::process::Command::new("/bin/ps")
            .args(["-o", "rss=", "-p", &pid.to_string()])
            .output()
            .ok()
            .filter(|o| o.status.success())
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .and_then(|s| s.trim().parse::<u64>().ok());
        eprintln!(
            "pcm resources case={} frame={} phase={} pid={pid} rss_kib={rss:?} original_source_bytes={} source_arc_count={} target_images=1 target_extent=64x64 target_allocation={} joined_image={:?} image_requirement={} decoder_sessions={} retained_pictures={} vt_pending={} gpu_jobs={} gpu_job={:?} metal_jobs={} metal_command={:?} accepted={} retired={} fixture_threads=1 guest_maps=0 watchdog=false",
            self.label,
            self.frame,
            self.phase,
            self.source.length,
            Arc::strong_count(&self.source),
            self.target.layout().allocation_size,
            self.joined.image(),
            self.requirement,
            usize::from(self.decoder.is_some()),
            self.pictures.len() + usize::from(self.current.is_some()),
            self.vt_pending,
            usize::from(!self.gpu_job.is_null()),
            self.gpu_job,
            usize::from(self.metal_job.is_some()),
            self.metal_job.as_ref().map(|job| job.command.0),
            self.accepted,
            self.retired
        );
    }
    fn phase(&mut self, phase: &'static str) {
        self.phase = phase;
        self.report();
    }
    fn info(&self) -> Info {
        Info {
            get_device_proc_addr: self.driver.instance.fp_v1_0().get_device_proc_addr,
            device: self.driver.device.handle(),
            queue: self.driver.queue,
            queue_family: self.driver.queue_family,
            src_queue_family: self.driver.queue_family,
            dst_queue_family: vk::QUEUE_FAMILY_EXTERNAL,
            memory_properties: &self.driver.mem_props,
            image: self.joined.image(),
            old_layout: if self.sampled {
                vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL
            } else {
                vk::ImageLayout::PREINITIALIZED
            },
            src_stage: if self.sampled {
                vk::PipelineStageFlags::COMPUTE_SHADER
            } else {
                vk::PipelineStageFlags::TOP_OF_PIPE
            },
            src_access: if self.sampled {
                vk::AccessFlags::SHADER_READ
            } else {
                vk::AccessFlags::empty()
            },
            model: vk::SamplerYcbcrModelConversion::YCBCR_601,
            range: vk::SamplerYcbcrRange::ITU_NARROW,
            x_chroma: vk::ChromaLocation::MIDPOINT,
            y_chroma: vk::ChromaLocation::MIDPOINT,
        }
    }
    fn retire_gpu_job(&mut self) -> Result<(), String> {
        self.phase("native-fence");
        checked(
            unsafe { kk_nv12_fixture_wait(self.gpu_job, 2_000_000_000) },
            "native fence",
        )?;
        checked(
            unsafe { kk_nv12_fixture_destroy(self.gpu_job) },
            "native job destroy",
        )?;
        self.gpu_job = ptr::null_mut();
        Ok(())
    }
    fn sample(&mut self, y: u8, external: bool) -> Result<(), String> {
        let mut info = self.info();
        info.src_queue_family = if external {
            vk::QUEUE_FAMILY_EXTERNAL
        } else {
            vk::QUEUE_FAMILY_IGNORED
        };
        info.dst_queue_family = if external {
            self.driver.queue_family
        } else {
            vk::QUEUE_FAMILY_IGNORED
        };
        if external {
            info.old_layout = vk::ImageLayout::GENERAL;
            info.src_stage = vk::PipelineStageFlags::ALL_COMMANDS;
            info.src_access = vk::AccessFlags::MEMORY_WRITE;
        }
        self.phase("sample-submit");
        checked(
            unsafe { kk_nv12_fixture_start(&info, &raw mut self.gpu_job) },
            "actual joined sample submit",
        )?;
        self.phase("sample-fence-read");
        let mut rgba = [0.0f32; 64];
        checked(
            unsafe { kk_nv12_fixture_read(self.gpu_job, 2_000_000_000, rgba.as_mut_ptr()) },
            "actual joined sample fence/read",
        )?;
        checked(
            unsafe { kk_nv12_fixture_destroy(self.gpu_job) },
            "sample job destroy",
        )?;
        self.gpu_job = ptr::null_mut();
        self.sampled = true;
        let luma = (f32::from(y) - 16.0) / 219.0;
        let cb = (80.0 - 128.0) / 224.0;
        let cr = (180.0 - 128.0) / 224.0;
        let expected = [
            luma + 1.402 * cr,
            luma - 0.344136 * cb - 0.714136 * cr,
            luma + 1.772 * cb,
            1.0,
        ];
        let mut error = 0.0f32;
        for pixel in rgba.chunks_exact(4) {
            for channel in 0..4 {
                if !pixel[channel].is_finite() {
                    return Err("nonfinite actual joined RGB".into());
                }
                error = error.max((pixel[channel] - expected[channel]).abs());
            }
        }
        if error > 0.0005 {
            return Err(format!(
                "actual joined RGB error {error} exceeds0.0005 for Y{y}"
            ));
        }
        println!(
            "pcm case={} frame={} actual_joined_image={:?} uniform_yuv=[{y},80,180] sampled_pixels=16 max_rgb_error={error}",
            self.label,
            self.frame,
            self.joined.image()
        );
        Ok(())
    }
    fn execute(&mut self) -> Result<(), String> {
        for index in 0..self.stream.frames.len() {
            self.frame = index;
            let frame = &self.stream.frames[index];
            let range = frame.range.clone();
            let offsets = frame.slice_offsets.clone();
            let y = frame.expected_y.ok_or("PCM fixture has no pixel oracle")?;
            eprintln!(
                "pcm source case={} frame={index} original_range={range:?} source_frame_num={} poc={:?} reference={} intra={} idr={} prior={:?}",
                self.label,
                frame.frame_num,
                frame.poc,
                frame.is_reference,
                frame.is_intra,
                frame.idr,
                frame.references
            );
            self.accepted = true;
            self.vt_pending = true;
            self.phase("submit-original");
            self.submission = Some(
                self.decoder
                    .as_mut()
                    .ok_or("missing decoder")?
                    .submit_annex_b_tracked(self.source.clone(), range, &offsets)?,
            );
            self.phase("decoder-drain");
            self.decoder.as_mut().ok_or("missing decoder")?.drain()?;
            self.phase("callback");
            self.current = Some(
                self.submission
                    .as_ref()
                    .ok_or("missing tracked submission")?
                    .picture
                    .recv_timeout(Duration::from_secs(2))
                    .map_err(|e| format!("callback proof: {e}"))??,
            );
            self.phase("original-input-release");
            self.submission
                .as_ref()
                .ok_or("missing tracked submission")?
                .input_released
                .recv_timeout(Duration::from_secs(2))
                .map_err(|e| format!("original input release proof: {e}"))?;
            if Arc::strong_count(&self.source) != 1 {
                return Err("original source owner remains after release proof".into());
            }
            self.vt_pending = false;
            verify(
                self.current.as_ref().ok_or("missing decoded output")?,
                y,
                "decoded",
            )?;
            self.phase("release-to-external");
            let info = self.info();
            checked(
                unsafe {
                    kk_nv12_fixture_transition_start(
                        &info,
                        vk::ImageLayout::GENERAL,
                        vk::PipelineStageFlags::BOTTOM_OF_PIPE,
                        vk::AccessFlags::empty(),
                        &raw mut self.gpu_job,
                    )
                },
                "external release submit",
            )?;
            self.retire_gpu_job()?;
            self.metal_job = Some(MetalJob::prepare(
                Arc::clone(self.current.as_ref().ok_or("missing decoded output")?),
                Arc::clone(&self.target),
            )?);
            self.phase("metal-commit");
            self.metal_job
                .as_ref()
                .ok_or("missing Metal body")?
                .commit();
            self.phase("metal-completion");
            self.metal_job
                .as_ref()
                .ok_or("missing Metal body")?
                .wait()?;
            self.metal_job.take();
            self.sample(y, true)?;
            verify(&self.target, y, "original preallocated target")?;
            self.pictures
                .push(self.current.take().ok_or("missing completed output")?);
            self.submission.take();
            self.phase("frame-complete");
        }
        self.phase("final-decoder-drain");
        self.decoder.as_mut().ok_or("missing decoder")?.drain()?;
        self.phase("decoder-invalidate");
        self.decoder.take();
        if Arc::strong_count(&self.source) != 1 {
            return Err("original source retained after invalidation".into());
        }
        self.phase("overwrite-released-original");
        Arc::get_mut(&mut self.source)
            .ok_or("original source is not unique")?
            .clear();
        if self.source.bytes().iter().any(|byte| *byte != 0) {
            return Err("original source overwrite did not reach mmap".into());
        }
        for (index, picture) in self.pictures.iter().enumerate() {
            verify(
                picture,
                self.stream.frames[index]
                    .expected_y
                    .ok_or("retained pixel oracle missing")?,
                "retained after original overwrite",
            )?;
        }
        let last_y = self
            .stream
            .frames
            .last()
            .and_then(|frame| frame.expected_y)
            .ok_or("last target oracle missing")?;
        verify(&self.target, last_y, "retained original target")?;
        self.phase("sample-retained-target");
        self.sample(last_y, false)?;
        if self.vt_pending || !self.gpu_job.is_null() || self.metal_job.is_some() {
            return Err("case ended without actual retirement proof".into());
        }
        self.retired = true;
        self.phase("retired");
        println!(
            "PASS pcm case={} frames={} required_hardware=true original_vcl_unchanged=true original_input_released=true preallocated_joined_target=true exact_nv12=true retained_after_source_overwrite=true no_runtime_gate_change=true",
            self.label,
            self.stream.frames.len()
        );
        Ok(())
    }
}
impl Drop for CaseGraph {
    fn drop(&mut self) {
        if self.accepted && !self.retired {
            self.phase = "drop-blocked-unproven";
            self.report();
            eprintln!(
                "FAIL PCM graph drop blocked; retaining original owners in place; process exit is not safe retirement"
            );
            loop {
                std::thread::park();
            }
        }
    }
}
fn parked(mut graph: Box<CaseGraph>, message: &str) -> ! {
    graph.phase = "retained-unproven";
    graph.report();
    eprintln!(
        "FAIL pcm case={} reason={message}; one bounded original graph retained; no watchdog, exit, teardown, retry, or additional allocation; controller must preserve pid={}",
        graph.label,
        std::process::id()
    );
    let _ = Box::leak(graph);
    loop {
        std::thread::park();
    }
}
pub fn run() -> Result<(), String> {
    unsafe { libc::alarm(0) };
    let arguments: Vec<_> = std::env::args().skip(1).collect();
    let all = [
        ("marking0", PcmCase::Marking(0)),
        ("marking1", PcmCase::Marking(1)),
        ("marking2", PcmCase::Marking(2)),
        ("gap0", PcmCase::Gap(0)),
        ("gap1", PcmCase::Gap(1)),
        ("gap2", PcmCase::Gap(2)),
        ("bootstrap0", PcmCase::Bootstrap(0)),
        ("bootstrap1", PcmCase::Bootstrap(1)),
        ("bootstrap2", PcmCase::Bootstrap(2)),
        ("b", PcmCase::BSkip),
    ];
    let cases = if arguments == ["--pcm-all"] {
        all.to_vec()
    } else if arguments.len() == 2 && arguments[0] == "--pcm" {
        vec![
            all.iter()
                .copied()
                .find(|(name, _)| *name == arguments[1])
                .ok_or("unknown PCM case")?,
        ]
    } else {
        return Err("usage: --pcm-all | --pcm marking0|marking1|marking2|gap0|gap1|gap2|bootstrap0|bootstrap1|bootstrap2|b".into());
    };
    for (label, case) in cases {
        let mut graph = CaseGraph::new(label.into(), case)?;
        match catch_unwind(AssertUnwindSafe(|| graph.execute())) {
            Ok(Ok(())) => {}
            Ok(Err(error)) => {
                if graph.accepted {
                    parked(graph, &error)
                } else {
                    return Err(error);
                }
            }
            Err(_) => {
                if graph.accepted {
                    parked(graph, "panic after possible native acceptance")
                } else {
                    return Err("panic before native acceptance".into());
                }
            }
        }
    }
    println!(
        "PASS direct original PCM hardware boundary matrix; capability/readiness unchanged; not a wire or stock-browser proof"
    );
    Ok(())
}
