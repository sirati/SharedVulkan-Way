//! Test-only static C sampler. Its only readback is 16 diagnostic RGB samples.
use ash::vk;
use std::{ffi::c_void, sync::Arc};
use sv_gpu_server::{
    driver::Driver,
    video_nv12::{joined, macos::Picture},
};
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
fn checked(result: vk::Result, stage: &str) {
    if result != vk::Result::SUCCESS {
        // Native work can still retain the caller's target on timeout/error.
        // Exit this isolated fixture without unwinding any live GPU owner.
        eprintln!("FAIL joined sampler stage={stage}: {result:?}; terminating isolated fixture");
        std::process::exit(1);
    }
}
pub struct Probe {
    image: joined::Image,
    driver: Arc<Driver>,
    sampled: bool,
}
impl Probe {
    pub fn new(target: Arc<Picture>) -> Result<Self, String> {
        let driver = Arc::new(Driver::open(0).map_err(|e| e.to_string())?);
        let image = unsafe { joined::Image::import(Arc::clone(&driver), target) }?;
        eprintln!(
            "fixture stage=joined target imported before decode image={:?}",
            image.image()
        );
        Ok(Self {
            image,
            driver,
            sampled: false,
        })
    }
    /// Caller retires all prior image readers before this bounded fixture call.
    pub unsafe fn copy_sample(
        &mut self,
        source: Arc<Picture>,
        frame: usize,
        full: bool,
    ) -> Result<(), String> {
        let mut info = Info {
            get_device_proc_addr: self.driver.instance.fp_v1_0().get_device_proc_addr,
            device: self.driver.device.handle(),
            queue: self.driver.queue,
            queue_family: self.driver.queue_family,
            src_queue_family: self.driver.queue_family,
            dst_queue_family: vk::QUEUE_FAMILY_EXTERNAL,
            memory_properties: &self.driver.mem_props,
            image: self.image.image(),
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
            range: if full {
                vk::SamplerYcbcrRange::ITU_FULL
            } else {
                vk::SamplerYcbcrRange::ITU_NARROW
            },
            x_chroma: vk::ChromaLocation::MIDPOINT,
            y_chroma: vk::ChromaLocation::MIDPOINT,
        };
        let mut release = std::ptr::null_mut();
        checked(
            unsafe {
                kk_nv12_fixture_transition_start(
                    &info,
                    vk::ImageLayout::GENERAL,
                    vk::PipelineStageFlags::BOTTOM_OF_PIPE,
                    vk::AccessFlags::empty(),
                    &mut release,
                )
            },
            "release-submit",
        );
        checked(
            unsafe { kk_nv12_fixture_wait(release, 2_000_000_000) },
            "release-fence",
        );
        checked(
            unsafe { kk_nv12_fixture_destroy(release) },
            "release-destroy",
        );
        unsafe { self.image.picture().copy_from_gpu(&source) }.map_err(str::to_string)?;
        info.src_queue_family = vk::QUEUE_FAMILY_EXTERNAL;
        info.dst_queue_family = self.driver.queue_family;
        info.old_layout = vk::ImageLayout::GENERAL;
        info.src_stage = vk::PipelineStageFlags::ALL_COMMANDS;
        info.src_access = vk::AccessFlags::MEMORY_WRITE;
        let mut sampler = std::ptr::null_mut();
        checked(
            unsafe { kk_nv12_fixture_start(&info, &mut sampler) },
            "acquire-sample-submit",
        );
        let mut rgba = [0.0f32; 64];
        checked(
            unsafe { kk_nv12_fixture_read(sampler, 2_000_000_000, rgba.as_mut_ptr()) },
            "sample-fence-read",
        );
        checked(
            unsafe { kk_nv12_fixture_destroy(sampler) },
            "sample-destroy",
        );
        self.sampled = true;
        let layout = self.image.picture().layout();
        let mut max_error = 0.0f32;
        for row in 0..4 {
            for col in 0..4 {
                let x = ((col * 2 + 1) * layout.width) / 8;
                let y = ((row * 2 + 1) * layout.height) / 8;
                let y_byte = (40 + frame * 12 + (x / 16) * 8 + (y.min(49) / 10) * 6) as f32;
                let cb_byte = (64 + frame * 5) as f32;
                let cr_byte = (192 - frame * 5) as f32;
                let luma = if full {
                    y_byte / 255.0
                } else {
                    (y_byte - 16.0) / 219.0
                };
                let divisor = if full { 255.0 } else { 224.0 };
                let cb = (cb_byte - 128.0) / divisor;
                let cr = (cr_byte - 128.0) / divisor;
                let expected = [
                    luma + 1.402 * cr,
                    luma - 0.344136 * cb - 0.714136 * cr,
                    luma + 1.772 * cb,
                    1.0,
                ];
                for channel in 0..4 {
                    let actual = rgba[(row * 4 + col) * 4 + channel];
                    if !actual.is_finite() {
                        return Err("joined sampler returned nonfinite pixel".into());
                    }
                    max_error = max_error.max((actual - expected[channel]).abs());
                }
            }
        }
        if max_error > 0.025 {
            return Err(format!(
                "joined frame={frame} max RGB error {max_error} exceeds 0.025"
            ));
        }
        println!(
            "frame={frame} joined_image={:?} extent={}x{} full_range={full} sampled_pixels=16 max_rgb_error={max_error}",
            self.image.image(),
            layout.width,
            layout.height
        );
        Ok(())
    }
}
