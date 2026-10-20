//! The real Vulkan driver, opened once, at startup.
//!
//! # Why the driver is open before any application connects
//!
//! docs/design.md §3 draws `sv-gpu-server` with `ash → Intel ICD → iGPU` hanging off
//! the ring consumers, which reads as "open a driver when the app side says
//! `vkCreateInstance`". That is the wrong order, and the arena is what decides
//! it: on the GPU-allocated arena path (see [`crate::arena`]) the driver has to
//! allocate and export the arena **before** the app side can attach at all,
//! because the descriptor it exports is the thing the app side is attaching to.
//!
//! So the driver is opened when the process starts, and the app side's
//! `vkCreateInstance` and `vkCreateDevice` *bind an id* to the instance and
//! device that already exist rather than creating new ones. That also matches
//! what we advertise: §8 gives one physical device, and §9 a curated table, so
//! there was never going to be a second device to create.
//!
//! # Extensions
//!
//! Enabled if present, never required, and each is recorded so the session can
//! say which arena provider is available:
//!
//! | extension | what it buys |
//! |---|---|
//! | `VK_KHR_external_memory_fd` | export the arena as a descriptor the app side maps |
//! | `VK_EXT_external_memory_dma_buf` | make that descriptor a dma-buf, which an unrelated process can map |
//! | `VK_EXT_external_memory_host` | import an arena the *app* side allocated |

use ash::vk;
use std::ffi::CStr;

/// Everything that can go wrong talking to a driver.
#[derive(Debug)]
pub enum DriverError {
    NoLoader(String),
    Vk(&'static str, vk::Result),
    Missing(&'static str),
    Other(String),
}

impl std::fmt::Display for DriverError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DriverError::NoLoader(e) => write!(f, "no Vulkan loader: {e}"),
            DriverError::Vk(what, r) => write!(f, "{what}: {r:?}"),
            DriverError::Missing(what) => write!(f, "the driver does not offer {what}"),
            DriverError::Other(s) => write!(f, "{s}"),
        }
    }
}

impl std::error::Error for DriverError {}

pub type Result<T> = std::result::Result<T, DriverError>;

fn vkerr(what: &'static str) -> impl Fn(vk::Result) -> DriverError {
    move |r| DriverError::Vk(what, r)
}

/// One queue family the GPU side offers to applications, and the one queue it
/// opened on it.
///
/// # Why the index is the driver's own (docs/video-decode.md §7)
///
/// With video decode served, the families an application sees are the driver's
/// families `0..=max(compute, decode)`, **at their own indices**, each with one
/// queue. The alternative — a compact list of two with the decode family
/// renumbered to 1 — would make every `queueFamilyIndex` anywhere in the API a
/// value to translate: `VkCommandPoolCreateInfo`, `VkVideoSessionCreateInfoKHR`,
/// `pQueueFamilyIndices` of every concurrent buffer and image, and the
/// `src`/`dstQueueFamilyIndex` of every barrier inside a recorded command
/// buffer. None of those is typed as a family index in `vk.xml`, so no
/// generated pass could find them. Keeping the driver's numbering means there
/// is nothing to translate, and the price is that an application also sees the
/// families in between (RADV's async-compute family 1, NVIDIA's transfer and
/// compute families 1 and 2) — which are real queues on real families, opened
/// like the others, so seeing them is true rather than merely harmless.
#[derive(Clone, Copy, Debug)]
pub struct ServedFamily {
    /// The driver's properties for this family, with the capability bits this
    /// GPU side does not serve masked out (see [`SERVED_QUEUE_FLAGS`]) and
    /// `queueCount` set to the one queue that exists.
    pub props: vk::QueueFamilyProperties,
    /// `VkQueueFamilyVideoPropertiesKHR::videoCodecOperations`, restricted to
    /// the decode codecs whose extension this device enabled. Empty for a
    /// family that is not served for video.
    pub video_codec_operations: vk::VideoCodecOperationFlagsKHR,
    /// `VkQueueFamilyQueryResultStatusPropertiesKHR::queryResultStatusSupport`.
    pub query_result_status: bool,
    /// The queue opened on this family, index 0.
    pub queue: vk::Queue,
}

/// The queue capabilities a served family may keep. Everything else is masked:
/// `VIDEO_ENCODE` because encode is not part of this stage and an advertised
/// encode family would bring `VK_KHR_video_encode_queue` back through
/// `sv_icd::advertise::unservable_queue_requirement` with nothing measured
/// behind it; `PROTECTED` because protected content is out of scope by
/// decision (docs/design.md §0.21.4); `OPTICAL_FLOW_NV` and the rest because no
/// stage has asked for them.
pub const SERVED_QUEUE_FLAGS: vk::QueueFlags = vk::QueueFlags::from_raw(
    vk::QueueFlags::GRAPHICS.as_raw()
        | vk::QueueFlags::COMPUTE.as_raw()
        | vk::QueueFlags::TRANSFER.as_raw()
        | vk::QueueFlags::SPARSE_BINDING.as_raw()
        | vk::QueueFlags::VIDEO_DECODE_KHR.as_raw(),
);

/// The decode codec each extension stands for, so that a family's
/// `videoCodecOperations` names only codecs this device enabled. A driver whose
/// decode family does VP9 while the profile (or the driver's own extension
/// list) has no `VK_KHR_video_decode_vp9` must not tell an application it can.
pub const DECODE_CODEC_EXTENSIONS: &[(&str, vk::VideoCodecOperationFlagsKHR)] = &[
    ("VK_KHR_video_decode_h264", vk::VideoCodecOperationFlagsKHR::DECODE_H264),
    ("VK_KHR_video_decode_h265", vk::VideoCodecOperationFlagsKHR::DECODE_H265),
    ("VK_KHR_video_decode_av1", vk::VideoCodecOperationFlagsKHR::DECODE_AV1),
    ("VK_KHR_video_decode_vp9", vk::VideoCodecOperationFlagsKHR::from_raw(0x0000_0008)),
];

/// What a driver answers for one decode codec's basic profile: 4:2:0, 8-bit,
/// H.264 High progressive, H.265 Main, AV1 Main without film grain.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodecAnswer {
    /// `vkGetPhysicalDeviceVideoCapabilitiesKHR` failed with this `VkResult`.
    Refused(i32),
    /// It answered: `VkVideoDecodeCapabilitiesKHR::flags` and `maxDpbSlots`.
    Caps { decode_flags: u32, max_dpb_slots: u32 },
    /// Not asked: this side has no profile struct for the codec (VP9).
    NotAsked,
}

impl std::fmt::Display for CodecAnswer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CodecAnswer::Refused(r) => write!(f, "capabilities: VkResult {r}"),
            CodecAnswer::Caps { decode_flags, max_dpb_slots } => {
                write!(f, "decode flags {decode_flags:#x}, {max_dpb_slots} DPB slots")
            }
            CodecAnswer::NotAsked => write!(f, "not asked"),
        }
    }
}

/// The decode codecs whose structures `crate::video_stable` keeps at stable
/// addresses.
pub const STABLE_DECODE_CODECS: &[&str] =
    &["VK_KHR_video_decode_h264", "VK_KHR_video_decode_h265", "VK_KHR_video_decode_av1"];

/// AV1's eight reference frames and the picture being decoded.
pub const AV1_DPB_SLOTS_NEEDED: u32 = 9;

/// Which of the driver's decode codec extensions to withhold, and why: the
/// pure rules, from the driver's answers (docs/video-decode.md §7.12).
///
/// On every driver:
/// - a codec whose basic profile the driver refuses;
/// - a codec whose decode capabilities name neither
///   `DPB_AND_OUTPUT_COINCIDE` nor `DISTINCT`, which the specification forbids
///   and on which ffmpeg refuses to open a decoder;
/// - AV1 with fewer than [`AV1_DPB_SLOTS_NEEDED`] DPB slots. A real stream
///   keeps eight distinct reference pictures and decodes a ninth, and ffmpeg
///   then uses slot index 8 whatever the driver said. Intel 101.7076 reports 8,
///   and so does 101.7092, the newest driver for this GPU (§7.13).
///
/// On a driver that needs the decode structures at stable addresses
/// (`needs_stable`, Intel's Windows driver), also every codec
/// [`STABLE_DECODE_CODECS`] does not name. `h264_only`
/// (`SV_VIDEO_H264_ONLY=1`) withholds all but H.264 on any driver, for testing
/// the fallback.
pub fn withheld_decode_codecs(
    answers: &[(&str, CodecAnswer)],
    needs_stable: bool,
    h264_only: bool,
) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for (name, answer) in answers {
        let why = if h264_only && *name != "VK_KHR_video_decode_h264" {
            Some("SV_VIDEO_H264_ONLY=1".to_string())
        } else if needs_stable && !STABLE_DECODE_CODECS.contains(name) {
            Some(
                "this driver keeps pointers from earlier decode calls, and the GPU side keeps no stable \
                 structures for this codec"
                    .to_string(),
            )
        } else {
            match *answer {
                CodecAnswer::Refused(r) => Some(format!("the driver refuses its basic profile (VkResult {r})")),
                CodecAnswer::Caps { decode_flags, .. } if decode_flags & 0x3 == 0 => Some(format!(
                    "its decode flags {decode_flags:#x} name neither DPB_AND_OUTPUT_COINCIDE nor DISTINCT"
                )),
                CodecAnswer::Caps { max_dpb_slots, .. }
                    if *name == "VK_KHR_video_decode_av1" && max_dpb_slots < AV1_DPB_SLOTS_NEEDED =>
                {
                    Some(format!(
                        "{max_dpb_slots} DPB slots, and AV1 needs {AV1_DPB_SLOTS_NEEDED} (8 references and the \
                         picture being decoded)"
                    ))
                }
                _ => None,
            }
        };
        if let Some(why) = why {
            out.push((name.to_string(), why));
        }
    }
    out
}

/// Ask the driver about one codec's basic decode profile.
///
/// Every structure the driver writes has guard room behind it: Intel's
/// Windows driver writes four bytes past `VkVideoDecodeCapabilitiesKHR`
/// (waycompose-design.md §87).
///
/// # Safety
/// `pd` is a physical device of `instance`, which has `VK_KHR_video_queue`.
unsafe fn codec_answer(entry: &ash::Entry, instance: &ash::Instance, pd: vk::PhysicalDevice, ext: &str) -> CodecAnswer {
    #[repr(C, align(16))]
    struct Room<T> {
        v: T,
        _guard: [u8; 64],
    }
    #[repr(C, align(16))]
    struct Raw([u8; 256]);
    let mut h264 = vk::VideoDecodeH264ProfileInfoKHR::default()
        .std_profile_idc(vk::native::StdVideoH264ProfileIdc_STD_VIDEO_H264_PROFILE_IDC_HIGH)
        .picture_layout(vk::VideoDecodeH264PictureLayoutFlagsKHR::PROGRESSIVE);
    let mut h265 = vk::VideoDecodeH265ProfileInfoKHR::default()
        .std_profile_idc(vk::native::StdVideoH265ProfileIdc_STD_VIDEO_H265_PROFILE_IDC_MAIN);
    let mut av1 = vk::VideoDecodeAV1ProfileInfoKHR::default()
        .std_profile(vk::native::StdVideoAV1Profile_STD_VIDEO_AV1_PROFILE_MAIN)
        .film_grain_support(false);
    let (op, codec_profile, caps_type): (_, *const std::ffi::c_void, vk::StructureType) = match ext {
        "VK_KHR_video_decode_h264" => (
            vk::VideoCodecOperationFlagsKHR::DECODE_H264,
            (&raw mut h264).cast(),
            vk::StructureType::VIDEO_DECODE_H264_CAPABILITIES_KHR,
        ),
        "VK_KHR_video_decode_h265" => (
            vk::VideoCodecOperationFlagsKHR::DECODE_H265,
            (&raw mut h265).cast(),
            vk::StructureType::VIDEO_DECODE_H265_CAPABILITIES_KHR,
        ),
        "VK_KHR_video_decode_av1" => (
            vk::VideoCodecOperationFlagsKHR::DECODE_AV1,
            (&raw mut av1).cast(),
            vk::StructureType::VIDEO_DECODE_AV1_CAPABILITIES_KHR,
        ),
        _ => return CodecAnswer::NotAsked,
    };
    let mut profile = vk::VideoProfileInfoKHR::default()
        .video_codec_operation(op)
        .chroma_subsampling(vk::VideoChromaSubsamplingFlagsKHR::TYPE_420)
        .luma_bit_depth(vk::VideoComponentBitDepthFlagsKHR::TYPE_8)
        .chroma_bit_depth(vk::VideoComponentBitDepthFlagsKHR::TYPE_8);
    profile.p_next = codec_profile;
    // The codec's capabilities struct, as raw bytes with its header written:
    // each is `sType`, `pNext`, then fields the driver fills.
    let mut codec = Box::new(Raw([0; 256]));
    // SAFETY: the buffer is 16-aligned and larger than any of the three.
    unsafe {
        let head = codec.0.as_mut_ptr() as *mut vk::BaseOutStructure<'static>;
        (*head).s_type = caps_type;
        (*head).p_next = std::ptr::null_mut();
    }
    let mut decode = Box::new(Room { v: vk::VideoDecodeCapabilitiesKHR::default(), _guard: [0; 64] });
    decode.v.p_next = codec.0.as_mut_ptr().cast();
    let mut caps = Box::new(Room { v: vk::VideoCapabilitiesKHR::default(), _guard: [0; 64] });
    caps.v.p_next = (&raw mut decode.v).cast();
    let vq = ash::khr::video_queue::Instance::new(entry, instance);
    // SAFETY: every structure is live, chained and has room behind it.
    let r = unsafe { (vq.fp().get_physical_device_video_capabilities_khr)(pd, &profile, &mut caps.v) };
    if r != vk::Result::SUCCESS {
        return CodecAnswer::Refused(r.as_raw());
    }
    CodecAnswer::Caps { decode_flags: decode.v.flags.as_raw(), max_dpb_slots: caps.v.max_dpb_slots }
}

/// Which families to serve, from the driver's family table: the pure decision,
/// separated from the Vulkan calls so that it can be tested.
///
/// Returns the driver family indices to open a queue on, in order, and the
/// decode family if one is served. Without decode it is exactly the one
/// compute family the GPU side always opened — the behaviour before this stage,
/// unchanged, so that `SV_VIDEO_DECODE=0` or a driver without decode puts back
/// what every other gate was measured against.
pub fn choose_families(
    flags: &[vk::QueueFlags],
    codec_ops: &[vk::VideoCodecOperationFlagsKHR],
    decode_wanted: bool,
) -> Option<(Vec<u32>, Option<u32>)> {
    let compute = flags.iter().position(|q| q.contains(vk::QueueFlags::COMPUTE))? as u32;
    let decode = decode_wanted
        .then(|| {
            flags.iter().zip(codec_ops).position(|(q, ops)| {
                q.contains(vk::QueueFlags::VIDEO_DECODE_KHR) && !ops.is_empty()
            })
        })
        .flatten()
        .map(|i| i as u32);
    match decode {
        // The prefix, so that every served family is at its own index.
        Some(d) => Some(((0..=compute.max(d)).collect(), Some(d))),
        None => Some((vec![compute], None)),
    }
}

/// Whether video decode is wanted at all. On unless `SV_VIDEO_DECODE=0`, so
/// that the shape before this stage stays one variable away for anyone
/// bisecting a regression to it.
pub fn decode_wanted_by_environment() -> bool {
    std::env::var("SV_VIDEO_DECODE").map(|v| v != "0").unwrap_or(true)
}

/// What [`Driver::open_inner`] opened, before the instance-level parts join it.
/// A struct because the tuple it replaces had grown to eight unnamed fields.
struct Opened {
    pd: vk::PhysicalDevice,
    device: ash::Device,
    queue: vk::Queue,
    queue_family: u32,
    served: Vec<ServedFamily>,
    decode_family: Option<u32>,
    external_fd: Option<ash::khr::external_memory_fd::Device>,
    has_dma_buf: bool,
    external_host: Option<ash::ext::external_memory_host::Device>,
    buffer_device_address: bool,
    features: sv_proto::VkPhysicalDeviceFeatures2,
    enabled_extensions: Vec<String>,
    withheld_extensions: Vec<String>,
}

/// `VkPhysicalDeviceDriverProperties::driverID`, if the device has the query.
///
/// The instance is at least 1.1 (`open`), so `vkGetPhysicalDeviceProperties2` is
/// core; the structure is the device's to answer, at 1.2 or with
/// `VK_KHR_driver_properties`.
fn driver_id(instance: &ash::Instance, pd: vk::PhysicalDevice, api_version: u32) -> i32 {
    let has_ext = unsafe { instance.enumerate_device_extension_properties(pd) }
        .map(|exts| {
            exts.iter()
                .any(|e| unsafe { CStr::from_ptr(e.extension_name.as_ptr()) } == c"VK_KHR_driver_properties")
        })
        .unwrap_or(false);
    if api_version < vk::API_VERSION_1_2 && !has_ext {
        return 0;
    }
    let mut dp = vk::PhysicalDeviceDriverProperties::default();
    let mut p2 = vk::PhysicalDeviceProperties2::default().push_next(&mut dp);
    unsafe { instance.get_physical_device_properties2(pd, &mut p2) };
    dp.driver_id.as_raw()
}

/// Load the Vulkan loader (waycompose-design.md §103.6).
///
/// `ash::Entry::load` asks the dynamic linker for `libvulkan.so.1`, or on macOS
/// for `libvulkan.dylib`. On macOS the loader is not on the default search path
/// when it comes from MacPorts (`/opt/local/lib`) or Homebrew
/// (`/opt/homebrew/lib`), and a process started by `sshd` or `launchd` has no
/// `DYLD_LIBRARY_PATH` unless it was given one. So, in order: `SV_VULKAN_LOADER`
/// if set (a path, used alone); then the linker's own search with both names;
/// then the three install prefixes. The loader itself still honours
/// `VK_DRIVER_FILES` / `VK_ICD_FILENAMES`, which is how a run picks lavapipe.
fn load_entry() -> Result<ash::Entry> {
    if let Some(path) = std::env::var_os("SV_VULKAN_LOADER") {
        return unsafe { ash::Entry::load_from(&path) }
            .map_err(|e| DriverError::NoLoader(format!("SV_VULKAN_LOADER={}: {e}", path.to_string_lossy())));
    }
    #[cfg(target_os = "macos")]
    {
        let mut tried = Vec::new();
        for path in [
            "libvulkan.dylib",
            "libvulkan.1.dylib",
            "/usr/local/lib/libvulkan.1.dylib",
            "/opt/local/lib/libvulkan.1.dylib",
            "/opt/homebrew/lib/libvulkan.1.dylib",
        ] {
            match unsafe { ash::Entry::load_from(path) } {
                Ok(e) => return Ok(e),
                Err(e) => tried.push(format!("{path}: {e}")),
            }
        }
        Err(DriverError::NoLoader(tried.join("; ")))
    }
    #[cfg(not(target_os = "macos"))]
    unsafe { ash::Entry::load() }.map_err(|e| DriverError::NoLoader(e.to_string()))
}

/// The instance extensions this side enables when the loader has them, in the
/// order they are passed. One list, read by [`Driver::open`] to create the
/// instance and by the device's `depends=` closure, so the two cannot disagree.
///
/// * `VK_EXT_debug_utils`: Zink names every object (see `Driver::open`).
/// * `VK_KHR_surface`: makes `VK_KHR_swapchain`'s layout legal (same).
/// * `VK_KHR_portability_enumeration` (§103.6): a loader hides every ICD whose
///   manifest says `is_portability_driver` (MoltenVK, KosmicKrisp) from an
///   instance that does not enable this and set
///   `VK_INSTANCE_CREATE_ENUMERATE_PORTABILITY_BIT_KHR`. Without it, a Mac
///   with only MoltenVK reports `VK_ERROR_INCOMPATIBLE_DRIVER` from
///   `vkCreateInstance` even on real Metal.
pub fn wanted_instance_extensions(entry: &ash::Entry) -> Vec<&'static CStr> {
    let avail = unsafe { entry.enumerate_instance_extension_properties(None) }.unwrap_or_default();
    let has = |want: &CStr| avail.iter().any(|e| unsafe { CStr::from_ptr(e.extension_name.as_ptr()) } == want);
    [c"VK_EXT_debug_utils", c"VK_KHR_surface", c"VK_KHR_portability_enumeration"]
        .into_iter()
        .filter(|w| has(w))
        .collect()
}

/// `VK_DRIVER_ID_MESA_KOSMICKRISP`, newer than ash's registry (vk.xml 1.4.350).
pub const DRIVER_ID_MESA_KOSMICKRISP: i32 = 28;
/// `VK_DRIVER_ID_MOLTENVK`.
pub const DRIVER_ID_MOLTENVK: i32 = 14;

/// Which physical device [`Driver::open_choice`] opens.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeviceChoice {
    /// By its index in `vkEnumeratePhysicalDevices`, as `--device N` says.
    Index(usize),
    /// The best by [`device_rank`]: what the macOS entry uses when no
    /// `--device` is given.
    Preferred,
}

/// The order [`DeviceChoice::Preferred`] takes, lower first (§103.6):
///
/// 1. KosmicKrisp (Mesa on Metal; Apple Silicon, macOS 26), conformant;
/// 2. MoltenVK (Metal; every Mac, the only one on Intel);
/// 3. any other discrete, 4. integrated, 5. virtual or other GPU;
/// 6. a CPU device (lavapipe, SwiftShader), the fallback.
///
/// `prefer_cpu` (`SV_GPU_PREFER=cpu`) ranks the CPU device 0, ahead of all,
/// for a test that wants lavapipe on a machine that also has a GPU. Ties keep
/// the loader's order.
pub fn device_rank(device_type: vk::PhysicalDeviceType, driver_id: i32, prefer_cpu: bool) -> u32 {
    if device_type == vk::PhysicalDeviceType::CPU {
        return if prefer_cpu { 0 } else { 6 };
    }
    match (driver_id, device_type) {
        (DRIVER_ID_MESA_KOSMICKRISP, _) => 1,
        (DRIVER_ID_MOLTENVK, _) => 2,
        (_, vk::PhysicalDeviceType::DISCRETE_GPU) => 3,
        (_, vk::PhysicalDeviceType::INTEGRATED_GPU) => 4,
        _ => 5,
    }
}

/// The index [`device_rank`] picks among `devices`, `(type, driverID)` each.
pub fn preferred_device(devices: &[(vk::PhysicalDeviceType, i32)], prefer_cpu: bool) -> Option<usize> {
    (0..devices.len()).min_by_key(|&i| (device_rank(devices[i].0, devices[i].1, prefer_cpu), i))
}

/// Whether `SV_GPU_PREFER=cpu` asked for the CPU device first.
pub fn prefer_cpu_by_environment() -> bool {
    std::env::var("SV_GPU_PREFER").is_ok_and(|v| v.eq_ignore_ascii_case("cpu"))
}

/// One open driver: instance, physical device, device, queue.
pub struct Driver {
    #[cfg(target_os = "macos")]
    native_submissions: std::sync::Mutex<Option<std::sync::Arc<crate::video_worker::Queue>>>,
    /// Held rather than read: it owns the loaded `libvulkan.so`, and dropping it
    /// while an instance is live would unload the loader out from under the
    /// driver. `gpu-probe` learned the same thing.
    #[allow(dead_code)]
    pub entry: std::mem::ManuallyDrop<ash::Entry>,
    pub instance: ash::Instance,
    pub pd: vk::PhysicalDevice,
    pub device: ash::Device,
    pub queue: vk::Queue,
    pub queue_family: u32,
    /// The families applications see, by the index they see them at.
    ///
    /// One entry — the compute family above, presented as family 0 — unless
    /// video decode is served, in which case it is the driver's own prefix
    /// `0..=max(compute, decode)` (see [`ServedFamily`]). `queue` and
    /// `queue_family` stay what the GPU side uses for its own work (the arena
    /// probe, `svPresentBlit`); this is what `vkGetDeviceQueue` hands out.
    pub served: Vec<ServedFamily>,
    /// The served decode family, if any, by its (driver and application) index.
    pub decode_family: Option<u32>,
    /// `VK_KHR_video_queue`'s device entry points, when decode is served: for
    /// the one video command this side answers by hand,
    /// `server::get_video_session_memory_requirements_khr`.
    pub video_queue: Option<ash::khr::video_queue::Device>,
    pub props: vk::PhysicalDeviceProperties,
    pub mem_props: vk::PhysicalDeviceMemoryProperties,
    /// What the loader on *this* side answers `vkEnumerateInstanceVersion` with,
    /// before [`Driver::open`] clamps it down to what the profile uses.
    ///
    /// Kept unclamped and reported unclamped over `svDriverVersions`, because
    /// this side's job is to say what the driver is, not what the app side may
    /// advertise. The clamp is `sv_gpu_server`'s business only for the instance
    /// it creates for itself; the *advertised* number is negotiated on the app
    /// side, once, in `sv_icd::state::negotiate` — see docs/design.md §0.18.
    pub instance_api_version: u32,
    pub name: String,
    /// `VK_KHR_external_memory_fd`, and its dispatch table.
    pub external_fd: Option<ash::khr::external_memory_fd::Device>,
    pub has_dma_buf: bool,
    /// `VK_EXT_external_memory_host`, and its dispatch table.
    pub external_host: Option<ash::ext::external_memory_host::Device>,
    /// `VK_EXT_debug_utils`, for the one command whose handle argument the
    /// generated passes cannot see (`server::set_debug_utils_object_name_ext`).
    pub debug_utils: Option<ash::ext::debug_utils::Device>,
    /// Whether `VkPhysicalDeviceVulkan12Features::bufferDeviceAddress` came back
    /// true and was therefore enabled on this device.
    ///
    /// Read by [`crate::arena`], and this is the whole reason it is a field. A
    /// buffer whose address a shader may take must be bound to memory allocated
    /// with `VK_MEMORY_ALLOCATE_DEVICE_ADDRESS_BIT`
    /// (`VUID-VkMemoryAllocateInfo-flags-03331` in the other direction:
    /// `VUID-VkBufferDeviceAddressInfo-buffer-02601`), and **the arena is one
    /// allocation made before any application connects**, so the decision has to
    /// be taken here or not at all. Setting the bit unconditionally is invalid
    /// usage on a driver without the feature, so it is conditional on this.
    pub buffer_device_address: bool,
    /// `VkPhysicalDeviceDriverProperties::driverID`, or 0 where the device
    /// cannot say (below 1.2 without `VK_KHR_driver_properties`).
    ///
    /// Read for one reason: [`crate::set_layout`] refuses a shape on Intel's
    /// Windows driver, and the device name alone does not tell that driver
    /// from ANV on the same iGPU.
    pub driver_id: i32,
    /// The device's enabled features: the chain [`crate::features::device_chain`]
    /// built, as the driver filled it in. An application's `vkCreateDevice` is
    /// checked against it ([`crate::features::not_enabled`]).
    pub features: sv_proto::VkPhysicalDeviceFeatures2,
    /// The device extensions this device was created with.
    pub enabled_extensions: Vec<String>,
    /// Extensions the driver has that this side deliberately neither enables
    /// nor advertises (Intel's non-H.264 decode, docs/video-decode.md §7.11.5).
    pub withheld_extensions: Vec<String>,
}

impl Driver {
    /// Open device `index`, or the first one, and enable whatever it offers.
    pub fn open(index: usize) -> Result<Driver> {
        Driver::open_choice(DeviceChoice::Index(index))
    }

    /// Open the device `choice` names, and enable whatever it offers.
    pub fn open_choice(choice: DeviceChoice) -> Result<Driver> {
        let _one = OPEN_CLOSE.lock().unwrap_or_else(|p| p.into_inner());
        let entry = load_entry()?;
        // 1.1 was the floor the arena needed and it is **not** enough for Zink:
        // `scalarBlockLayout` and `timelineSemaphore` live in
        // `VkPhysicalDeviceVulkan12Features` and `dynamicRendering` in the 1.3
        // block, and a 1.1 instance cannot even name those structures. Ask for
        // what the loader says is available, capped at what the wire carries —
        // asking for more than the driver has makes `vkCreateInstance` fail
        // outright, and asking for more than the profile knows buys nothing.
        //
        // The cap used to be the literal 1.3 and is now
        // [`sv_proto::CORE_API_VERSION`], which *is* 1.3 today and is derived
        // from the profile. The difference matters the day the profile gains
        // `VK_VERSION_1_4`: this instance rises with it in the same edit rather
        // than being the one place that was forgotten. `max` against 1.1 because
        // a profile below that is a profile this side cannot serve at all, and
        // `clamp` panics rather than saturating when its bounds cross.
        let available = unsafe { entry.try_enumerate_instance_version() }
            .ok()
            .flatten()
            .unwrap_or(vk::API_VERSION_1_0);
        let ceiling = sv_proto::CORE_API_VERSION.max(vk::API_VERSION_1_1);
        let want = available.clamp(vk::API_VERSION_1_1, ceiling);
        let app = vk::ApplicationInfo::default()
            .application_name(c"sv-gpu-server")
            .api_version(want);
        // `VK_EXT_debug_utils` if the loader has it. Zink names every object it
        // creates, and without the extension enabled here
        // `vkSetDebugUtilsObjectNameEXT` resolves to null on this side and every
        // one of those calls is a forwarded command with nothing behind it.
        // It is an *instance* extension whose device entry points the loader
        // itself provides, so enabling it costs one string and nothing else.
        //
        // (The rest of the list, and `VK_KHR_portability_enumeration`, are in
        // [`wanted_instance_extensions`].)
        // `VK_KHR_surface`, if the loader has it, for one reason and not for
        // presentation: `VK_KHR_swapchain` is in the profile (docs/design.md §10) so
        // that the device enables it and
        // `VK_IMAGE_LAYOUT_PRESENT_SRC_KHR` exists, and a device extension may
        // only be enabled when its instance-level dependency is. This side
        // creates no surface and never will — the window is on the *other* side
        // of the boundary — so the extension is enabled purely to make the
        // layout legal.
        let wanted = wanted_instance_extensions(&entry);
        let portability = wanted.contains(&c"VK_KHR_portability_enumeration");
        let inst_exts: Vec<*const std::ffi::c_char> = wanted.iter().map(|c| c.as_ptr()).collect();
        let ci = vk::InstanceCreateInfo::default()
            .application_info(&app)
            .enabled_extension_names(&inst_exts)
            .flags(if portability {
                vk::InstanceCreateFlags::ENUMERATE_PORTABILITY_KHR
            } else {
                vk::InstanceCreateFlags::empty()
            });
        let instance = unsafe { entry.create_instance(&ci, None) }.map_err(|e| {
            // §103.6: the one failure a Mac without Metal (a VM) always has,
            // said in words rather than as a code three layers up.
            if e == vk::Result::ERROR_INCOMPATIBLE_DRIVER {
                let hint = if cfg!(target_os = "macos") {
                    "; MoltenVK and KosmicKrisp need Metal, so a Mac VM without a Metal GPU \
                     has none: set VK_DRIVER_FILES to lavapipe's manifest"
                } else {
                    "; VK_LOADER_DEBUG=driver says why each manifest was skipped"
                };
                DriverError::Other(format!(
                    "vkCreateInstance: {e:?}: the loader found no usable Vulkan driver \
                     (portability enumeration {}{hint})",
                    if portability { "enabled" } else { "not offered by this loader" }
                ))
            } else {
                DriverError::Vk("vkCreateInstance", e)
            }
        })?;

        let opened = Driver::open_on(entry, instance, choice, available);
        if opened.is_err() {
            // Nothing else holds the instance if device creation failed, and
            // leaking it would keep the driver's own threads alive for the life
            // of the process.
            //
            // (`open_on` takes the instance by value and destroys it on its own
            // error paths; this branch exists only to say so.)
        }
        opened
    }

    fn open_on(
        entry: ash::Entry,
        instance: ash::Instance,
        choice: DeviceChoice,
        instance_api_version: u32,
    ) -> Result<Driver> {
        let r = Driver::open_inner(&entry, &instance, choice);
        match r {
            Ok(Opened {
                pd,
                device,
                queue,
                queue_family,
                served,
                decode_family,
                external_fd,
                has_dma_buf,
                external_host,
                buffer_device_address,
                features,
                enabled_extensions,
                withheld_extensions,
            }) => {
                let props = unsafe { instance.get_physical_device_properties(pd) };
                let name = unsafe { CStr::from_ptr(props.device_name.as_ptr()) }
                    .to_string_lossy()
                    .into_owned();
                let mem_props = unsafe { instance.get_physical_device_memory_properties(pd) };
                let driver_id = driver_id(&instance, pd, props.api_version);
                // Instance-level extension, device-level entry points, provided
                // by the loader rather than by the driver — so it is available
                // whenever the instance enabled it, which `open` does.
                let debug_utils = Some(ash::ext::debug_utils::Device::new(&instance, &device));
                let video_queue = decode_family
                    .is_some()
                    .then(|| ash::khr::video_queue::Device::new(&instance, &device));
                Ok(Driver {
                    #[cfg(target_os = "macos")]
                    native_submissions: Default::default(),
                    entry: std::mem::ManuallyDrop::new(entry),
                    instance,
                    pd,
                    device,
                    queue,
                    queue_family,
                    served,
                    decode_family,
                    video_queue,
                    props,
                    mem_props,
                    instance_api_version,
                    name,
                    external_fd,
                    has_dma_buf,
                    external_host,
                    debug_utils,
                    buffer_device_address,
                    driver_id,
                    features,
                    enabled_extensions,
                    withheld_extensions,
                })
            }
            Err(e) => {
                unsafe { instance.destroy_instance(None) };
                Err(e)
            }
        }
    }

    /// Whether a PUSH_DESCRIPTOR set layout with non-zero binding flags is
    /// refused rather than forwarded on this driver (docs/design.md §0.29).
    pub fn refuses_push_binding_flags(&self) -> bool {
        crate::set_layout::refuses_push_binding_flags(self.props.vendor_id, self.driver_id)
    }

    fn open_inner(entry: &ash::Entry, instance: &ash::Instance, choice: DeviceChoice) -> Result<Opened> {

        let pds =
            unsafe { instance.enumerate_physical_devices() }.map_err(vkerr("vkEnumeratePhysicalDevices"))?;
        let index = match choice {
            DeviceChoice::Index(i) => i,
            DeviceChoice::Preferred => {
                let traits: Vec<(vk::PhysicalDeviceType, i32)> = pds
                    .iter()
                    .map(|&pd| {
                        let p = unsafe { instance.get_physical_device_properties(pd) };
                        (p.device_type, driver_id(instance, pd, p.api_version))
                    })
                    .collect();
                let prefer_cpu = prefer_cpu_by_environment();
                let i = preferred_device(&traits, prefer_cpu)
                    .ok_or_else(|| DriverError::Other("no physical device (0 present)".into()))?;
                if pds.len() > 1 {
                    eprintln!(
                        "sv-gpu-server: {} devices {traits:?}; chose {i} by the §103.6 order{}",
                        pds.len(),
                        if prefer_cpu { " with SV_GPU_PREFER=cpu" } else { "" }
                    );
                }
                i
            }
        };
        let &pd = pds.get(index).ok_or_else(|| {
            DriverError::Other(format!("no physical device {index} ({} present)", pds.len()))
        })?;

        let exts = unsafe { instance.enumerate_device_extension_properties(pd) }
            .map_err(vkerr("vkEnumerateDeviceExtensionProperties"))?;
        let has = |want: &CStr| {
            exts.iter().any(|e| unsafe { CStr::from_ptr(e.extension_name.as_ptr()) } == want)
        };
        let has_fd = has(c"VK_KHR_external_memory_fd");
        let has_dma_buf = has_fd && has(c"VK_EXT_external_memory_dma_buf");
        let has_host = has(c"VK_EXT_external_memory_host");

        let qfams = unsafe { instance.get_physical_device_queue_family_properties(pd) };
        // The video half of the family table, asked only of a driver that has
        // the extension: `VkQueueFamilyVideoPropertiesKHR` in a chain the driver
        // does not know is invalid usage. Count-then-fill through the `2` form,
        // which a 1.1 instance has.
        let has_video = has(c"VK_KHR_video_queue") && has(c"VK_KHR_video_decode_queue");
        let (codec_ops, result_status): (Vec<_>, Vec<_>) = if has_video {
            let n = unsafe { instance.get_physical_device_queue_family_properties2_len(pd) };
            let mut vids = vec![vk::QueueFamilyVideoPropertiesKHR::default(); n];
            let mut qrs = vec![vk::QueueFamilyQueryResultStatusPropertiesKHR::default(); n];
            let mut fams = vec![vk::QueueFamilyProperties2::default(); n];
            for ((f, v), q) in fams.iter_mut().zip(vids.iter_mut()).zip(qrs.iter_mut()) {
                *f = f.push_next(v).push_next(q);
            }
            unsafe { instance.get_physical_device_queue_family_properties2(pd, &mut fams) };
            drop(fams);
            (
                vids.iter().map(|v| v.video_codec_operations).collect(),
                qrs.iter().map(|q| q.query_result_status_support != 0).collect(),
            )
        } else {
            (
                vec![vk::VideoCodecOperationFlagsKHR::empty(); qfams.len()],
                vec![false; qfams.len()],
            )
        };
        // Decode is served when the driver has the extensions, the profile
        // forwards them, and nobody turned it off. The profile half matters:
        // a build whose profile lacks `VK_KHR_video_decode_queue` has no
        // forwarders for its commands, and opening a family nothing can record
        // on would be §0.22's over-promise from the other end.
        let profile_has = |n: &str| sv_gpu_entry::PROFILE_DEVICE_EXTENSIONS.contains(&n);
        let decode_wanted = has_video
            && profile_has("VK_KHR_video_queue")
            && profile_has("VK_KHR_video_decode_queue")
            && decode_wanted_by_environment();
        let flags: Vec<vk::QueueFlags> = qfams.iter().map(|q| q.queue_flags).collect();
        // COMPUTE and TRANSFER, because the acceptance workload is a dispatch
        // and the present path is a copy. Any family with COMPUTE has TRANSFER
        // implicitly per the specification, but asking says what we need.
        let (open_on, decode_family) = choose_families(&flags, &codec_ops, decode_wanted)
            .ok_or(DriverError::Missing("a compute-capable queue family"))?;
        let queue_family = flags
            .iter()
            .position(|q| q.contains(vk::QueueFlags::COMPUTE))
            .ok_or(DriverError::Missing("a compute-capable queue family"))? as u32;

        let prio = [1.0f32];
        let qi: Vec<vk::DeviceQueueCreateInfo> = open_on
            .iter()
            .map(|&f| {
                vk::DeviceQueueCreateInfo::default().queue_family_index(f).queue_priorities(&prio)
            })
            .collect();

        // Every extension the profile forwards that this driver actually has,
        // plus the three the arena needs.
        //
        // # Why everything, rather than what the application asked for
        //
        // The GPU side opens its device at startup, before any application
        // connects, because on the export arena path the arena *is* a driver
        // allocation (see this module's header). So by the time
        // `vkCreateDevice` arrives the device exists and its enabled set is
        // fixed. The only way for the answer to "did I get what I asked for" to
        // be yes is to have enabled everything the driver could have said yes
        // to. This is a real limitation of opening early, and it is recorded in
        // `server::create_device` from the other end.
        let mut owned: Vec<std::ffi::CString> = Vec::new();
        for want in sv_gpu_entry::PROFILE_DEVICE_EXTENSIONS {
            let Ok(c) = std::ffi::CString::new(want) else { continue };
            if has(c.as_c_str()) {
                owned.push(c);
            }
        }
        for want in [
            c"VK_KHR_external_memory_fd",
            c"VK_EXT_external_memory_dma_buf",
            c"VK_EXT_external_memory_host",
        ] {
            if has(want) && !owned.iter().any(|o| o.as_c_str() == want) {
                owned.push(want.to_owned());
            }
        }
        // **Close the enabled set under `depends=`**, against the two sets that
        // can satisfy one: the device extensions about to be enabled, and the
        // *instance* extensions this instance actually enabled above.
        //
        // Enabling "every profile extension the driver has" is not the same as
        // enabling a legal set, and the validation layer said so —
        // `VUID-vkCreateDevice-ppEnabledExtensionNames-01387`, three times:
        // `VK_EXT_display_control` without `VK_EXT_display_surface_counter`,
        // `VK_EXT_present_timing` without `VK_KHR_get_surface_capabilities2`,
        // `VK_EXT_swapchain_maintenance1` without `VK_EXT_surface_maintenance1`.
        // Every one of those dependencies is an **instance** extension, so no
        // amount of looking at the device list could have found them; this is
        // the same "right about the extension, wrong about the set it reads"
        // shape `sv_icd`'s own closure records (docs/design.md §0.23).
        //
        // A fixpoint rather than one pass, because dependency chains are deeper
        // than one link and a single pass keeps whatever depended on what it
        // just removed.
        // The two `Driver::open` enables, re-derived by the same rule it used —
        // "available on this loader" — rather than threaded through, so the two
        // places cannot say different things about one instance.
        let instance_enabled: Vec<&str> =
            wanted_instance_extensions(entry).iter().filter_map(|c| c.to_str().ok()).collect();
        loop {
            let present: Vec<String> =
                owned.iter().filter_map(|c| c.to_str().ok().map(str::to_string)).collect();
            let have = |n: &str| {
                // A core version satisfies a `depends=` on itself; this device
                // is created against the instance's own version, capped at 1.3.
                if let Some(minor) =
                    n.strip_prefix("VK_VERSION_1_").and_then(|m| m.parse::<u32>().ok())
                {
                    return minor <= 3;
                }
                present.iter().any(|o| o == n) || instance_enabled.contains(&n)
            };
            let drop: Vec<String> = present
                .iter()
                .filter(|n| !sv_gpu_entry::depends_satisfied(n, &have))
                .cloned()
                .collect();
            if drop.is_empty() {
                break;
            }
            owned.retain(|c| !c.to_str().is_ok_and(|s| drop.iter().any(|d| d == s)));
        }
        // `VK_EXT_buffer_device_address` is the pre-1.2 form of a core feature
        // this device enables through `VkPhysicalDeviceVulkan12Features`, and
        // the two together are `VUID-VkDeviceCreateInfo-pNext-04748` (one of
        // docs/video-decode.md §7.5's three standing messages). Its feature
        // struct goes with it; an application chaining that struct is checked
        // against the core block by member name (`crate::features::not_enabled`).
        let dev_props = unsafe { instance.get_physical_device_properties(pd) };
        let minor = vk::api_version_minor(dev_props.api_version)
            .min(vk::api_version_minor(sv_proto::CORE_API_VERSION));
        if minor >= 2 {
            owned.retain(|c| c.as_c_str() != c"VK_EXT_buffer_device_address");
        }
        // Which decode codecs to serve (docs/video-decode.md §7.11.5, §7.12).
        // Firefox decides what it can decode in hardware from the extension
        // list, so a codec is withheld extension and all: masking the
        // family's codec bits alone left it offering AV1 and H.265 in
        // hardware. The rules are `withheld_decode_codecs`'; the answers they
        // read are the driver's own, asked once here.
        let withheld: Vec<String> = if decode_wanted {
            let present: Vec<&str> = DECODE_CODEC_EXTENSIONS
                .iter()
                .map(|(n, _)| *n)
                .filter(|n| owned.iter().any(|o| o.to_bytes() == n.as_bytes()))
                .collect();
            // SAFETY: `pd` is this instance's device, and `has_video` said it has
            // `VK_KHR_video_queue` (`decode_wanted` implies it).
            let answers: Vec<(&str, CodecAnswer)> =
                present.iter().map(|n| (*n, unsafe { codec_answer(entry, instance, pd, n) })).collect();
            let needs_stable = crate::set_layout::refuses_push_binding_flags(
                dev_props.vendor_id,
                driver_id(instance, pd, dev_props.api_version),
            );
            let h264_only = std::env::var("SV_VIDEO_H264_ONLY").is_ok_and(|v| v == "1");
            let gone = withheld_decode_codecs(&answers, needs_stable, h264_only);
            let served: Vec<String> = answers
                .iter()
                .filter(|(n, _)| !gone.iter().any(|(g, _)| g == n))
                .map(|(n, a)| format!("{n} ({a})"))
                .collect();
            eprintln!("sv-gpu-server: video decode served for: {} (docs/video-decode.md §7.12)", served.join(", "));
            for (n, why) in &gone {
                eprintln!("sv-gpu-server: video decode withheld: {n}: {why}");
            }
            owned.retain(|c| !gone.iter().any(|(g, _)| c.to_bytes() == g.as_bytes()));
            gone.into_iter().map(|(n, _)| n).collect()
        } else {
            Vec::new()
        };
        // §103.6: a portability implementation (MoltenVK, KosmicKrisp) lists
        // `VK_KHR_portability_subset`, and the specification requires it be
        // enabled whenever it is listed (`VUID-VkDeviceCreateInfo-pProperties-04451`).
        // Not a profile extension, so nothing forwards it; it is enabled for the
        // device's own validity only. Its one dependency,
        // `VK_KHR_get_physical_device_properties2`, is core in this 1.1+ instance.
        if has(c"VK_KHR_portability_subset") && !owned.iter().any(|o| o.as_c_str() == c"VK_KHR_portability_subset") {
            owned.push(c"VK_KHR_portability_subset".to_owned());
        }
        let names: Vec<*const std::ffi::c_char> = owned.iter().map(|c| c.as_ptr()).collect();

        // The features: every feature struct the device may be created with
        // whose extension (or core version) is enabled, as the registry says
        // (`crate::features`), filled in by the driver and enabled as answered.
        // This replaces a hand-kept list of two dozen structs that had fallen
        // behind the profile: `VK_EXT_extended_dynamic_state3` was enabled and
        // its struct never chained, so its features were enabled false while
        // the forwarded `vkGetPhysicalDeviceFeatures2` told Zink they were
        // supported (docs/video-decode.md §7.11).
        let enabled_names: Vec<&str> = owned.iter().filter_map(|c| c.to_str().ok()).collect();
        let template = sv_proto::VkPhysicalDeviceFeatures2 {
            p_next: crate::features::device_chain(minor, &enabled_names),
            ..Default::default()
        };
        let mut wanted =
            crate::features::query_driver(instance, pd, &template).map_err(DriverError::Other)?;
        let dropped = crate::features::resolve_exclusive(&mut wanted.p_next);
        if !dropped.is_empty() {
            eprintln!("sv-gpu-server: not enabling {dropped:?}: exclusive with a feature kept");
        }
        let mut pool = sv_gpu_entry::Raise::default();
        let c_features = sv_gpu_entry::raise_vk_physical_device_features2(&wanted, &mut pool)
            .map_err(|e| DriverError::Other(format!("raising the feature chain: {e:?}")))?;

        let mut dci = vk::DeviceCreateInfo::default()
            .queue_create_infos(&qi)
            .enabled_extension_names(&names);
        dci.p_next = (&raw const c_features).cast();
        let device =
            unsafe { instance.create_device(pd, &dci, None) }.map_err(vkerr("vkCreateDevice"))?;
        drop(pool);
        let queue = unsafe { device.get_device_queue(queue_family, 0) };
        // The codecs this device enabled, for masking the decode family's
        // `videoCodecOperations` — an application told "VP9" by the family and
        // refused `VK_KHR_video_decode_vp9` by the extension list has been told
        // two different things.
        let codecs = DECODE_CODEC_EXTENSIONS
            .iter()
            .filter(|(n, _)| owned.iter().any(|o| o.to_bytes() == n.as_bytes()))
            .fold(vk::VideoCodecOperationFlagsKHR::empty(), |acc, (_, op)| acc | *op);
        let served: Vec<ServedFamily> = open_on
            .iter()
            .map(|&f| {
                let mut props = qfams[f as usize];
                props.queue_count = 1;
                let decode = Some(f) == decode_family;
                if decode_family.is_some() {
                    props.queue_flags &= SERVED_QUEUE_FLAGS;
                }
                ServedFamily {
                    props,
                    video_codec_operations: if decode {
                        codec_ops[f as usize] & codecs
                    } else {
                        vk::VideoCodecOperationFlagsKHR::empty()
                    },
                    query_result_status: decode && result_status[f as usize],
                    queue: unsafe { device.get_device_queue(f, 0) },
                }
            })
            .collect();
        // Read out of the enabled set itself, so the two cannot disagree.
        // Recording it is what lets `crate::arena` allocate memory a shader may
        // take the address of (docs/design.md §0.16 item 3).
        let bda = wanted.p_next.iter().any(|l| {
            matches!(l, sv_proto::PNextLink::VkPhysicalDeviceVulkan12Features(f) if f.buffer_device_address)
        });

        let external_fd =
            has_fd.then(|| ash::khr::external_memory_fd::Device::new(instance, &device));
        let external_host =
            has_host.then(|| ash::ext::external_memory_host::Device::new(instance, &device));
        Ok(Opened {
            pd,
            device,
            queue,
            queue_family,
            served,
            decode_family,
            external_fd,
            has_dma_buf,
            external_host,
            buffer_device_address: bda,
            features: wanted,
            enabled_extensions: enabled_names.iter().map(|s| s.to_string()).collect(),
            withheld_extensions: withheld,
        })
    }

    /// The driver's own name, as `sv_arena::import::path_for_driver` spells it.
    ///
    /// Matched on the device name because `VkPhysicalDeviceDriverProperties` is a
    /// 1.2 query and this instance is 1.1. The names are the ones that module's
    /// table already knows.
    pub fn driver_key(&self) -> &'static str {
        let n = self.name.to_ascii_lowercase();
        if n.contains("llvmpipe") || n.contains("lavapipe") {
            "lavapipe"
        } else if n.contains("radv") {
            "radv"
        } else if n.contains("nvidia") {
            "nvidia"
        } else if n.contains("intel") {
            "intel"
        } else {
            "unknown"
        }
    }

    /// Resolve every entry point of the profile from this driver.
    ///
    /// Instance-level names come from `vkGetInstanceProcAddr` and device-level
    /// ones from `vkGetDeviceProcAddr`. The distinction is not cosmetic: the
    /// loader's instance table answers device commands with a *trampoline* that
    /// dispatches on the handle, and we are calling a driver directly with a
    /// handle the loader has never seen.
    pub fn load_fns(&self) -> sv_gpu_entry::DriverFns {
        let inst_fp = self.entry.static_fn().get_instance_proc_addr;
        let dev_fp = self.instance.fp_v1_0().get_device_proc_addr;
        let inst_handle = self.instance.handle();
        let dev_handle = self.device.handle();
        let by_instance = |name: &str| -> Option<unsafe extern "system" fn()> {
            let c = std::ffi::CString::new(name).ok()?;
            unsafe { inst_fp(inst_handle, c.as_ptr()) }
        };
        // A KHR name whose driver has only the EXT original it was promoted
        // from: the same command, the same signature. lavapipe has
        // `VK_EXT_calibrated_timestamps` only, the application side advertises
        // the KHR one, and Zink then calls `vkGetCalibratedTimestampsKHR`. The
        // device lookup must try the alias before the instance one: the
        // loader's `vkGetInstanceProcAddr` hands out a trampoline for any
        // device command it knows, which then jumps through the driver's null
        // (waycompose-design.md §104).
        const PROMOTED: [(&str, &str); 2] = [
            ("vkGetCalibratedTimestampsKHR", "vkGetCalibratedTimestampsEXT"),
            ("vkGetPhysicalDeviceCalibrateableTimeDomainsKHR", "vkGetPhysicalDeviceCalibrateableTimeDomainsEXT"),
        ];
        let alias = |name: &str| PROMOTED.iter().find(|(k, _)| *k == name).map(|(_, e)| *e);
        let dev_only = |name: &str| -> Option<unsafe extern "system" fn()> {
            let c = std::ffi::CString::new(name).ok()?;
            unsafe { dev_fp(dev_handle, c.as_ptr()) }
        };
        let by_device = |name: &str| -> Option<unsafe extern "system" fn()> {
            // A device-level name the driver does not have comes back null from
            // `vkGetDeviceProcAddr`, and so does one whose extension was not
            // enabled — which is the same thing from our point of view, since
            // `open_inner` enables everything the driver offers.
            dev_only(name).or_else(|| alias(name).and_then(dev_only)).or_else(|| by_instance(name))
        };
        // The physical-device query of the pair: the loader has a trampoline
        // for both names, so ask for the EXT original first.
        let by_instance = |name: &str| match alias(name) {
            Some(ext) => by_instance(ext).or_else(|| by_instance(name)),
            None => by_instance(name),
        };
        sv_gpu_entry::DriverFns::load(&by_instance, &by_device)
    }

    /// The first memory type satisfying `want`, or `None`.
    pub fn memory_type(&self, mask: u32, want: vk::MemoryPropertyFlags) -> Option<u32> {
        (0..self.mem_props.memory_type_count).find(|i| {
            mask & (1 << i) != 0
                && self.mem_props.memory_types[*i as usize].property_flags.contains(want)
        })
    }
}

/// The device extensions this GPU side may advertise: the profile's, restricted
/// to what the driver actually has, with the driver's own `specVersion`.
///
/// Both halves of the intersection are load-bearing and for different reasons.
/// The profile half is docs/design.md §9.2's rule — "an extension in this list is a
/// promise that its entry points work", and an entry point works only if pass 12
/// emitted a forwarder. The driver half is obvious and is the one a hand-written
/// list would have got right anyway.
pub fn forwardable_device_extensions(d: &Driver) -> Vec<(String, u32)> {
    let exts = match unsafe { d.instance.enumerate_device_extension_properties(d.pd) } {
        Ok(e) => e,
        Err(_) => return Vec::new(),
    };
    let mut out = Vec::new();
    for want in sv_gpu_entry::PROFILE_DEVICE_EXTENSIONS {
        if d.withheld_extensions.iter().any(|w| w == want) {
            continue;
        }
        for e in &exts {
            let name = unsafe { CStr::from_ptr(e.extension_name.as_ptr()) };
            if name.to_bytes() == want.as_bytes() {
                out.push(((*want).to_string(), e.spec_version));
                break;
            }
        }
    }
    out
}

/// Held while a driver is opened or closed, process-wide.
///
/// The Vulkan loader's ICD scan is not safe against another thread of the same
/// process loading or unloading the loader at the same moment: two tests that
/// each opened a driver segfaulted in `loader_scanned_icd_add`, under
/// `vkEnumerateInstanceExtensionProperties`, on every parallel run. The guest's
/// GPU process is the same shape in production: each ivshmem session opens its
/// own driver on its own thread (`entry::win_guest_ivshmem`), and `glxtest`'s
/// session closes while the browser's opens. Opening serially costs a session
/// at most the other's open, 93–224 ms on the guest (waycompose-design.md §87.3).
static OPEN_CLOSE: std::sync::Mutex<()> = std::sync::Mutex::new(());

impl Drop for Driver {
    fn drop(&mut self) {
        let _one = OPEN_CLOSE.lock().unwrap_or_else(|p| p.into_inner());
        unsafe {
            let _ = self.device.device_wait_idle();
            self.device.destroy_device(None);
            self.instance.destroy_instance(None);
            // The loader is unloaded here, under the lock, not after it.
            std::mem::ManuallyDrop::drop(&mut self.entry);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const H264: &str = "VK_KHR_video_decode_h264";
    const H265: &str = "VK_KHR_video_decode_h265";
    const AV1: &str = "VK_KHR_video_decode_av1";
    const VP9: &str = "VK_KHR_video_decode_vp9";

    use vk::PhysicalDeviceType as T;
    const LVP: (T, i32) = (T::CPU, 13);
    const MVK: (T, i32) = (T::INTEGRATED_GPU, DRIVER_ID_MOLTENVK);
    const KK: (T, i32) = (T::INTEGRATED_GPU, DRIVER_ID_MESA_KOSMICKRISP);
    const RADV: (T, i32) = (T::INTEGRATED_GPU, 3);
    const NV: (T, i32) = (T::DISCRETE_GPU, 4);

    /// §103.6's order: KosmicKrisp, MoltenVK, other GPUs, the CPU last.
    #[test]
    fn a_real_gpu_driver_is_preferred_and_lavapipe_is_the_fallback() {
        assert_eq!(preferred_device(&[LVP, MVK], false), Some(1));
        assert_eq!(preferred_device(&[LVP, MVK, KK], false), Some(2));
        assert_eq!(preferred_device(&[MVK, KK], false), Some(1));
        assert_eq!(preferred_device(&[LVP, RADV, NV], false), Some(2));
        assert_eq!(preferred_device(&[LVP], false), Some(0));
        assert_eq!(preferred_device(&[], false), None);
        // Ties keep the loader's order.
        assert_eq!(preferred_device(&[RADV, RADV], false), Some(0));
    }

    #[test]
    fn sv_gpu_prefer_cpu_puts_lavapipe_first() {
        assert_eq!(preferred_device(&[MVK, KK, LVP], true), Some(2));
        assert_eq!(preferred_device(&[MVK, KK], true), Some(1));
    }

    fn names(w: &[(String, String)]) -> Vec<&str> {
        w.iter().map(|(n, _)| n.as_str()).collect()
    }

    /// The guest's Intel 101.7076, as docs/video-decode.md §1 and §7.11.2
    /// measured it: H.264 COINCIDE with 16 slots, H.265 15 slots, AV1 8 slots,
    /// no VP9. AV1 goes for its slots; H.264 and H.265 are served. 101.7092,
    /// the newest driver for this GPU, answers every one the same (§7.13).
    #[test]
    fn intel_101_7076_and_7092_are_served_h264_and_h265() {
        let answers = [
            (H264, CodecAnswer::Caps { decode_flags: 0x1, max_dpb_slots: 16 }),
            (H265, CodecAnswer::Caps { decode_flags: 0x1, max_dpb_slots: 15 }),
            (AV1, CodecAnswer::Caps { decode_flags: 0x1, max_dpb_slots: 8 }),
        ];
        let w = withheld_decode_codecs(&answers, true, false);
        assert_eq!(names(&w), [AV1]);
        assert!(w[0].1.contains("8 DPB slots"), "{}", w[0].1);
    }

    /// §1's survey read H.265's decode flags as 0x1100: neither output mode,
    /// which the specification forbids and ffmpeg refuses. If the driver
    /// answers that, H.265 is withheld rather than offered and then refused
    /// mid-init.
    #[test]
    fn decode_flags_naming_no_output_mode_withhold_the_codec() {
        let answers = [
            (H264, CodecAnswer::Caps { decode_flags: 0x1, max_dpb_slots: 16 }),
            (H265, CodecAnswer::Caps { decode_flags: 0x1100, max_dpb_slots: 15 }),
        ];
        assert_eq!(names(&withheld_decode_codecs(&answers, true, false)), [H265]);
    }

    /// RADV and NVIDIA: every codec answers well, nothing is withheld; on a
    /// driver that needs stable structures VP9 is, as nothing keeps its.
    #[test]
    fn a_good_driver_keeps_every_codec_and_vp9_needs_stable_structures() {
        let answers = [
            (H264, CodecAnswer::Caps { decode_flags: 0x2, max_dpb_slots: 17 }),
            (H265, CodecAnswer::Caps { decode_flags: 0x2, max_dpb_slots: 17 }),
            (AV1, CodecAnswer::Caps { decode_flags: 0x2, max_dpb_slots: 9 }),
            (VP9, CodecAnswer::NotAsked),
        ];
        assert!(withheld_decode_codecs(&answers, false, false).is_empty());
        assert_eq!(names(&withheld_decode_codecs(&answers, true, false)), [VP9]);
        assert_eq!(names(&withheld_decode_codecs(&answers, false, true)), [H265, AV1, VP9]);
    }

    /// A refused basic profile withholds the codec.
    #[test]
    fn a_refused_profile_withholds_the_codec() {
        let answers = [(AV1, CodecAnswer::Refused(-1_000_023_004))];
        let w = withheld_decode_codecs(&answers, false, false);
        assert_eq!(names(&w), [AV1]);
        assert!(w[0].1.contains("-1000023004"));
    }
    use vk::QueueFlags as Q;
    use vk::VideoCodecOperationFlagsKHR as Op;

    const G: Q = Q::from_raw(Q::GRAPHICS.as_raw() | Q::COMPUTE.as_raw() | Q::TRANSFER.as_raw());

    /// RADV's family table as docs/video-decode.md §1 measured it.
    fn radv() -> (Vec<Q>, Vec<Op>) {
        let none = Op::empty();
        (
            vec![G, Q::COMPUTE | Q::TRANSFER, Q::VIDEO_DECODE_KHR, Q::VIDEO_ENCODE_KHR, Q::SPARSE_BINDING],
            vec![none, none, Op::DECODE_H264 | Op::DECODE_H265 | Op::DECODE_AV1, Op::ENCODE_H264, none],
        )
    }

    #[test]
    fn without_decode_the_one_compute_family_is_served_exactly_as_before() {
        let (f, o) = radv();
        assert_eq!(choose_families(&f, &o, false), Some((vec![0], None)));
    }

    #[test]
    fn with_decode_the_served_families_are_the_drivers_own_prefix() {
        // RADV: decode is family 2, so 0, 1 and 2 are served at their own
        // indices and the encode family after it is not.
        let (f, o) = radv();
        assert_eq!(choose_families(&f, &o, true), Some((vec![0, 1, 2], Some(2))));
        // NVIDIA: decode is family 3, after transfer and compute families.
        let n = vec![G, Q::TRANSFER, Q::COMPUTE | Q::TRANSFER, Q::TRANSFER | Q::VIDEO_DECODE_KHR];
        let no = vec![Op::empty(), Op::empty(), Op::empty(), Op::DECODE_H264];
        assert_eq!(choose_families(&n, &no, true), Some((vec![0, 1, 2, 3], Some(3))));
        // The guest Intel: decode is family 1 and the prefix is the whole table.
        let i = vec![G, Q::VIDEO_DECODE_KHR];
        let io = vec![Op::empty(), Op::DECODE_H264];
        assert_eq!(choose_families(&i, &io, true), Some((vec![0, 1], Some(1))));
    }

    #[test]
    fn a_decode_family_that_names_no_codec_is_not_a_decode_family() {
        let f = vec![G, Q::VIDEO_DECODE_KHR];
        let o = vec![Op::empty(), Op::empty()];
        assert_eq!(choose_families(&f, &o, true), Some((vec![0], None)));
    }

    #[test]
    fn no_compute_family_is_no_device() {
        assert_eq!(choose_families(&[Q::VIDEO_DECODE_KHR], &[Op::DECODE_H264], true), None);
    }

    #[test]
    fn encode_and_protected_are_not_served_capabilities() {
        assert!(!SERVED_QUEUE_FLAGS.contains(Q::VIDEO_ENCODE_KHR));
        assert!(!SERVED_QUEUE_FLAGS.contains(Q::PROTECTED));
        assert!(SERVED_QUEUE_FLAGS.contains(Q::VIDEO_DECODE_KHR | G));
    }
}

#[cfg(target_os = "macos")]
impl Driver {
    pub(crate) fn native_submission_queue(&self) -> std::result::Result<std::sync::Arc<crate::video_worker::Queue>, vk::Result> {
        let mut queue = self.native_submissions.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(queue) = &*queue { return Ok(std::sync::Arc::clone(queue)); }
        let created = std::sync::Arc::new(crate::video_worker::Queue::new()
            .map_err(|_| vk::Result::ERROR_OUT_OF_HOST_MEMORY)?);
        *queue = Some(std::sync::Arc::clone(&created));
        Ok(created)
    }
}
