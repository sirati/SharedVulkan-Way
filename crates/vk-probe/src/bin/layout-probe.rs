//! Which `vkCreatePipelineLayout` a driver dies on (waycompose-design.md §86).
//!
//! **DO NOT RUN THIS ON testhost's GUEST.** Its one run there, 2026-10-15
//! 17:06–17:07, faulted the Intel user-mode driver three times in a few
//! seconds (`zink`, `push-empty`, `push-texel`: 0xc0000005), and at 17:07:16
//! QEMU stopped the VM with `kvm run failed Bad address` (status
//! `internal-error`), the vCPU in a guest-kernel register-polling loop. The
//! iGPU is passed through with VFIO; a GPU reset after the faults revoking the
//! BAR mapping under that read is the likely mechanism, and it is not proven.
//! What it measured before that is in §86: the fault needs a PUSH_DESCRIPTOR
//! set layout carrying a `VkDescriptorSetLayoutBindingFlagsCreateInfo` in a
//! pipeline layout with another set after it.
//!
//! On the real guest, Intel's Windows driver (`igvk64.dll`, 101.7076) faulted
//! reading address 0x8 inside `vkCreatePipelineLayout` for the first layout
//! Zink made with descriptor sets in it, under Firefox through the forwarder:
//!
//! ```text
//! set 0: PUSH_DESCRIPTOR, 5 x UNIFORM_BUFFER (one per graphics stage), binding flags all 0
//! set 1: empty (bindingCount 0) — Zink's dummy layout, binding-flags pNext with count 0
//! set 2: 1 x UNIFORM_TEXEL_BUFFER, fragment, binding flags 0
//! push constants: ALL_GRAPHICS, 0..52
//! ```
//!
//! This makes that layout, and variations of it, directly on the driver with
//! no forwarder in between, one variation per child process so that a fault in
//! one is a result and not the end of the run.
//!
//!   layout-probe            run every variation, one child each, and report
//!   layout-probe <name>     run one variation in this process
//!
//! Console only, no window and no GPU work beyond object creation.

use ash::vk;
use std::ffi::CStr;

struct Dev {
    _entry: ash::Entry,
    instance: ash::Instance,
    device: ash::Device,
}

fn open() -> Result<Dev, String> {
    let entry = unsafe { ash::Entry::load() }.map_err(|e| format!("no loader: {e}"))?;
    let app = vk::ApplicationInfo::default().api_version(vk::API_VERSION_1_3);
    let instance = unsafe { entry.create_instance(&vk::InstanceCreateInfo::default().application_info(&app), None) }
        .map_err(|e| format!("vkCreateInstance: {e}"))?;
    let pd = *unsafe { instance.enumerate_physical_devices() }
        .map_err(|e| format!("enumerate: {e}"))?
        .first()
        .ok_or("no device")?;
    let props = unsafe { instance.get_physical_device_properties(pd) };
    eprintln!(
        "device: {} driver 0x{:x}",
        unsafe { CStr::from_ptr(props.device_name.as_ptr()) }.to_string_lossy(),
        props.driver_version
    );
    let prio = [1.0f32];
    let q = [vk::DeviceQueueCreateInfo::default().queue_family_index(0).queue_priorities(&prio)];
    let exts = [c"VK_KHR_push_descriptor".as_ptr()];
    let mut di = vk::PhysicalDeviceDescriptorIndexingFeatures::default();
    let mut f2 = vk::PhysicalDeviceFeatures2::default().push_next(&mut di);
    unsafe { instance.get_physical_device_features2(pd, &mut f2) };
    let mut di_on = vk::PhysicalDeviceDescriptorIndexingFeatures::default()
        .descriptor_binding_partially_bound(di.descriptor_binding_partially_bound != 0)
        .descriptor_binding_uniform_texel_buffer_update_after_bind(di.descriptor_binding_uniform_texel_buffer_update_after_bind != 0);
    let ci = vk::DeviceCreateInfo::default()
        .queue_create_infos(&q)
        .enabled_extension_names(&exts)
        .push_next(&mut di_on);
    let device = unsafe { instance.create_device(pd, &ci, None) }.map_err(|e| format!("vkCreateDevice: {e}"))?;
    Ok(Dev { _entry: entry, instance, device })
}

/// One descriptor set layout, optionally with a binding-flags pNext of the
/// same count (all zero), as Zink makes every one of them.
fn dsl(
    d: &ash::Device,
    flags: vk::DescriptorSetLayoutCreateFlags,
    bindings: &[vk::DescriptorSetLayoutBinding],
    with_flags_pnext: bool,
) -> vk::DescriptorSetLayout {
    let zeros = vec![vk::DescriptorBindingFlags::empty(); bindings.len()];
    let mut bf = vk::DescriptorSetLayoutBindingFlagsCreateInfo::default().binding_flags(&zeros);
    let mut ci = vk::DescriptorSetLayoutCreateInfo::default().flags(flags).bindings(bindings);
    if with_flags_pnext {
        ci = ci.push_next(&mut bf);
    }
    unsafe { d.create_descriptor_set_layout(&ci, None) }.expect("vkCreateDescriptorSetLayout")
}

fn ubo(binding: u32, stage: vk::ShaderStageFlags) -> vk::DescriptorSetLayoutBinding<'static> {
    vk::DescriptorSetLayoutBinding::default()
        .binding(binding)
        .descriptor_type(vk::DescriptorType::UNIFORM_BUFFER)
        .descriptor_count(1)
        .stage_flags(stage)
}

const VARIATIONS: &[&str] = &[
    "zink",             // the layout that faulted, exactly
    "zink-no-pnext",    // the same without the binding-flags pNext
    "push-empty",       // sets 0 and 1 only
    "push-texel",       // the push set and the texel set, no empty one
    "empty-texel",      // the empty set and the texel set
    "plain-empty-texel",// set 0 an ordinary (not push) UBO layout
    "push-only",        // the push set alone
    "empty-only",       // the empty set alone
];

fn run(name: &str) -> Result<(), String> {
    let dev = open()?;
    let d = &dev.device;
    let g = vk::ShaderStageFlags::VERTEX;
    let stages = [
        vk::ShaderStageFlags::VERTEX,
        vk::ShaderStageFlags::TESSELLATION_CONTROL,
        vk::ShaderStageFlags::TESSELLATION_EVALUATION,
        vk::ShaderStageFlags::GEOMETRY,
        vk::ShaderStageFlags::FRAGMENT,
    ];
    let push_bindings: Vec<_> = stages.iter().enumerate().map(|(i, s)| ubo(i as u32, *s)).collect();
    let texel = [vk::DescriptorSetLayoutBinding::default()
        .binding(0)
        .descriptor_type(vk::DescriptorType::UNIFORM_TEXEL_BUFFER)
        .descriptor_count(1)
        .stage_flags(vk::ShaderStageFlags::FRAGMENT)];
    let pn = name != "zink-no-pnext";
    let push = || dsl(d, vk::DescriptorSetLayoutCreateFlags::PUSH_DESCRIPTOR_KHR, &push_bindings, pn);
    let plain = || dsl(d, vk::DescriptorSetLayoutCreateFlags::empty(), &[ubo(0, g)], pn);
    let empty = || dsl(d, vk::DescriptorSetLayoutCreateFlags::empty(), &[], pn);
    let tex = || dsl(d, vk::DescriptorSetLayoutCreateFlags::empty(), &texel, pn);
    let sets: Vec<vk::DescriptorSetLayout> = match name {
        "zink" | "zink-no-pnext" => vec![push(), empty(), tex()],
        "push-empty" => vec![push(), empty()],
        "push-texel" => vec![push(), tex()],
        "empty-texel" => vec![empty(), tex()],
        "plain-empty-texel" => vec![plain(), empty(), tex()],
        "push-only" => vec![push()],
        "empty-only" => vec![empty()],
        _ => return Err(format!("no variation {name}")),
    };
    let pcr = [vk::PushConstantRange::default().stage_flags(vk::ShaderStageFlags::ALL_GRAPHICS).offset(0).size(52)];
    let ci = vk::PipelineLayoutCreateInfo::default().set_layouts(&sets).push_constant_ranges(&pcr);
    let layout = unsafe { d.create_pipeline_layout(&ci, None) }.map_err(|e| format!("vkCreatePipelineLayout: {e}"))?;
    unsafe {
        d.destroy_pipeline_layout(layout, None);
        for s in sets {
            d.destroy_descriptor_set_layout(s, None);
        }
        d.destroy_device(None);
        dev.instance.destroy_instance(None);
    }
    Ok(())
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if let Some(name) = args.get(1) {
        match run(name) {
            Ok(()) => println!("{name}: ok"),
            Err(e) => {
                println!("{name}: error: {e}");
                std::process::exit(2);
            }
        }
        return;
    }
    let me = std::env::current_exe().expect("current_exe");
    for v in VARIATIONS {
        let out = std::process::Command::new(&me).arg(v).output().expect("spawn");
        let said = String::from_utf8_lossy(&out.stdout).trim().to_string();
        match out.status.code() {
            Some(0) => println!("{v:20} ok"),
            Some(c) => println!("{v:20} exit 0x{:08x} {said}", c as u32),
            None => println!("{v:20} killed"),
        }
    }
}
